# -*- coding: utf-8 -*-
"""过程图排版：把抽帧与分镜概要文字合成为 4 张「AI 工作台」风格的报审过程图。

设计对齐 Grove-Max 产物形态（黑底工作台界面：左资产栏 / 中分镜概要卡 / 右视频分镜帧），
纯本地 Pillow 排版，不依赖网络。调用方（桌面端深度处理流程）传入：

    python make_process_images.py \
        --output-dir <产物目录> \
        --title <剧名> \
        --episode-label <"第03集"> \
        --frames <逗号分隔的 12 张帧图路径> \
        --poster <海报路径，可选> \
        --summaries <概要 JSON 文件路径，可选>

输出：<output-dir>/过程图_01.png ~ 过程图_04.png，退出码 0；参数/文件异常时以中文报错退出 1。
"""

import argparse
import json
import os
import sys

from PIL import Image, ImageDraw, ImageFont, ImageOps

# 画布与配色（与 Grove 产物截图观感一致：深底、绿点缀、灰层级）。
CANVAS_W, CANVAS_H = 1600, 1000
BG = (11, 11, 14)
PANEL = (22, 22, 27)
PANEL_LINE = (44, 44, 52)
TEXT_MAIN = (232, 232, 236)
TEXT_SUB = (150, 150, 160)
ACCENT = (52, 199, 123)
CHIP_BG = (34, 34, 40)
SUBTITLE_BG = (240, 240, 244)
SUBTITLE_FG = (24, 24, 28)

IMAGE_COUNT = 4
FRAMES_PER_IMAGE = 3


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


def font(size, bold=False):
    """加载中文字体：macOS 优先苹方，Windows 优先微软雅黑。"""
    candidates = [
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "C:/Windows/Fonts/msyhbd.ttc" if bold else "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/simhei.ttf",
    ]
    return probe_font(candidates, size)


def load_image(path, box_w, box_h):
    """居中裁剪到目标比例并缩放；文件缺失/损坏返回 None，由调用方画占位。"""
    if not path or not os.path.isfile(path):
        return None
    try:
        with Image.open(path) as source:
            return ImageOps.fit(source.convert("RGB"), (box_w, box_h))
    except OSError:
        return None


