# -*- coding: utf-8 -*-
"""AI 制作证明排版：把深度处理的材料清单与 AI 声明渲染为一张证明图片。

与 make_process_images.py 同风格纯本地 Pillow 排版，不依赖网络。调用方
（桌面端深度处理流程）把证明字段写入 JSON 数据文件后调用：

    python make_proof_image.py --data <证明字段 JSON> --output <输出 PNG 路径>

JSON 字段（缺失项按“未填写”渲染）：
    date / version / dramaName / originalEpisodes / encoder / evidence /
    title / titleSource / synopsis / synopsisSource /
    posterName / posterSource / posterPath（可选，嵌入海报缩略图）/
    processCount / storyboardSource

输出：单张 PNG（1240x1754，A4 比例白底正式文件风格），
参数/文件异常时以中文报错退出 1。
"""

import argparse
import json
import os
import sys

from PIL import Image, ImageDraw, ImageFont, ImageOps

# 画布与配色：白底正式证明文件，深色页眉呼应工作台产物的品牌黑。
CANVAS_W, CANVAS_H = 1240, 1754
BG = (250, 250, 248)
CARD = (255, 255, 255)
CARD_LINE = (216, 216, 212)
HEADER_BG = (17, 17, 20)
TEXT_MAIN = (28, 28, 32)
TEXT_SUB = (110, 110, 118)
ACCENT = (52, 199, 123)
CHIP_BG = (238, 240, 236)
CHIP_FG = (74, 96, 84)

MARGIN = 72
CARD_GAP = 20


def fail(message):
    print(f"AI 制作证明生成失败：{message}", file=sys.stderr)
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


def wrap_text(text, per_line):
    """中文按固定字数折行（证明文字多为中文，按字符数切即可）。"""
    text = (text or "").strip()
    return [text[index:index + per_line] for index in range(0, len(text), per_line)] or [""]


def chip(draw, x, y, text, chip_font):
    """来源徽章；返回右边界，供下一枚徽章定位。"""
    width = int(chip_font.getlength(text)) + 28
    draw.rounded_rectangle([x, y, x + width, y + 32], radius=16, fill=CHIP_BG)
    draw.text((x + 14, y + 16), text, font=chip_font, fill=CHIP_FG, anchor="lm")
    return x + width + 12


def load_poster(path, box_w, box_h):
    """海报缩略图：居中裁剪缩放；缺失/损坏返回 None（画占位块）。"""
    if not path or not os.path.isfile(path):
        return None
    try:
        with Image.open(path) as source:
            return ImageOps.fit(source.convert("RGB"), (box_w, box_h))
    except OSError:
        return None


def draw_section(canvas, draw, y, height, title):
    """画一张白卡与左上角小节标题；返回卡片内容起始 y。"""
    x0, x1 = MARGIN, CANVAS_W - MARGIN
    draw.rounded_rectangle([x0, y, x1, y + height], radius=16, fill=CARD, outline=CARD_LINE)
    draw.text((x0 + 28, y + 22), title, font=font(24, bold=True), fill=TEXT_MAIN)
    return y + 66


def row(draw, x, y, label, value, value_font=None, value_fill=None):
    """键值行：浅灰标签列 + 正文；返回下一行 y。"""
    draw.text((x, y), label, font=font(19), fill=TEXT_SUB)
    draw.text(
        (x + 180, y),
        value or "未填写",
        font=value_font or font(19),
        fill=value_fill or TEXT_MAIN,
    )
    return y + 38


