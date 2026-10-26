# -*- coding: utf-8 -*-
"""过程图排版：1:1 对齐 Grove-Max 的「分镜图帧」卡片。

参考样式的卡片结构（自上而下）：
  ┌──────────────────────────────┐
  │ ▶ 分镜图帧 #1      1080 × 1920 │  ← 深色标题条（左标题 / 右分辨率）
  ├──────────────────────────────┤
  │                              │
  │        原片竖屏帧（全出血，      │  ← 真实抽帧，保留原生硬字幕，
  │        不叠任何角标/水印）       │     不做任何界面化修饰
  │                              │
  └──────────────────────────────┘

调用方（桌面端深度处理流程）传入：

    python make_process_images.py \
        --output-dir <产物目录> \
        --title <剧名，忽略> \
        --episode-label <"全剧抽样"，忽略> \
        --frames <逗号分隔的候选帧（12 张左右，可乱序多源）> \
        --labels <竖线分隔的帧标签，忽略> \
        --poster <可选，忽略> \
        --summaries <可选，忽略>

选帧：按亮度过滤黑场/纯色帧后均匀取 4 张（不足时循环补位）。
输出：<output-dir>/过程图_01.png ~ 过程图_04.png，退出码 0；异常时中文报错退出 1。
"""

import argparse
import os
import sys

from PIL import Image, ImageDraw, ImageFont, ImageOps, ImageStat

IMAGE_COUNT = 4
# 亮度阈值：均值低于它视为黑场/转场帧，不作为过程图素材。
MIN_MEAN_BRIGHTNESS = 22.0
MIN_FRAMES = IMAGE_COUNT

# 卡片配色（取自 Grove-Max 深色工作台的图帧卡片）。
CARD_BG = (20, 20, 20)
CARD_BORDER = (48, 48, 48)
TITLE_COLOR = (224, 224, 224)
META_COLOR = (130, 130, 130)


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
    """读帧：缺失/损坏返回 None。"""
    if not path or not os.path.isfile(path):
        return None
    try:
        with Image.open(path) as source:
            return source.convert("RGB")
    except OSError:
        return None


def normalize_frame(frame):
    """竖屏帧原样保留（最真实）；异常横屏帧居中裁成 9:16。"""
    width, height = frame.size
    if height > width:
        return frame
    target = (int(height * 9 / 16), height)
    return ImageOps.fit(frame, target)


def pick_frames(paths):
    """从候选帧中选出 IMAGE_COUNT 张：过滤坏帧/黑场，按来源分组取样。

    候选帧的组织是「若干抽样来源 × 每源多帧」（如 4 集 × 10%/40%/70%），
    因此均分成 4 组、每组取中间一帧，保证 4 张图来自不同片段而非同一位置。
    不足 4 张时用已有候选循环补位，保证产出数量稳定。
    """
    candidates = []
    for path in paths:
        frame = load_frame(path)
        if frame is None:
            continue
        if mean_brightness(frame) < MIN_MEAN_BRIGHTNESS:
            continue
        candidates.append(frame)
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
    fill = 0
    while len(picked) < IMAGE_COUNT and candidates:
        picked.append(candidates[fill % len(candidates)])
        fill += 1
    return picked[:IMAGE_COUNT]


def render(frame, index):
    """照抄 Grove-Max 分镜图帧卡片：深色卡片 + 标题条（#N · 分辨率）+ 原片帧全出血。"""
    frame = normalize_frame(frame)
    width, height = frame.size

    font_size = max(22, width // 40)
    header_h = int(font_size * 2.4)
    pad_x = max(14, width // 72)
    title_font = font(font_size)
    meta_font = font(max(18, int(font_size * 0.82)))

    canvas = Image.new("RGB", (width, header_h + height), CARD_BG)
    draw = ImageDraw.Draw(canvas)

    # 标题条：左侧 ▶ 分镜图帧 #N，右侧 分辨率。
    cy = header_h // 2
    tri = font_size * 0.28
    draw.polygon(
        [(pad_x, cy - tri), (pad_x, cy + tri), (pad_x + tri * 1.7, cy)],
        fill=TITLE_COLOR,
    )
    draw.text(
        (pad_x + tri * 2.7, cy),
        f"分镜图帧 #{index}",
        font=title_font,
        fill=TITLE_COLOR,
        anchor="lm",
    )
    draw.text(
        (width - pad_x, cy),
        f"{width} × {height}",
        font=meta_font,
        fill=META_COLOR,
        anchor="rm",
    )

    # 原片帧全出血铺在标题条下方；最后给整张卡片描一圈 1px 边框。
    canvas.paste(frame, (0, header_h))
    draw.rectangle(
        [0, 0, width - 1, header_h + height - 1], outline=CARD_BORDER, width=1
    )
    return canvas


def main():
    parser = argparse.ArgumentParser(description="生成 4 张竖屏过程图（分镜图帧卡片）")
    parser.add_argument("--output-dir", required=True)
    parser.add_argument("--title", default="", help="兼容保留，不使用")
    parser.add_argument("--episode-label", default="全剧抽样", help="兼容保留，不使用")
    parser.add_argument("--frames", required=True, help="逗号分隔的候选帧图路径")
    parser.add_argument("--labels", default="", help="兼容保留，不使用")
    parser.add_argument("--poster", default="", help="兼容保留，不使用")
    parser.add_argument("--summaries", default="", help="兼容保留，不使用")
    args = parser.parse_args()

    os.makedirs(args.output_dir, exist_ok=True)
    paths = [path for path in args.frames.split(",") if path.strip()]
    if not paths:
        fail("未提供任何帧图路径")

    picked = pick_frames(paths)
    if len(picked) < MIN_FRAMES:
        fail("可用的候选帧不足（全部缺失或为黑场）")

    for index, frame in enumerate(picked):
        out_path = os.path.join(args.output_dir, f"过程图_{index + 1:02d}.png")
        render(frame, index + 1).save(out_path)
        print(f"已生成 {out_path}")


if __name__ == "__main__":
    main()
