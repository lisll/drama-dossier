"""C7 音频处理：EQ、响度、动态压缩与音频质量检查。

同样走「先写计划、后统一物化」的模式：这里只往 `ctx.plan["audio_filters"]` 里追加滤镜，
真正的编码由 `media.encode` 完成。响度检查用 `loudnorm` 的 JSON 输出读回，
结果记进 `ctx.plan["audio_measurement"]` 供质量门禁参考。
"""

from __future__ import annotations

import json
from typing import List, Literal, Optional

from pydantic import Field

from ..base import StrictModel
from ..errors import WorkError, ErrorClass
from ..recipes.registry import ModuleSpec, register
from .ffmpeg import MediaContext, run


class EqBand(StrictModel):
    frequency: float = Field(gt=20.0, le=20000.0)
    gain: float = Field(ge=-24.0, le=24.0)
    #: 倍频程带宽，越小越窄。
    width_octaves: float = Field(1.0, gt=0.01, le=8.0)


class LoudnessParams(StrictModel):
    #: 目标整体响度（LUFS）。
    target_lufs: float = Field(-16.0, ge=-40.0, le=-5.0)
    #: 真峰值上限（dBTP）。
    true_peak: float = Field(-1.5, ge=-9.0, le=0.0)
    #: 响度范围。
    range_lu: float = Field(11.0, ge=1.0, le=30.0)


class CompressorParams(StrictModel):
    threshold: float = Field(-20.0, ge=-60.0, le=0.0)
    ratio: float = Field(2.0, ge=1.0, le=20.0)
    attack: float = Field(20.0, ge=0.01, le=2000.0)
    release: float = Field(250.0, ge=0.01, le=9000.0)


class AudioQualityParams(StrictModel):
    loudness: Optional[LoudnessParams] = None
    compressor: Optional[CompressorParams] = None
    eq: List[EqBand] = Field(default_factory=list)
    #: 动态范围归一化，适合解说/旁白音量起伏大的素材。
    dynaudnorm: bool = False
    #: 测量输入响度（只读检查，不改变音频）。
    measure: bool = True
    channels: Optional[Literal[1, 2]] = None


def _filters(ctx: MediaContext) -> List[str]:
    return ctx.plan_section("audio_filters", [])


def measure_loudness(ctx: MediaContext) -> Optional[dict]:
    """用 loudnorm 的 JSON 输出读回输入响度。"""
    result = run(
        [
            ctx.ffmpeg,
            "-hide_banner",
            "-nostdin",
            "-i",
            str(ctx.primary),
            "-af",
            "loudnorm=print_format=json",
            "-f",
            "null",
            "-",
        ],
        timeout=900.0,
    )
    stderr = result.stderr.decode("utf-8", "replace")
    start = stderr.rfind("{")
    end = stderr.rfind("}")
    if result.returncode != 0 or start < 0 or end <= start:
        # 测量失败不应让整个任务失败：它只是诊断信息。
        ctx.notes.append("响度测量失败，跳过")
        return None
    try:
        payload = json.loads(stderr[start : end + 1])
    except json.JSONDecodeError:
        ctx.notes.append("响度测量输出无法解析，跳过")
        return None
    keys = ("input_i", "input_tp", "input_lra", "input_thresh")
    measurement = {k: payload.get(k) for k in keys}
    try:
        measurement["input_i_db"] = float(payload["input_i"])
    except (KeyError, TypeError, ValueError):
        measurement["input_i_db"] = None
    return measurement


def implement_audio_quality(ctx: MediaContext, params: AudioQualityParams) -> dict:
    if not ctx.probe.get("has_audio"):
        # 没有音轨时静默跳过会让配方「看起来生效了」，所以显式记一条说明。
        ctx.notes.append("输入没有音轨，跳过 C7 音频处理")
        return {"skipped": True}

    if params.measure:
        measurement = measure_loudness(ctx)
        if measurement:
            ctx.plan_section("audio_measurement", {}).update(measurement)
            ctx.notes.append(f"输入响度：{measurement.get('input_i')} LUFS")

    chain = _filters(ctx)
    for band in params.eq:
        chain.append(
            f"equalizer=f={band.frequency:g}:width_type=o:width={band.width_octaves:g}"
            f":g={band.gain:g}"
        )
    if params.compressor is not None:
        comp = params.compressor
        chain.append(
            f"acompressor=threshold={comp.threshold}dB:ratio={comp.ratio}"
            f":attack={comp.attack}:release={comp.release}"
        )
    if params.dynaudnorm:
        chain.append("dynaudnorm=f=250:g=15:p=0.8")
    if params.loudness is not None:
        loud = params.loudness
        # 双通道 EBU R128 归一化：第一遍测量在 measure_loudness，这里做目标归一化。
        chain.append(
            f"loudnorm=I={loud.target_lufs}:TP={loud.true_peak}:LRA={loud.range_lu}"
        )
    if params.channels:
        chain.append(f"aformat=channel_layouts={'mono' if params.channels == 1 else 'stereo'}")
    if chain:
        ctx.notes.append("已加入音频处理：" + ", ".join(chain))
    return {"filters": chain}


def require_audio(ctx: MediaContext) -> None:
    if not ctx.probe.get("has_audio"):
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "该模块要求输入包含音轨")


register(
    ModuleSpec(
        name="audio.quality",
        title="音频质量处理（C7）",
        capability="audio_filters",
        requires=frozenset({"audio"}),
        produces=frozenset({"audio_filters"}),
        params_model=AudioQualityParams,
        implement=implement_audio_quality,
    )
)
