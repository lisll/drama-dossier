"""overlay 模块（AI 角标/剧名水印）的技术验证测试（对照 Grove 逆向文档 §6/§8）。"""
from __future__ import annotations

import pytest
from PIL import Image

from drama_cloud.modules.overlay import (
    OverlayParams,
    compile_overlay_expr,
    find_font,
    render_text_png,
)


class TestExprCompilation:
    def test_static_anchors(self):
        x, y = compile_overlay_expr("left", "top", 24, 0, 300, 0, 300)
        assert x == "24" and y == "24"
        x, y = compile_overlay_expr("right", "bottom", 24, 0, 300, 0, 300)
        assert x == "main_w-overlay_w-24" and y == "main_h-overlay_h-24"

    def test_drift_uses_sine_expression_without_comma(self):
        """漂移编译成 sin 表达式且不含逗号（可直接嵌入 filter_complex）。"""
        x, _ = compile_overlay_expr("left", "top", 24, 6.0, 240.0, 0, 300)
        assert x.startswith("(24)+6.0000*sin(2*PI*t/240.0000)")
        assert "," not in x

    def test_invalid_anchor_rejected(self):
        with pytest.raises(Exception):
            OverlayParams(text="x", anchor_x="center").normalized_anchor()


class TestPngRendering:
    def test_renders_rgba_png_with_opacity(self, tmp_path):
        font = find_font()  # 本机探测不到会抛 CAPABILITY_MISSING，则该用例依赖字体
        target = tmp_path / "badge.png"
        render_text_png("测试角标", target, font, 36, "#FFFFFF", None, 0.5, 12)
        with Image.open(target) as image:
            assert image.mode == "RGBA"
            assert image.width > 24 and image.height > 24
            # 半透明像素确实存在（透明度烘进像素而非滤镜参数）
            alphas = {px[3] for px in image.getdata()}
            assert any(0 < a < 255 for a in alphas)
