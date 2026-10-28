"""AI 声明角标与剧名水印（技术验证，对照 Grove Max 逆向文档 §6/§8）。

Grove 的做法（ai_label.py / title_watermark.py + V5 配方 boxes）：
- 文案渲染成带透明通道的 PNG，第一轮编码后、第二轮成片前用 ffmpeg overlay 叠加；
- 角标位置支持随时间正弦漂移（V5 配方 boxes 的 xWave/yWave），避免逐帧位置恒定。

本模块按既有「计划型模块」约定实现两个模块：
- ``overlay.badge``：AI 声明角标（默认左上角，对应 Grove ai-labels.json 的角落配置）；
- ``overlay.watermark``：剧名水印（默认右上角，对应转码设置里的「剧名水印右上角」）。

实现为：PIL 把文案烘焙成 RGBA PNG（透明度直接烘进像素，避免依赖 overlay 的
alpha 选项），落 ``ctx.scratch``，并把已编译好的 overlay ``x/y`` 表达式登记进
``ctx.plan["overlays"]``，由 ``media.encode`` 统一物化（见 encode.py 的 overlay 段）。
"""
from __future__ import annotations

from pathlib import Path
from typing import List, Optional

from pydantic import Field

from ..base import StrictModel
from ..errors import ErrorClass, WorkError, bad_param
from ..recipes.registry import ModuleSpec, register
from .ffmpeg import MediaContext
from .ffmpeg_caps import requires_filters

#: 常见中文字体探测顺序（技术验证：探测不到就用 font_path 显式指定）。
_FONT_CANDIDATES = (
    # macOS
    "/System/Library/Fonts/PingFang.ttc",
    "/System/Library/Fonts/STHeiti Light.ttc",
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    # Windows
    "C:/Windows/Fonts/msyh.ttc",
    "C:/Windows/Fonts/simhei.ttf",
    # Linux
    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
)

ANCHORS_X = ("left", "right")
ANCHORS_Y = ("top", "bottom")


def find_font(explicit: Optional[str] = None) -> Path:
    """定位一个可画中文的 TTF/TTC；找不到时抛 CAPABILITY_MISSING 类错误。"""
    if explicit:
        path = Path(explicit)
        if path.is_file():
            return path
        raise WorkError(ErrorClass.CAPABILITY_MISSING, f"指定的字体不存在：{explicit}")
    for candidate in _FONT_CANDIDATES:
        path = Path(candidate)
        if path.is_file():
            return path
    raise WorkError(
        ErrorClass.CAPABILITY_MISSING,
        "未找到可用中文字体；请在参数里指定 font_path（系统 ttf/ttc 文件路径）",
    )


def _clamp(value: float, low: float, high: float) -> float:
    return max(low, min(high, value))


def compile_overlay_expr(anchor_x: str, anchor_y: str, margin: int,
                         drift_x: float, drift_period_x: float,
                         drift_y: float, drift_period_y: float) -> tuple:
    """编译 overlay 的 x/y 表达式（保持无逗号，方便直接嵌入 filter_complex）。

    漂移为 0 时输出常量表达式；否则用 ``base + amp·sin(2·PI·t/period)``，
    与 V5 配方 boxes 的 xWave/yWave 语义一致（clock 用 t 秒）。
    """
    base_x = f"{margin}" if anchor_x == "left" else f"main_w-overlay_w-{margin}"
    base_y = f"{margin}" if anchor_y == "top" else f"main_h-overlay_h-{margin}"
    if drift_x > 0:
        base_x = f"({base_x})+{drift_x:.4f}*sin(2*PI*t/{drift_period_x:.4f})"
    if drift_y > 0:
        base_y = f"({base_y})+{drift_y:.4f}*sin(2*PI*t/{drift_period_y:.4f})"
    return base_x, base_y


def render_text_png(
    text: str,
    target: Path,
    font_path: Path,
    font_size: int,
    text_color: str,
    bg_color: Optional[str],
    opacity: float,
    padding: int,
) -> None:
    """把文案烘焙成 RGBA PNG（透明度烘进像素，背景可选半透明底板）。"""
    from PIL import Image, ImageDraw, ImageFont  # 延迟导入：仅 overlay 模块需要 pillow

    font = ImageFont.truetype(str(font_path), font_size)
    probe = Image.new("RGBA", (1, 1))
    draw = ImageDraw.Draw(probe)
    box = draw.textbbox((0, 0), text, font=font)
    text_w = box[2] - box[0]
    text_h = box[3] - box[1]
    width = text_w + padding * 2
    height = text_h + padding * 2
    alpha = int(round(_clamp(opacity, 0.0, 1.0) * 255))
    image = Image.new("RGBA", (max(width, 2), max(height, 2)), (0, 0, 0, 0))
    layer = ImageDraw.Draw(image)
    if bg_color:
        layer.rounded_rectangle(
            (0, 0, width - 1, height - 1), radius=height // 4,
            fill=_hex_to_rgba(bg_color, alpha),
        )
    layer.text((padding, padding - box[1]), text, font=font, fill=_hex_to_rgba(text_color, alpha))
    image.save(target, "PNG")