def paste_placeholder(draw, x, y, w, h, label):
    """图片缺失时的深灰占位块。"""
    draw.rectangle([x, y, x + w, y + h], fill=(28, 28, 34), outline=PANEL_LINE)
    draw.text((x + w // 2, y + h // 2), label, font=font(18), fill=TEXT_SUB, anchor="mm")


def paste_image(canvas, draw, path, x, y, w, h, label):
    image = load_image(path, w, h)
    if image is None:
        paste_placeholder(draw, x, y, w, h, label)
        return
    canvas.paste(image, (x, y))
    draw.rectangle([x, y, x + w, y + h], outline=PANEL_LINE)


def wrap_text(text, per_line):
    """中文按固定字数折行（概要文字多为中文，按字符数切即可）。"""
    text = (text or "").strip()
    return [text[index:index + per_line] for index in range(0, len(text), per_line)] or [""]


def fake_timecode(index):
    """每张图一个确定的伪时间码（同参数重跑输出一致）。"""
    return f"01:{40 + index * 7:02d}:{(13 + index * 17) % 60:02d}"


def chip(draw, x, y, text, chip_font):
    width = int(chip_font.getlength(text)) + 24
    draw.rounded_rectangle([x, y, x + width, y + 30], radius=15, fill=CHIP_BG)
    draw.text((x + 12, y + 15), text, font=chip_font, fill=TEXT_SUB, anchor="lm")
    return x + width + 12


def render_image(index, title, episode_label, frames, poster, summary):
    """渲染第 index（0 起）张过程图。"""
    canvas = Image.new("RGB", (CANVAS_W, CANVAS_H), BG)
    draw = ImageDraw.Draw(canvas)
    f_title = font(34, bold=True)
    f_sub = font(20)
    f_label = font(18)
    f_body = font(17)
    f_chip = font(14)

    timecode = fake_timecode(index)

    # ---- 顶栏：剧名 + 集 + 本集概要；右侧制作流程进度（4 段胶囊，当前段高亮）----
    draw.text((36, 26), title or "未命名短剧", font=f_title, fill=TEXT_MAIN)
    draw.text(
        (36 + int(f_title.getlength(title or "未命名短剧")) + 20, 38),
        f"{episode_label} · 本集概要 · {timecode}",
        font=f_sub,
        fill=TEXT_SUB,
    )
    progress_x = CANVAS_W - 36 - (IMAGE_COUNT * 90 - 10)
    for step in range(IMAGE_COUNT):
        x0 = progress_x + step * 100
        fill = ACCENT if step == index else (40, 40, 47)
        draw.rounded_rectangle([x0, 34, x0 + 90, 46], radius=6, fill=fill)
    draw.text((CANVAS_W - 36, 62), "制作流程", font=f_label, fill=TEXT_SUB, anchor="ra")

    # ---- 左栏「资产」：海报（或首帧）+ 一张分镜工作空间缩略 ----
    draw.text((36, 108), "资产", font=f_sub, fill=ACCENT)
    asset_x, asset_w = 36, 300
    paste_image(canvas, draw, poster or frames[0], asset_x, 142, asset_w, 400, "资产图")
    draw.text((asset_x, 556), "角色 / 场景设定", font=f_label, fill=TEXT_SUB)
    paste_image(canvas, draw, frames[0], asset_x, 592, asset_w, 180, "工作空间")
    draw.text((asset_x, 786), "分镜工作空间", font=f_label, fill=TEXT_SUB)

    # ---- 中栏「分镜概要」：3 张文字卡（时间码 + 描述 + 标签）----
    mid_x, mid_w = 356, 660
    draw.text((mid_x, 108), "分镜概要", font=f_sub, fill=ACCENT)
    card_gap = 16
    card_h = (CANVAS_H - 150 - 12 - (FRAMES_PER_IMAGE - 1) * card_gap) // FRAMES_PER_IMAGE
    lines = wrap_text(summary, 36)
    for card in range(FRAMES_PER_IMAGE):
        y0 = 142 + card * (card_h + card_gap)
        draw.rounded_rectangle(
            [mid_x, y0, mid_x + mid_w, y0 + card_h], radius=14, fill=PANEL, outline=PANEL_LINE
        )
        head_y = y0 + 18
        draw.ellipse([mid_x + 20, head_y + 8, mid_x + 32, head_y + 20], fill=ACCENT)
        draw.text(
            (mid_x + 44, head_y + 14),
            f"分镜概要 {card + 1}  ·  {timecode}:{card + 1}0",
            font=f_label,
            fill=TEXT_MAIN,
        )
        for line_index, line in enumerate(lines[:4]):
            draw.text((mid_x + 24, head_y + 52 + line_index * 28), line, font=f_body, fill=TEXT_SUB)
        chips_y = y0 + card_h - 44
        cx = mid_x + 24
        for chip_text in (f"#{title or '短剧'}", f"#{episode_label}", "#镜头语言"):
            cx = chip(draw, cx, chips_y, chip_text, f_chip)
            if cx > mid_x + mid_w - 120:
                break

    # ---- 右栏「视频分镜」：3 张帧卡（帧 + 底部字幕条）----
    right_x, right_w = 1036, 528
    draw.text((right_x, 108), "视频分镜", font=f_sub, fill=ACCENT)
    frame_h = (CANVAS_H - 150 - 12 - (FRAMES_PER_IMAGE - 1) * card_gap) // FRAMES_PER_IMAGE
    for slot in range(FRAMES_PER_IMAGE):
        y0 = 142 + slot * (frame_h + card_gap)
        frame = frames[(index * FRAMES_PER_IMAGE + slot) % len(frames)]
        paste_image(canvas, draw, frame, right_x, y0, right_w, frame_h - 44, f"分镜 {slot + 1}")
        # 字幕条：仿工作台视频卡的底部字幕样式。
        bar_y0 = y0 + frame_h - 40
        draw.rectangle([right_x, bar_y0, right_x + right_w, bar_y0 + 36], fill=SUBTITLE_BG)
        draw.text(
            (right_x + right_w // 2, bar_y0 + 18),
            f"{title or '短剧'} · {episode_label} · 镜头 {slot + 1}",
            font=f_chip,
            fill=SUBTITLE_FG,
            anchor="mm",
        )

    return canvas


def main():
    parser = argparse.ArgumentParser(description="生成 4 张报审过程图")
    parser.add_argument("--output-dir", required=True)
    parser.add_argument("--title", default="")
    parser.add_argument("--episode-label", default="第01集")
    parser.add_argument("--frames", required=True, help="逗号分隔的帧图路径（建议 12 张）")
    parser.add_argument("--poster", default="")
    parser.add_argument("--summaries", default="", help="概要 JSON 文件（{\"summaries\": [...]})")
    args = parser.parse_args()

    os.makedirs(args.output_dir, exist_ok=True)
    frames = [path for path in args.frames.split(",") if path.strip()]
    if not frames:
        fail("未提供任何帧图路径")

    # 概要文字：JSON 里每图一段；不足 4 段时用模板补齐。
    summaries = []
    if args.summaries and os.path.isfile(args.summaries):
        try:
            with open(args.summaries, encoding="utf-8") as handle:
                summaries = json.load(handle).get("summaries", [])
        except (OSError, ValueError) as error:
            print(f"概要 JSON 解析失败（{error}），使用模板文字", file=sys.stderr)
    title = args.title or "未命名短剧"
    for index in range(IMAGE_COUNT):
        if index >= len(summaries) or not str(summaries[index]).strip():
            summaries.append(
                f"《{title}》{args.episode_label} 分镜 {index + 1}：剧情推进，{fake_timecode(index)} 处关键情节，冲突升级。"
            )

    for index in range(IMAGE_COUNT):
        canvas = render_image(
            index,
            title,
            args.episode_label,
            frames,
            args.poster,
            str(summaries[index]),
        )
        out_path = os.path.join(args.output_dir, f"过程图_{index + 1:02d}.png")
        canvas.save(out_path)
        print(f"已生成 {out_path}")


if __name__ == "__main__":
    main()
