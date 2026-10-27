# -*- coding: utf-8 -*-
"""AI 合规角标渲染：把「内容由AI生成·纯属虚构」文案烘焙成带透明通道的 PNG。

与 make_process_images.py 同一套纯本地 Pillow 排版，不依赖网络。桌面端深度
处理流程在编码前调用本脚本生成角标图，再用 ffmpeg overlay 烧进成片画面：

    python make_ai_badge.py --output <PNG 路径> [--text 文案] \
        [--font-size 40] [--opacity 0.8] [--padding 14] [--bg #000000]

输出：单张 RGBA PNG（文字尺寸自适应，透明度烘进像素，避免依赖 overlay
滤镜的 alpha 选项）；参数异常时以中文报错退出 1。
"""

import argparse
import os
import sys

from PIL import Image, ImageDraw, ImageFont

# 常见中文字体探测顺序（与制作证明脚本一致）。
FONT_CANDIDATES = [
    "/System/Library/Fonts/PingFang.ttc",
    "/System/Library/Fonts/STHeiti Light.ttc",
    "C:/Windows/Fonts/msyh.ttc",
    "C:/Windows/Fonts/simhei.ttf",
]


def fail(message):
    print(f"AI 合规角标生成失败：{message}", file=sys.stderr)
    sys.exit(1)


def load_font(size):
    """按平台候选顺序探测中文字体；全部缺失时退回 Pillow 默认字体。"""
    for path in FONT_CANDIDATES:
        if os.path.isfile(path):
            try:
                return ImageFont.truetype(path, size)
            except OSError:
                continue
    return ImageFont.load_default()


def hex_to_rgb(color):
    """#RRGGBB / #RGB → RGB 元组。"""
    color = color.lstrip("#")
    if len(color) == 3:
        color = "".join(ch * 2 for ch in color)
    if len(color) != 6:
        fail(f"颜色必须是 #RRGGBB：{color}")
    return tuple(int(color[i:i + 2], 16) for i in (0, 2, 4))


def main():
    parser = argparse.ArgumentParser(description="渲染 AI 合规角标 PNG")
    parser.add_argument("--output", required=True, help="输出 PNG 路径")
    parser.add_argument("--text", default="内容由AI生成·纯属虚构", help="角标文案")
    parser.add_argument("--font-size", type=int, default=40, help="字号（像素）")
    parser.add_argument("--opacity", type=float, default=0.8, help="整体不透明度 0~1")
    parser.add_argument("--padding", type=int, default=14, help="文字四周留白（像素）")
    parser.add_argument("--bg", default="#000000", help="底板颜色 #RRGGBB")
    parser.add_argument("--fg", default="#FFFFFF", help="文字颜色 #RRGGBB")
    args = parser.parse_args()

    text = args.text.strip()
    if not text:
        fail("角标文案不能为空")
    if not 8 <= args.font_size <= 200:
        fail(f"字号须在 8~200 之间：{args.font_size}")
    if not 0.0 <= args.opacity <= 1.0:
        fail(f"不透明度须在 0~1 之间：{args.opacity}")

    alpha = int(round(max(0.0, min(1.0, args.opacity)) * 255))
    font = load_font(args.font_size)
    probe = Image.new("RGBA", (1, 1))
    box = ImageDraw.Draw(probe).textbbox((0, 0), text, font=font)
    text_w, text_h = box[2] - box[0], box[3] - box[1]
    width = text_w + args.padding * 2
    height = text_h + args.padding * 2

    image = Image.new("RGBA", (max(width, 2), max(height, 2)), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    # 半透明圆角底板 + 同透明度文字（透明度直接烘进像素）。
    draw.rounded_rectangle(
        [0, 0, width - 1, height - 1],
        radius=height // 4,
        fill=hex_to_rgb(args.bg) + (alpha,),
    )
    draw.text(
        (args.padding, args.padding - box[1]),
        text,
        font=font,
        fill=hex_to_rgb(args.fg) + (alpha,),
    )
    try:
        image.save(args.output, "PNG")
    except OSError as error:
        fail(f"写入 {args.output} 失败：{error}")
    print(f"已生成角标：{args.output}（{width}x{height}）")


if __name__ == "__main__":
    main()