def _hex_to_rgba(color: str, alpha: int) -> tuple:
    color = color.lstrip("#")
    if len(color) == 3:
        color = "".join(ch * 2 for ch in color)
    if len(color) != 6:
        raise bad_param(f"颜色必须是 #RRGGBB：{color}")
    return (int(color[0:2], 16), int(color[2:4], 16), int(color[4:6], 16), alpha)


class OverlayParams(StrictModel):
    text: str = Field(min_length=1, max_length=40)
    #: 角落锚点（Grove：AI 声明=角落，剧名水印默认右上角）。
    anchor_x: str = "left"
    anchor_y: str = "top"
    margin: int = Field(default=24, ge=0, le=500)
    font_size: int = Field(default=36, ge=8, le=200)
    font_path: Optional[str] = Field(default=None, max_length=500)
    text_color: str = "#FFFFFF"
    bg_color: Optional[str] = None
    opacity: float = Field(default=0.85, ge=0.0, le=1.0)
    padding: int = Field(default=12, ge=0, le=100)
    #: 位置漂移（像素幅度与周期秒），0 = 固定位置（对应 V5 boxes 的 xWave/yWave）。
    drift_x: float = Field(default=0.0, ge=0.0, le=80.0)
    drift_period_x: float = Field(default=300.0, gt=0.0)
    drift_y: float = Field(default=0.0, ge=0.0, le=80.0)
    drift_period_y: float = Field(default=300.0, gt=0.0)

    def normalized_anchor(self) -> tuple:
        if self.anchor_x not in ANCHORS_X or self.anchor_y not in ANCHORS_Y:
            raise bad_param(f"anchor 只支持 {ANCHORS_X}/{ANCHORS_Y}")
        return self.anchor_x, self.anchor_y


def _implement_overlay(ctx: MediaContext, params: OverlayParams, kind: str) -> dict:
    if not ctx.probe.get("has_video", True):
        raise bad_param("overlay 模块要求输入包含视频轨")
    anchor_x, anchor_y = params.normalized_anchor()
    font_path = find_font(params.font_path)
    target = ctx.scratch(f"overlay-{kind}.png")
    render_text_png(
        params.text, target, font_path, params.font_size,
        params.text_color, params.bg_color, params.opacity, params.padding,
    )
    x_expr, y_expr = compile_overlay_expr(
        anchor_x, anchor_y, params.margin,
        params.drift_x, params.drift_period_x, params.drift_y, params.drift_period_y,
    )
    overlays: List[dict] = ctx.plan_section("overlays", [])
    overlays.append({"kind": kind, "path": str(target), "x": x_expr, "y": y_expr})
    ctx.notes.append(f"已生成 {kind} overlay：{params.text}（{anchor_x}/{anchor_y}）")
    return {"path": str(target), "x": x_expr, "y": y_expr}


def implement_badge(ctx: MediaContext, params: OverlayParams) -> dict:
    """AI 声明角标：默认文案对应 Grove 的「内容由AI生成，纯属虚构，请勿代入现实」。"""
    if not params.text:
        raise bad_param("角标文案不能为空")
    return _implement_overlay(ctx, params, "ai-badge")


def implement_watermark(ctx: MediaContext, params: OverlayParams) -> dict:
    """剧名水印：默认右上角（Grove 转码设置「剧名水印右上角」）。"""
    return _implement_overlay(ctx, params, "title-watermark")


register(
    ModuleSpec(
        name="overlay.badge",
        title="AI 声明角标（PNG overlay，支持位置漂移）",
        capability="ffmpeg",
        requires=frozenset({"video"}),
        produces=frozenset({"overlay_badge"}),
        params_model=OverlayParams,
        implement=implement_badge,
        # 由 media.encode 统一物化成 ffmpeg overlay，没有这个滤镜的机器不能声明能力。
        capability_probe=requires_filters("overlay"),
    )
)
register(
    ModuleSpec(
        name="overlay.watermark",
        title="剧名水印（PNG overlay，默认右上角）",
        capability="ffmpeg",
        requires=frozenset({"video"}),
        produces=frozenset({"overlay_watermark"}),
        params_model=OverlayParams,
        implement=implement_watermark,
        # 由 media.encode 统一物化成 ffmpeg overlay，没有这个滤镜的机器不能声明能力。
        capability_probe=requires_filters("overlay"),
    )
)