def render(data):
    canvas = Image.new("RGB", (CANVAS_W, CANVAS_H), BG)
    draw = ImageDraw.Draw(canvas)
    f_h1 = font(44, bold=True)
    f_sub = font(20)
    f_body = font(19)
    f_small = font(16)
    f_chip = font(15)

    version = str(data.get("version") or "").strip()
    drama = str(data.get("dramaName") or "").strip() or "未命名剧目"

    # ---- 页眉：品牌黑横幅 + AI 徽标 ----
    draw.rectangle([0, 0, CANVAS_W, 132], fill=HEADER_BG)
    draw.text((MARGIN, 36), "AI 制作证明", font=f_h1, fill=(245, 245, 242))
    badge = "由人工智能辅助制作"
    badge_w = int(f_sub.getlength(badge)) + 32
    draw.rounded_rectangle(
        [CANVAS_W - MARGIN - badge_w, 48, CANVAS_W - MARGIN, 84], radius=18, fill=ACCENT
    )
    draw.text(
        (CANVAS_W - MARGIN - badge_w // 2, 66), badge, font=f_small, fill=(10, 26, 18), anchor="mm"
    )

    y = 172

    # ---- 基本信息卡 ----
    info_h = 66 + 38 * 5 + 18
    inner = draw_section(canvas, draw, y, info_h, "基本信息")
    inner = row(draw, MARGIN + 28, inner, "生成日期", str(data.get("date") or ""), f_body)
    inner = row(draw, MARGIN + 28, inner, "制作工具", f"DramaDossier v{version}".strip())
    inner = row(
        draw,
        MARGIN + 28,
        inner,
        "原剧目",
        f"{drama}（原始素材 {data.get('originalEpisodes', 0)} 集）",
    )
    processing = str(data.get("processing") or "").strip() or (
        f"按场景切点重组分集并逐集微帧，编码 {data.get('encoder') or ''}".strip("， ")
    )
    inner = row(draw, MARGIN + 28, inner, "处理方式", processing)
    row(
        draw,
        MARGIN + 28,
        inner,
        "内容证据",
        str(data.get("evidence") or "未提取（ASR/OCR 未就绪或未启用）"),
    )
    y += info_h + CARD_GAP

    # ---- 材料明细卡：四行，每行右侧一枚来源徽章 ----
    items = [
        ("剧名", str(data.get("title") or ""), str(data.get("titleSource") or "未生成")),
        (
            "简介",
            f"（详见下方简介正文，{len(str(data.get('synopsis') or ''))} 字）",
            str(data.get("synopsisSource") or "未生成"),
        ),
        ("海报", str(data.get("posterName") or ""), str(data.get("posterSource") or "未生成")),
        (
            "过程图",
            f"已生成 {data.get('processCount', 0)} 张（画面抽自原始素材代表集）",
            str(data.get("storyboardSource") or "未生成"),
        ),
    ]
    detail_h = 66 + 44 * len(items) + 14
    inner = draw_section(canvas, draw, y, detail_h, "材料明细")
    for label, value, source in items:
        draw.text((MARGIN + 28, inner + 8), label, font=f_body, fill=TEXT_SUB)
        draw.text((MARGIN + 28 + 180, inner + 8), value or "未填写", font=f_body, fill=TEXT_MAIN)
        chip(draw, CANVAS_W - MARGIN - 200, inner, source, f_chip)
        inner += 44
    y += detail_h + CARD_GAP

    # ---- 简介正文卡：折行渲染，右侧海报缩略位（卡高须同时容纳文字与海报）----
    synopsis_lines = wrap_text(str(data.get("synopsis") or ""), 34)
    POSTER_THUMB_H = 364
    synopsis_h = max(66 + 34 * len(synopsis_lines) + 24, 66 + POSTER_THUMB_H + 24)
    inner = draw_section(canvas, draw, y, synopsis_h, "剧情简介")
    poster = load_poster(str(data.get("posterPath") or ""), 260, POSTER_THUMB_H)
    if poster is None:
        draw.rounded_rectangle(
            [CANVAS_W - MARGIN - 28 - 260, inner, CANVAS_W - MARGIN - 28, inner + POSTER_THUMB_H],
            radius=14,
            fill=(236, 236, 232),
            outline=CARD_LINE,
        )
        draw.text(
            (CANVAS_W - MARGIN - 28 - 130, inner + POSTER_THUMB_H // 2),
            "海报未生成",
            font=f_small,
            fill=TEXT_SUB,
            anchor="mm",
        )
    else:
        canvas.paste(poster, (CANVAS_W - MARGIN - 28 - 260, inner))
        draw.rectangle(
            [CANVAS_W - MARGIN - 28 - 260, inner, CANVAS_W - MARGIN - 28, inner + POSTER_THUMB_H],
            outline=CARD_LINE,
        )
    for line_index, line in enumerate(synopsis_lines[:12]):
        draw.text(
            (MARGIN + 28, inner + 6 + line_index * 34), line, font=f_body, fill=TEXT_MAIN
        )
    y += synopsis_h + CARD_GAP

    # ---- AI 声明卡 ----
    declaration = (
        "AI 声明：本目录中的剧名、简介、海报与过程图由人工智能辅助生成或整理，"
        "分集重组与微帧由 DramaDossier 自动化完成，原始视频素材未作改动。"
        "报审前请人工复核上述内容。"
    )
    decl_lines = wrap_text(declaration, 40)
    decl_h = 66 + 34 * len(decl_lines) + 20
    inner = draw_section(canvas, draw, y, decl_h, "AI 声明")
    for line_index, line in enumerate(decl_lines):
        draw.text((MARGIN + 28, inner + line_index * 34), line, font=f_body, fill=TEXT_MAIN)
    y += decl_h + CARD_GAP

    # ---- 页脚：生成方落款 ----
    draw.line([MARGIN, CANVAS_H - 96, CANVAS_W - MARGIN, CANVAS_H - 96], fill=CARD_LINE, width=1)
    footer = f"本证明由 DramaDossier v{version} 自动生成 · {data.get('date') or ''}".strip(" ·")
    draw.text((MARGIN, CANVAS_H - 70), footer, font=f_small, fill=TEXT_SUB)
    draw.text(
        (CANVAS_W - MARGIN, CANVAS_H - 70),
        "剧备 DramaDossier",
        font=f_small,
        fill=TEXT_SUB,
        anchor="ra",
    )

    return canvas


def main():
    parser = argparse.ArgumentParser(description="生成 AI 制作证明图片")
    parser.add_argument("--data", required=True, help="证明字段 JSON 文件路径")
    parser.add_argument("--output", required=True, help="输出 PNG 路径")
    args = parser.parse_args()

    if not os.path.isfile(args.data):
        fail(f"证明数据文件不存在：{args.data}")
    try:
        with open(args.data, encoding="utf-8") as handle:
            data = json.load(handle)
    except (OSError, ValueError) as error:
        fail(f"证明数据 JSON 解析失败：{error}")
    if not isinstance(data, dict):
        fail("证明数据 JSON 顶层必须是对象")

    output_dir = os.path.dirname(os.path.abspath(args.output))
    os.makedirs(output_dir, exist_ok=True)
    canvas = render(data)
    canvas.save(args.output)
    print(f"已生成 {args.output}")


if __name__ == "__main__":
    main()
