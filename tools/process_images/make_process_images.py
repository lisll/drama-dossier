# -*- coding: utf-8 -*-
"""过程图排版：把原片抽帧输出为 4 张竖屏单帧图，观感对齐剪辑软件导出的分镜帧。

设计原则（对齐 Grove-Max 的「分镜图帧」形态）：不用任何界面化排版——
每张过程图就是一张**原片帧原样输出**（原生画面与硬字幕都保留），
仅在右上角叠一枚半透明小标签（集内时间码 · 分辨率），像剪辑师
从时间线上随手导出的帧。调用方（桌面端深度处理流程）传入：

    python make_process_images.py \
        --output-dir <产物目录> \
        --title <剧名> \
        --episode-label <"全剧抽样"> \
        --frames <逗号分隔的候选帧（12 张左右，可乱序多源）> \
        --labels <竖线分隔的帧标签，与帧一一对应（如 "第2集 · 01:23"）> \
        --poster <可选，忽略> \
        --summaries <可选，忽略>

选帧：按亮度过滤黑场/纯色帧后均匀取 4 张（不足时允许复用，取最亮的补位）。
输出：<output-dir>/过程图_01.png ~ 过程图_04.png，退出码 0；异常时中文报错退出 1。
"""

import argparse
import os
import sys

from PIL import Image, ImageDraw, ImageFont, ImageOps, ImageStat

IMAGE_COUNT = 4
# 亮度阈值：均值低于它视为黑场/转场帧，不作为过程图素材。
MIN_MEAN_BRIGHTNESS = 22.0
# 竖屏判定：帧高明显大于宽才算竖屏原片帧，否则（异常横屏素材）裁成 9:16。
MIN_FRAMES = IMAGE_COUNT


def fail(message):
    print(f"过程图生成失败：{message}", file=sys.stderr)
    sys.exit(1)


def probe_font(candidates, size):
    """按平台候选顺序探测中文字体；全部缺失时退回 Pillow 默认位图字体。"""
    for path in candidates:
        if os.path.isfile(path):
            try:
                return ImageFont.truetype(path, size)
            except OSError:
                continue
    return ImageFont.load_default()


def font(size):
    candidates = [
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/simhei.ttf",
    ]
    return probe_font(candidates, size)


def mean_brightness(image):
    """灰度均值（0~255）：黑场与全白转场卡都会被过滤。"""
    gray = image.convert("L").resize((32, 32))
    return ImageStat.Stat(gray).mean[0]


def load_frame(path):
    """读帧：缺失/损坏返回 None。返回 (PIL.Image, 标签用的宽高)。"""
    if not path or not os.path.isfile(path):
        return None
    try:
        with Image.open(path) as source:
            frame = source.convert("RGB")
            return frame
    except OSError:
        return None


def normalize_frame(frame):
    """竖屏帧原样保留（最真实）；异常横屏帧居中裁成 9:16。"""
    width, height = frame.size
    if height > width:
        return frame
    target = (int(height * 9 / 16), height)
    return ImageOps.fit(frame, target)


def pick_frames(paths, labels):
    """从候选帧中选出 IMAGE_COUNT 张：过滤坏帧/黑场，按来源分组取样。

    候选帧的组织是「若干抽样来源 × 每源多帧」（如 4 集 × 10%/40%/70%），
    因此均分成 4 组、每组取中间一帧，保证 4 张图来自不同片段而非同一位置。
    返回 [(frame, label)]；候选全部不可用时返回空列表。
    """
    candidates = []
    for index, path in enumerate(paths):
        frame = load_frame(path)
        if frame is None:
            continue
        if mean_brightness(frame) < MIN_MEAN_BRIGHTNESS:
            continue
        label = labels[index] if index < len(labels) else ""
        candidates.append((frame, label))
    if not candidates:
        return []
    if len(candidates) <= IMAGE_COUNT:
        picked = list(candidates)
    else:
        chunk = (len(candidates) + IMAGE_COUNT - 1) // IMAGE_COUNT
        picked = []
        for index in range(IMAGE_COUNT):
            group = candidates[index * chunk:(index + 1) * chunk]
            if group:
                picked.append(group[len(group) // 2])
    # 不足 4 张时用已有候选循环补位，保证产出数量稳定。
    fill = 0
    while len(picked) < IMAGE_COUNT and candidates:
        picked.append(candidates[fill % len(candidates)])
        fill += 1
    return picked[:IMAGE_COUNT]


def render(frame, label):
    """原片帧 + 右上角半透明小标签。"""
    canvas = normalize_frame(frame)
    width, height = canvas.size

    tag_parts = []
    if label:
        tag_parts.append(label)
    tag_parts.append(f"{width}×{height}")
    tag_text = "  ".join(tag_parts)
    tag_font = font(max(22, width // 44))

    # 标签底：半透明黑圆角条（RGBA 叠加，观感接近播放器时间码角标）。
    overlay = Image.new("RGBA", canvas.size, (0, 0, 0, 0))
    overlay_draw = ImageDraw.Draw(overlay)
    text_w = int(tag_font.getlength(tag_text))
    pad_x, pad_y = width // 60, width // 90
    box_w = text_w + pad_x * 2
    box_h = int(tag_font.size * 1.7)
    x1, y1 = width - pad_x, pad_y
    x0, y0 = x1 - box_w, y1
    overlay_draw.rounded_rectangle(
        [x0, y0, x1, y0 + box_h], radius=box_h // 2, fill=(0, 0, 0, 150)
    )
    canvas = Image.alpha_composite(canvas.convert("RGBA"), overlay)
    draw = ImageDraw.Draw(canvas)
    draw.text(
        (x1 - pad_x, y0 + box_h // 2),
        tag_text,
        font=tag_font,
        fill=(255, 255, 255, 235),
        anchor="rm",
    )
    return canvas.convert("RGB")


def main():
    parser = argparse.ArgumentParser(description="生成 4 张竖屏过程图（原片帧）")
    parser.add_argument("--output-dir", required=True)
    parser.add_argument("--title", default="")
    parser.add_argument("--episode-label", default="全剧抽样")
    parser.add_argument("--frames", required=True, help="逗号分隔的候选帧图路径")
    parser.add_argument("--labels", default="", help="竖线分隔的帧标签（与帧一一对应）")
    parser.add_argument("--poster", default="", help="兼容保留，不使用")
    parser.add_argument("--summaries", default="", help="兼容保留，不使用")
    args = parser.parse_args()

    os.makedirs(args.output_dir, exist_ok=True)
    paths = [path for path in args.frames.split(",") if path.strip()]
    if not paths:
        fail("未提供任何帧图路径")
    # 标签用 | 分隔（时间码文本里可能含逗号）。
    labels = [label.strip() for label in (args.labels.split("|") if args.labels else [])]

    picked = pick_frames(paths, labels)
    if len(picked) < MIN_FRAMES:
        fail("可用的候选帧不足（全部缺失或为黑场）")

    for index, (frame, label) in enumerate(picked):
        out_path = os.path.join(args.output_dir, f"过程图_{index + 1:02d}.png")
        render(frame, label).save(out_path)
        print(f"已生成 {out_path}")


if __name__ == "__main__":
    main()
