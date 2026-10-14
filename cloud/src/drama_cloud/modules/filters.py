"""C4 画面处理 与 C9 帧级处理。

这两个功能在原文档里是「含糊的单一模块」，文档自己也要求 C9「必须先细化定义为帧率转换、
插帧、去抖或逐帧质量检查等具体能力，不作为含糊的单一模块」。这里就按具体能力拆开：

- `video.quality`：色彩校正、锐化、降噪（C4）
- `frame.fps_convert`：帧率转换（C9）
- `frame.deflicker`：去抖/去闪烁（C9）
- `frame.inspect`：逐帧质量抽检（C9）

**未实现**：插帧（光流/模型插帧）需要 GPU 与额外模型，本机不可行，因此不注册对应模块；
配方里写不存在的模块会被白名单直接拒绝，不会静默降级（见 docs/PROCESSING_PIPELINE.md）。
"""

from __future__ import annotations

from typing import List, Literal, Optional

from pydantic import Field, model_validator

from ..base import StrictModel
from ..errors import bad_param
from ..recipes.registry import ModuleSpec, register
from .ffmpeg import MediaContext, run_checked
from .ffmpeg_caps import requires_filters

DENOISE_PRESETS = {
    "off": None,
    "light": "hqdn3d=1.5:1.5:6:6",
    "medium": "hqdn3d=3:3:9:9",
    "strong": "hqdn3d=6:6:12:12",
}


class ColorParams(StrictModel):
    """`eq` 滤镜参数，取值范围与 ffmpeg 一致。"""

    brightness: float = Field(0.0, ge=-1.0, le=1.0)
    contrast: float = Field(0.0, ge=-1.0, le=1.0)
    saturation: float = Field(0.0, ge=-1.0, le=1.0)
    gamma: float = Field(0.0, ge=-1.0, le=1.0)


class ScaleParams(StrictModel):
    """分辨率归一化。宽高都按偶数对齐，避免 yuv420p 下出现奇数尺寸。"""

    width: Optional[int] = Field(None, ge=16, le=7680)
    height: Optional[int] = Field(None, ge=16, le=4320)
    #: fit = 完整画面缩放到框内（可能留边）；fill = 铺满后中心裁剪；pad = 缩放后补边。
    mode: Literal["fit", "fill", "pad"] = "fit"
    pad_color: str = "black"

    @model_validator(mode="after")
    def _at_least_one(self):
        if self.width is None and self.height is None:
            raise ValueError("scale 至少要给出 width 或 height")
        return self


class VideoQualityParams(StrictModel):
    color: Optional[ColorParams] = None
    #: unsharp 强度；0 表示不锐化。
    sharpen: float = Field(0.0, ge=0.0, le=3.0)
    denoise: Literal["off", "light", "medium", "strong"] = "off"
    #: 分辨率归一化（多平台适配的基础能力）。
    scale: Optional[ScaleParams] = None


class FpsConvertParams(StrictModel):
    fps: float = Field(gt=0.0, le=240.0)
    #: cfr = 恒定帧率重采样（丢/补帧）；drop/dup 是 ffmpeg 的显式丢帧、补帧语义。
    mode: Literal["cfr", "drop", "dup"] = "cfr"


class DeflickerParams(StrictModel):
    size: int = Field(5, ge=2, le=129)
    mode: Literal["am", "gm", "hm", "ma", "md", "pm"] = "am"


class FrameInspectParams(StrictModel):
    #: 抽检帧数，均匀分布在整片。
    sample_count: int = Field(8, ge=1, le=200)


def _filters(ctx: MediaContext) -> List[str]:
    return ctx.plan_section("video_filters", [])


def _scale_filter(scale: ScaleParams) -> str:
    width = scale.width
    height = scale.height
    # 只给一边时，另一边用 -2 自动推导（-2 保证偶数，yuv420p 必需）。
    w = str(width) if width is not None else "-2"
    h = str(height) if height is not None else "-2"
    if scale.mode == "fill":
        box_w = str(width) if width is not None else "iw"
        box_h = str(height) if height is not None else "ih"
        return (
            f"scale={w}:{h}:force_original_aspect_ratio=increase,"
            f"crop={box_w}:{box_h}"
        )
    if scale.mode == "pad":
        box_w = str(width) if width is not None else "iw"
        box_h = str(height) if height is not None else "ih"
        return (
            f"scale={w}:{h}:force_original_aspect_ratio=decrease,"
            f"pad={box_w}:{box_h}:(ow-iw)/2:(oh-ih)/2:color={scale.pad_color}"
        )
    return f"scale={w}:{h}:force_original_aspect_ratio=decrease"


def implement_video_quality(ctx: MediaContext, params: VideoQualityParams) -> dict:
    chain = _filters(ctx)
    if params.scale is not None:
        chain.append(_scale_filter(params.scale))
        ctx.plan["target_resolution"] = {
            "width": params.scale.width,
            "height": params.scale.height,
            "mode": params.scale.mode,
        }
    if params.color is not None:
        color = params.color
        if any(
            (color.brightness, color.contrast, color.saturation, color.gamma)
        ):
            chain.append(
                "eq=brightness={brightness}:contrast={contrast}:saturation={saturation}"
                ":gamma={gamma}".format(**color.model_dump())
            )
    if params.sharpen > 0:
        chain.append(f"unsharp=5:5:{params.sharpen}")
    denoise = DENOISE_PRESETS[params.denoise]
    if denoise:
        chain.append(denoise)
    if chain:
        ctx.notes.append("已加入画面处理：" + ", ".join(chain))
    return {"filters": chain}


