"""V5 式去重执行模块（技术验证，对照 Grove Max 逆向文档 §6）。

把 :mod:`drama_cloud.recipes.dedup` 签发的配方编译成 ffmpeg 滤镜链，按既有
「计划型模块」约定写入 ``ctx.plan``，由 ``media.encode`` 统一物化。三个模块：

- ``video.dedup``：画面去重链（归一化 → tmix 时域混帧 → 缩放 → 波形旋转 →
  比例裁切 → 回归画幅），对应 Grove first-encode 母版轮；
- ``video.dedup_finish``：精修链（波形 eq/hue → 噪点 → 锐化），对应 Grove
  final-encode 成片轮；
- ``audio.dedup``：音频去重链（asetrate 音调微移 + atempo 时长精确补偿 →
  4 段 EQ → 压缩器 → 变速），对应 Grove nativeV5Recipe.audio；
- ``metadata.device_pool``：把配方里的 iPhone 机型伪装 tags 并进
  ``metadata_policy.set``，复用现有 ``media.encode`` 元数据挂点（C8）。

滤镜编译是纯函数（不碰 MediaContext），可脱离 ffmpeg 单测；波形表达式与
参数格式化精度（12 位）照抄 Grove ``reference_profile.py`` 的
``compile_reference``，保证两端编译结果可直接对照。
"""
from __future__ import annotations

import math
from typing import Annotated, Dict, List, Optional

from pydantic import BeforeValidator, Field

from ..base import StrictModel
from ..errors import bad_param
from ..recipes.registry import ModuleSpec, register
from .ffmpeg import MediaContext
from .ffmpeg_caps import ffmpeg_available, requires_filters

WAVE_CLOCK_FRAME = "n"  # 几何类波形随帧号
WAVE_CLOCK_TIME = "t"  # 色彩类波形随秒


def _as_dict(value):
    """pydantic 模型转 dict（嵌套递归）；dict 原样返回。"""
    return value.model_dump() if hasattr(value, "model_dump") else value


def _wave_values(wave) -> List[float]:
    """波形统一转 4 元 list（接受 list / 具名 dict / pydantic 模型）。"""
    wave = _as_dict(wave)
    if isinstance(wave, dict):
        return [wave["base"], wave["amp"], wave["period"], wave["phase"]]
    return list(wave)


def format_wave(wave, clock: str) -> str:
    """把 [base, amp, period, phase] 编译成 ffmpeg 表达式（12 位精度，照抄 Grove）。"""
    values = _wave_values(wave)
    if len(values) != 4:
        raise bad_param(f"波形必须是 4 元组 [base, amp, period, phase]：{values}")
    base, amp, period, phase = (f"{float(v):.12f}" for v in values)
    if float(period) <= 1e-12:
        raise bad_param(f"波形 period 必须 > 0：{values}")
    return f"{base}+{amp}*sin(2*PI*{clock}/{period}+{phase})"


def _safe_crop_ratio(width: int, height: int, scale: float, ratio: float, rotation_wave) -> float:
    """旋转后不露黑边的安全裁切比例（照抄 Grove ``_geometry`` 的投影计算）。"""
    rotation_wave = _wave_values(rotation_wave)
    scaled_w = max(2, math.floor(width * scale / 2) * 2)
    scaled_h = max(2, math.floor(height * scale / 2) * 2)
    angle = abs(float(f"{rotation_wave[0]:.12f}")) + abs(float(f"{rotation_wave[1]:.12f}"))
    if angle <= 0:
        return ratio
    bound = min(angle, math.pi / 2)

    def projection(other_ratio: float) -> float:
        return math.cos(min(bound, math.atan(other_ratio))) + other_ratio * math.sin(
            min(bound, math.atan(other_ratio))
        )

    safe = min(
        (scaled_w - 2) / (scaled_w * projection(scaled_h / scaled_w)),
        (scaled_h - 2) / (scaled_h * projection(scaled_w / scaled_h)),
    )
    rendered_safe = float(f"{safe:.12f}")
    if rendered_safe > safe:
        rendered_safe -= 1e-12
    return min(ratio, rendered_safe)


def compile_dedup_video(params: "VideoDedupParams") -> List[str]:
    """母版轮画面链（Grove compile_reference 的 videoFilters 段）。"""
    width, height = params.width, params.height
    scale = params.scaleFactor
    variant = _as_dict(params.variant)
    ratio = _safe_crop_ratio(width, height, scale, variant["cropRatio"], variant["rotationWave"])
    scaled_w = max(2, math.floor(width * scale / 2) * 2)
    scaled_h = max(2, math.floor(height * scale / 2) * 2)
    cover = f"scale={width}:{height}:force_original_aspect_ratio=increase:flags=lanczos"
    weights = " ".join(f"{float(w):.12f}" for w in params.temporalWeights)
    chain = [
        f"format={params.pixelFormat}",
        cover,
        f"crop={width}:{height}",
        "setsar=1",
        f"tmix=frames=3:weights='{weights}'",
        f"scale={scaled_w}:{scaled_h}:flags=lanczos",
        f"rotate=a='{format_wave(variant['rotationWave'], WAVE_CLOCK_FRAME)}':fillcolor=black:ow=iw:oh=ih",
        f"crop=trunc(iw*{ratio:.12f}/2)*2:trunc(ih*{ratio:.12f}/2)*2:exact=1",
        cover,
        f"crop={width}:{height}",
        "setsar=1",
    ]
    return chain


def compile_dedup_finish(variant) -> List[str]:
    """成片轮精修链（Grove compile_reference 的 finishingFilters 段）。"""
    variant = _as_dict(variant)
    return [
        f"eq=brightness='{format_wave(variant['brightnessWave'], WAVE_CLOCK_TIME)}'"
        f":contrast='{format_wave(variant['contrastWave'], WAVE_CLOCK_TIME)}'"
        f":saturation='{format_wave(variant['saturationWave'], WAVE_CLOCK_TIME)}':eval=frame",
        f"hue=h='{format_wave(variant['hueWave'], WAVE_CLOCK_TIME)}'"
        f":s='{format_wave(variant['hueSaturationWave'], WAVE_CLOCK_TIME)}'",
        f"noise=alls={int(variant['noiseStrength'])}:allf=t",
        f"unsharp=5:5:{float(variant['sharpenAmount']):.12f}:5:5:0",
    ]


def compile_dedup_audio(audio, variant, backend: str = "atempo") -> List[str]:
    """音频去重链（Grove compile_reference 的 audioFilters 段，v5-video-clock/1 时序）。

    语义：asetrate 把采样率微移（音调变化），再 atempo 补偿回原时长
    （correction = rate/shifted），最后按 speed 整体变速。总变速恰为 speed。

    ``backend``（对照 Grove 逆向文档 §12.3「rubberband 优先、atempo 链兜底」）：
    - ``atempo``（默认）：asetrate+atempo 补偿链，任何 ffmpeg 都能跑；
    - ``rubberband``：``rubberband=tempo=speed:pitch=ratio`` 单滤镜完成音调微扰
      与总变速（librubberband 时域拉伸音质更好）。两个后端净语义严格一致
      （音调 ×ratio、时长 ×1/speed），rubberband 的 tempo 已折入总变速 speed，
      因此链尾不再追加 atempo=speed。调用方负责探测 ffmpeg 是否带该滤镜
      （auto 回落逻辑在 CLI 层，本函数保持纯函数）。
    """
    audio = _as_dict(audio)
    variant = _as_dict(variant)
    rate = int(audio["sampleRateHz"])
    ratio = float(f"{variant['asetrateRatio']:.12f}")
    if backend not in ("atempo", "rubberband"):
        raise bad_param(f"未知变调后端：{backend}（可选 atempo / rubberband）")
    chain = [
        f"aresample={rate}",
        "aformat=sample_fmts=fltp:channel_layouts=stereo",
    ]
    if backend == "rubberband":
        chain.append(f"rubberband=tempo={float(variant['speed']):.12f}:pitch={ratio:.12f}")
    else:
        shifted_rate = math.floor(rate * ratio + 0.5)
        compensation = float(variant["audioTempoCompensation"])
        correction = 1 / ((shifted_rate / rate) * compensation)
        effective = compensation * correction
        chain += [
            f"asetrate={shifted_rate}",
            f"aresample={rate}",
            f"atempo={effective:.12f}",
        ]
    chain.extend(
        f"equalizer=f={float(eq['frequencyHz']):.12f}:t=q:w={float(eq['widthQ']):.12f}"
        f":g={float(eq['gainDb']):.12f}"
        for eq in variant["equalizer"]
    )
    comp = variant["compressor"]
    chain.append(
        f"acompressor=threshold={float(comp['thresholdDb']):.12f}dB:ratio={float(comp['ratio']):.12f}"
        f":attack={float(comp['attackMs']):.12f}:release={float(comp['releaseMs']):.12f}"
        f":makeup={float(comp['makeup']):.12f}"
    )
    if backend == "atempo":
        chain.append(f"atempo={float(variant['speed']):.12f}")
    return chain