def implement_fps_convert(ctx: MediaContext, params: FpsConvertParams) -> dict:
    chain = _filters(ctx)
    if params.mode == "drop":
        # 只保留目标帧率附近的帧，不补帧：低帧率源素材不会因此变流畅，但时长不变。
        chain.append(f"select='isnan(prev_selected_t)+gte(t-prev_selected_t,{1.0 / params.fps:.6f})'")
        chain.append(f"fps={params.fps}")
    elif params.mode == "dup":
        chain.append(f"fps=fps={params.fps}:round=up")
    else:
        chain.append(f"fps={params.fps}")
    ctx.notes.append(f"帧率转换 → {params.fps}fps（{params.mode}）")
    return {"filters": chain}


def implement_deflicker(ctx: MediaContext, params: DeflickerParams) -> dict:
    chain = _filters(ctx)
    chain.append(f"deflicker=size={params.size}:mode={params.mode}")
    ctx.notes.append(f"去闪烁：deflicker size={params.size} mode={params.mode}")
    return {"filters": chain}


def implement_frame_inspect(ctx: MediaContext, params: FrameInspectParams) -> dict:
    """逐帧质量抽检：按固定间隔取帧，用 signalstats 统计 YAVG/YMIN/YMAX 等指标。

    产出的是**诊断数据**（抽检报告），不是艺术处理，所以只写进产物登记，
    不改变画面本身。
    """
    duration = float(ctx.probe.get("duration") or 0.0)
    if duration <= 0:
        raise bad_param("无法确定时长，跳过逐帧抽检")
    interval = max(duration / (params.sample_count + 1), 0.05)
    report_path = ctx.scratch("frame_inspect.txt")
    run_checked(
        [
            ctx.ffmpeg,
            "-hide_banner",
            "-nostdin",
            "-i",
            str(ctx.primary),
            "-vf",
            f"fps=1/{interval:.6f},signalstats,metadata=print:file={report_path}",
            "-frames:v",
            str(params.sample_count),
            "-an",
            "-f",
            "null",
            "-",
        ],
        timeout=600.0,
    )
    text = report_path.read_text(encoding="utf-8", errors="replace") if report_path.exists() else ""
    frames = []
    current = {}
    for line in text.splitlines():
        if line.startswith("frame:"):
            if current:
                frames.append(current)
            current = {"frame": int(line.split(":", 1)[1].strip() or 0)}
        elif "=" in line and current:
            key, _, value = line.partition("=")
            key = key.strip()
            if key.startswith("lavfi.signalstats."):
                try:
                    current[key.split(".")[-1]] = float(value.strip())
                except ValueError:
                    pass
    if current:
        frames.append(current)

    ctx.plan_section("frame_report", {})["samples"] = frames
    ctx.plan["frame_report"]["sample_interval_seconds"] = round(interval, 4)
    ctx.artifacts.append(
        {
            "kind": "frame_report",
            "path": report_path,
            "name": "frame_inspect.txt",
            "media": {"samples": len(frames)},
        }
    )
    ctx.notes.append(f"逐帧抽检：采样 {len(frames)} 帧（间隔 {interval:.3f}s）")
    return {"samples": len(frames)}


register(
    ModuleSpec(
        name="video.quality",
        title="画面质量处理（C4）",
        capability="video_filters",
        requires=frozenset({"video"}),
        produces=frozenset({"video_filters"}),
        params_model=VideoQualityParams,
        implement=implement_video_quality,
        # C4 真正用到的滤镜：缩放 + 亮度/对比度/饱和度 + 锐化 + 降噪。
        capability_probe=requires_filters("scale", "eq", "unsharp", "hqdn3d"),
    )
)
register(
    ModuleSpec(
        name="frame.fps_convert",
        title="帧率转换（C9）",
        capability="video_filters",
        requires=frozenset({"video"}),
        produces=frozenset({"video_filters"}),
        params_model=FpsConvertParams,
        implement=implement_fps_convert,
        capability_probe=requires_filters("fps"),
    )
)
register(
    ModuleSpec(
        name="frame.deflicker",
        title="去闪烁/去抖（C9）",
        capability="video_filters",
        requires=frozenset({"video"}),
        produces=frozenset({"video_filters"}),
        params_model=DeflickerParams,
        implement=implement_deflicker,
        capability_probe=requires_filters("deflicker"),
    )
)
register(
    ModuleSpec(
        name="frame.inspect",
        title="逐帧质量抽检（C9）",
        capability="signalstats",
        requires=frozenset({"video"}),
        produces=frozenset({"frame_report"}),
        params_model=FrameInspectParams,
        implement=implement_frame_inspect,
        # 抽检链是 `fps,signalstats,metadata=print`，三个滤镜缺一不可。
        capability_probe=requires_filters("fps", "signalstats", "metadata"),
    )
)