# ---------------------------------------------------------------------------
# 参数模型与模块注册
# ---------------------------------------------------------------------------


class _Wave(StrictModel):
    base: float
    amp: float
    period: float = Field(gt=0.0)
    phase: float


def _wave_from_list(value):
    """配方 JSON 里的波形是 4 元 list（[base, amp, period, phase]），转成模型 dict。"""
    if isinstance(value, (list, tuple)) and len(value) == 4:
        return dict(zip(("base", "amp", "period", "phase"), value))
    return value


#: 既能收 Grove 配方风格的 4 元 list，也能收具名 dict。
WaveLike = Annotated[_Wave, BeforeValidator(_wave_from_list)]


class _EqBand(StrictModel):
    frequencyHz: float = Field(ge=20.0, le=48000.0)
    widthQ: float = Field(ge=0.1, le=3000.0)
    gainDb: float = Field(ge=-12.0, le=12.0)


class _Compressor(StrictModel):
    thresholdDb: float = Field(ge=-60.0, le=0.0)
    ratio: float = Field(ge=1.0, le=20.0)
    attackMs: float = Field(ge=0.01, le=2000.0)
    releaseMs: float = Field(ge=0.01, le=9000.0)
    makeup: float = Field(ge=1.0, le=64.0)


class _Variant(StrictModel):
    speed: float = Field(ge=1.0, le=1.25)
    cropRatio: float = Field(ge=0.8, le=1.0)
    rotationWave: WaveLike
    contrastWave: WaveLike
    brightnessWave: WaveLike
    saturationWave: WaveLike
    hueWave: WaveLike
    hueSaturationWave: WaveLike
    noiseStrength: int = Field(ge=0, le=20)
    sharpenAmount: float = Field(ge=0.0, le=3.0)
    asetrateRatio: float = Field(ge=0.9, le=1.1)
    audioTempoCompensation: float = Field(ge=0.5, le=2.0)
    equalizer: List[_EqBand]
    compressor: _Compressor
    bFrames: int = Field(default=0, ge=0, le=16)
    referenceFrames: int = Field(default=4, ge=1, le=16)
    sceneCutThreshold: int = Field(default=40, ge=0, le=100)
    boxes: List[Dict] = Field(default_factory=list)


class VideoDedupParams(StrictModel):
    """``video.dedup`` / ``video.dedup_finish`` / ``audio.dedup`` 共用的配方片段。"""

    width: int = Field(ge=2, le=7680)
    height: int = Field(ge=2, le=7680)
    frameRate: int = Field(default=30, ge=1, le=240)
    scaleFactor: float = Field(default=1.0, ge=0.25, le=4.0)
    pixelFormat: str = "yuv420p"
    temporalWeights: List[float] = Field(default_factory=lambda: [25.0, 50.0, 25.0])
    variant: _Variant


def _require_video(ctx: MediaContext) -> None:
    if not ctx.probe.get("has_video", True):
        raise bad_param("去重模块要求输入包含视频轨")


def implement_video_dedup(ctx: MediaContext, params: VideoDedupParams) -> dict:
    _require_video(ctx)
    chain = compile_dedup_video(params)
    ctx.plan_section("video_filters", []).extend(chain)
    ctx.notes.append("已加入 V5 画面去重链（tmix 混帧/波形旋转/比例裁切）")
    return {"filters": len(chain)}


def implement_video_dedup_finish(ctx: MediaContext, params: VideoDedupParams) -> dict:
    _require_video(ctx)
    chain = compile_dedup_finish(params.variant.model_dump())
    ctx.plan_section("video_filters", []).extend(chain)
    ctx.notes.append("已加入 V5 精修链（波形调色/噪点/锐化）")
    return {"filters": len(chain)}


def implement_audio_dedup(ctx: MediaContext, params: VideoDedupParams) -> dict:
    if not ctx.probe.get("has_audio"):
        ctx.notes.append("输入没有音轨，跳过 V5 音频去重")
        return {"skipped": True}
    audio = {"sampleRateHz": 48000}
    chain = compile_dedup_audio(audio, params.variant.model_dump())
    ctx.plan_section("audio_filters", []).extend(chain)
    ctx.notes.append("已加入 V5 音频去重链（音调微移/EQ/压缩器/变速）")
    return {"filters": len(chain)}


class DevicePoolParams(StrictModel):
    """``metadata.device_pool``：iPhone 机型伪装 tags（Grove 8 机型池语义）。"""

    make: str = Field(min_length=1, max_length=64)
    comment: str = Field(min_length=1, max_length=73)
    copyright: str = Field(min_length=1, max_length=128)
    software: Optional[str] = Field(default=None, max_length=64)
    encoder_tag: Optional[str] = Field(default=None, max_length=128)
    creation_time: Optional[str] = Field(default=None, max_length=40)


def implement_device_pool(ctx: MediaContext, params: DevicePoolParams) -> dict:
    policy = ctx.plan.setdefault(
        "metadata_policy", {"mode": "keep", "set": {}, "remove": []}
    )
    tags = {"make": params.make, "comment": params.comment, "copyright": params.copyright}
    if params.software:
        tags["software"] = params.software
    if params.encoder_tag:
        tags["encoder"] = params.encoder_tag
    if params.creation_time:
        tags["creation_time"] = params.creation_time
    policy.setdefault("set", {}).update(tags)
    ctx.notes.append(f"已写入机型伪装元数据：make={params.make}")
    return {"tags": sorted(tags)}


register(
    ModuleSpec(
        name="video.dedup",
        title="V5 画面去重（时域混帧/波形旋转/比例裁切）",
        capability="ffmpeg",
        requires=frozenset({"video"}),
        produces=frozenset({"dedup_video_filters"}),
        params_model=VideoDedupParams,
        implement=implement_video_dedup,
        capability_probe=requires_filters("tmix", "rotate", "noise", "unsharp"),
    )
)
register(
    ModuleSpec(
        name="video.dedup_finish",
        title="V5 精修（波形调色/噪点/锐化）",
        capability="ffmpeg",
        requires=frozenset({"video"}),
        produces=frozenset({"dedup_finish_filters"}),
        params_model=VideoDedupParams,
        implement=implement_video_dedup_finish,
        capability_probe=requires_filters("eq", "hue", "noise", "unsharp"),
    )
)
register(
    ModuleSpec(
        name="audio.dedup",
        title="V5 音频去重（音调微移/EQ/压缩器/变速）",
        capability="ffmpeg",
        requires=frozenset({"video"}),
        produces=frozenset({"dedup_audio_filters"}),
        params_model=VideoDedupParams,
        implement=implement_audio_dedup,
        capability_probe=requires_filters("asetrate", "atempo", "equalizer", "acompressor"),
    )
)
register(
    ModuleSpec(
        name="metadata.device_pool",
        title="机型伪装元数据（iPhone 机型池）",
        capability="ffmpeg",
        requires=frozenset({"metadata"}),
        produces=frozenset({"metadata_policy"}),
        params_model=DevicePoolParams,
        implement=implement_device_pool,
        # 只写 -metadata，不需要特定滤镜，但没有 ffmpeg 的机器不能声明这项能力。
        capability_probe=ffmpeg_available,
    )
)
