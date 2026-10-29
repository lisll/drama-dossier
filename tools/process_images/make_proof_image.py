# -*- coding: utf-8 -*-
"""AI 制作证明排版：把深度处理的材料清单、处理方式、文件指纹与 AI 声明渲染为一张证明图片。

与 make_process_images.py 同风格纯本地 Pillow 排版，不依赖网络。调用方
（桌面端深度处理收尾 / 重做海报）把证明字段写入 JSON 数据文件后调用：

    python make_proof_image.py --data <证明字段 JSON> --output <输出 PNG 路径>

JSON 字段（缺失项按“未记录 / 未填写”渲染）：
    date（北京时间）/ version / dramaName / originalEpisodes / evidence /
    processingLines[]（处理方式逐行）/
    title / titleSource / synopsis / synopsisSource /
    posterName / posterSource / posterPath（可选，嵌入海报缩略图）/
    processCount / storyboardSource /
    outputs[] / sources[]（{fileName, size, sha256}；null = 未记录）/
    aiAssisted（是否含 AI 生成内容，决定页眉徽章）/ declaration（AI 声明正文）

输出：单张 PNG，宽 1240，高度按内容自适应（不小于 1754，即 A4 比例）。
参数/文件异常时以中文报错退出 1。
"""

import argparse
import json
import os
import sys

from PIL import Image, ImageDraw, ImageFont, ImageOps

# 画布与配色：白底正式证明文件，深色页眉呼应工作台产物的品牌黑。
CANVAS_W, MIN_CANVAS_H = 1240, 1754
DRAFT_H = 8000  # 先画在足够高的草稿画布上，排完再按实际内容裁切
BG = (250, 250, 248)
CARD = (255, 255, 255)
CARD_LINE = (216, 216, 212)
HEADER_BG = (17, 17, 20)
TEXT_MAIN = (28, 28, 32)
TEXT_SUB = (110, 110, 118)
ACCENT = (52, 199, 123)

MARGIN = 72
CARD_GAP = 20
PAD = 28  # 卡片内左右留白
LABEL_W = 180  # 键值行标签列宽
SYNOPSIS_MAX_LINES = 12
TABLE_MAX_ROWS = 10
POSTER_W, POSTER_H = 260, 364
FOOTER_H = 120
NO_LINE_START = "，。、；：？！）》」』”’…,.;:?!)"

_FONT_CACHE = {}


def fail(message):
    print(f"AI 制作证明生成失败：{message}", file=sys.stderr)
    sys.exit(1)


def font(size, bold=False):
    """加载中文字体：macOS 优先苹方，Windows 优先微软雅黑；全部缺失时退回 Pillow 默认字体。"""
    key = (size, bold)
    if key not in _FONT_CACHE:
        candidates = [
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/STHeiti Light.ttc",
            "C:/Windows/Fonts/msyhbd.ttc" if bold else "C:/Windows/Fonts/msyh.ttc",
            "C:/Windows/Fonts/simhei.ttf",
            # Linux 开发机兜底（发行包只面向 Windows / macOS）
            "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        ]
        loaded = None
        for path in candidates:
            if os.path.isfile(path):
                try:
                    loaded = ImageFont.truetype(path, size)
                    break
                except OSError:
                    continue
        _FONT_CACHE[key] = loaded or ImageFont.load_default()
    return _FONT_CACHE[key]


def text_width(text, text_font):
    return text_font.getlength(text)


def wrap(text, text_font, max_w):
    """按像素宽度折行；原文里的换行先拆成段落，空段落保留为空行（不会把换行符交给 Pillow 画成重叠）。"""
    lines = []
    for paragraph in str(text or "").replace("\r\n", "\n").replace("\r", "\n").split("\n"):
        line = ""
        for char in paragraph:
            # 收尾标点不放到行首：放不下时也留在本行（只超出一个标点宽）
            if text_width(line + char, text_font) <= max_w or (line and char in NO_LINE_START):
                line += char
            else:
                lines.append(line)
                line = char
        lines.append(line)
    # 去掉首尾空行，段落之间的空行保留
    while lines and not lines[0].strip():
        lines.pop(0)
    while lines and not lines[-1].strip():
        lines.pop()
    return lines


def clamp_lines(lines, limit, text_font, max_w):
    """最多保留 limit 行；被截断时最后一行末尾换成「…」。"""
    if len(lines) <= limit:
        return lines
    kept = lines[:limit]
    last = kept[-1]
    while last and text_width(last + "…", text_font) > max_w:
        last = last[:-1]
    kept[-1] = last + "…"
    return kept


def ellipsize(text, text_font, max_w):
    """单行放不下时截断并加「…」（文件名列用）。"""
    text = str(text or "")
    if text_width(text, text_font) <= max_w:
        return text
    while text and text_width(text + "…", text_font) > max_w:
        text = text[:-1]
    return text + "…"


def human_size(size):
    size = float(size or 0)
    for unit in ("B", "KB", "MB", "GB"):
        if size < 1024 or unit == "GB":
            return f"{size:.0f} {unit}" if unit == "B" else f"{size:.1f} {unit}"
        size /= 1024
    return f"{size:.1f} GB"


def load_poster(path):
    """海报缩略图：居中裁剪缩放；缺失/损坏返回 None（画占位块）。"""
    if not path or not os.path.isfile(path):
        return None
    try:
        with Image.open(path) as source:
            return ImageOps.fit(source.convert("RGB"), (POSTER_W, POSTER_H))
    except (OSError, Image.DecompressionBombError):
        return None

class Pen:
    """绘制代理：render=False 时只算位置不落笔，用于先量出卡片高度再画卡片底。"""

    def __init__(self, canvas, draw, render):
        self.canvas = canvas
        self.draw = draw
        self.render = render

    def text(self, xy, text, text_font, fill=TEXT_MAIN, anchor="la"):
        if self.render:
            self.draw.text(xy, text, font=text_font, fill=fill, anchor=anchor)

    def line(self, points, fill=CARD_LINE, width=1):
        if self.render:
            self.draw.line(points, fill=fill, width=width)

    def box(self, rect, fill, outline=None, radius=14):
        if self.render:
            self.draw.rounded_rectangle(rect, radius=radius, fill=fill, outline=outline)

    def paste(self, image, xy):
        if self.render:
            self.canvas.paste(image, xy)


def section(canvas, draw, y, title, body):
    """一张白卡：先量高度再画底与标题，最后正式绘制内容。返回下一张卡的起始 y。"""
    x0, x1 = MARGIN, CANVAS_W - MARGIN
    inner_x, inner_w = x0 + PAD, x1 - x0 - PAD * 2
    content_top = y + 66
    end = body(Pen(canvas, draw, False), inner_x, content_top, inner_w)
    height = end - y + 18
    draw.rounded_rectangle([x0, y, x1, y + height], radius=16, fill=CARD, outline=CARD_LINE)
    draw.text((inner_x, y + 22), title, font=font(24, bold=True), fill=TEXT_MAIN)
    body(Pen(canvas, draw, True), inner_x, content_top, inner_w)
    return y + height + CARD_GAP


def key_value(pen, x, y, w, label, value, source=None):
    """键值行：浅灰标签列 + 折行正文；有来源时在正文下方另起一行小字（超长自动折行）。"""
    body_font, small = font(19), font(16)
    pen.text((x, y), label, body_font, TEXT_SUB)
    lines = wrap(value or "未填写", body_font, w - LABEL_W) or ["未填写"]
    for index, line in enumerate(lines):
        pen.text((x + LABEL_W, y + index * 30), line, body_font)
    y += len(lines) * 30
    if source:
        for line in wrap(f"来源：{source}", small, w - LABEL_W):
            pen.text((x + LABEL_W, y), line, small, TEXT_SUB)
            y += 24
    return y + 10


def file_table(pen, x, y, w, files, missing):
    """文件清单：最多 TABLE_MAX_ROWS 行（文件名 / 大小 / SHA-256 前 16 位），完整列表在清单 JSON 里。"""
    body_font, small = font(19), font(16)
    if not isinstance(files, list):
        pen.text((x, y), missing, body_font, TEXT_SUB)
        return y + 34
    total = sum(int(item.get("size") or 0) for item in files if isinstance(item, dict))
    pen.text((x, y), f"共 {len(files)} 个，合计 {human_size(total)}", body_font)
    y += 38
    size_w, hash_w = 140, 220
    name_w = w - size_w - hash_w - 24
    pen.text((x, y), "文件名", small, TEXT_SUB)
    pen.text((x + name_w + 12, y), "大小", small, TEXT_SUB)
    pen.text((x + name_w + size_w + 24, y), "SHA-256（前 16 位）", small, TEXT_SUB)
    y += 26
    pen.line([(x, y), (x + w, y)])
    y += 8
    for item in files[:TABLE_MAX_ROWS]:
        item = item if isinstance(item, dict) else {}
        pen.text((x, y), ellipsize(item.get("fileName"), small, name_w), small)
        pen.text((x + name_w + 12, y), human_size(item.get("size")), small)
        pen.text((x + name_w + size_w + 24, y), str(item.get("sha256") or "")[:16], small)
        y += 28
    if len(files) > TABLE_MAX_ROWS:
        pen.text(
            (x, y + 4),
            f"其余 {len(files) - TABLE_MAX_ROWS} 个见 material-manifest.json",
            small,
            TEXT_SUB,
        )
        y += 32
    return y

def render(data):
    canvas = Image.new("RGB", (CANVAS_W, DRAFT_H), BG)
    draw = ImageDraw.Draw(canvas)
    text = lambda key: str(data.get(key) or "").strip()
    version = text("version")
    drama = text("dramaName") or "未命名剧目"

    # ---- 页眉：品牌黑横幅；只有含 AI 生成内容时才显示 AI 徽章 ----
    draw.rectangle([0, 0, CANVAS_W, 132], fill=HEADER_BG)
    draw.text((MARGIN, 36), "AI 制作证明", font=font(44, bold=True), fill=(245, 245, 242))
    if data.get("aiAssisted") is True:
        badge = "由人工智能辅助制作"
        badge_w = int(text_width(badge, font(20))) + 32
        draw.rounded_rectangle(
            [CANVAS_W - MARGIN - badge_w, 48, CANVAS_W - MARGIN, 84], radius=18, fill=ACCENT
        )
        draw.text(
            (CANVAS_W - MARGIN - badge_w // 2, 66), badge, font=font(16),
            fill=(10, 26, 18), anchor="mm",
        )
    y = 172

    def basic(pen, x, top, w):
        top = key_value(pen, x, top, w, "生成日期", f"{text('date')}（北京时间）")
        top = key_value(pen, x, top, w, "制作工具", f"DramaDossier v{version}".strip())
        top = key_value(
            pen, x, top, w, "原剧目", f"{drama}（原始素材 {data.get('originalEpisodes', 0)} 集）"
        )
        return key_value(
            pen, x, top, w, "内容证据", text("evidence") or "未提取（ASR/OCR 未就绪或未启用）"
        )

    y = section(canvas, draw, y, "基本信息", basic)

    def processing(pen, x, top, w):
        lines = data.get("processingLines")
        body_font = font(19)
        if not isinstance(lines, list):
            pen.text((x, top), "未记录（上次深度处理未完成）", body_font, TEXT_SUB)
            return top + 34
        for item in lines:
            wrapped = wrap(str(item), body_font, w - 28)
            for index, line in enumerate(wrapped):
                pen.text((x + 28 if index else x, top), ("· " if not index else "") + line, body_font)
                top += 30
            top += 4
        return top

    y = section(canvas, draw, y, "处理方式", processing)

    def materials(pen, x, top, w):
        synopsis_len = len(text("synopsis"))
        top = key_value(pen, x, top, w, "剧名", text("title"), text("titleSource") or "未生成")
        top = key_value(
            pen, x, top, w, "简介",
            f"见下方简介正文（{synopsis_len} 字）" if synopsis_len else "未填写",
            text("synopsisSource") or "未生成",
        )
        top = key_value(pen, x, top, w, "海报", text("posterName"), text("posterSource") or "未生成")
        count = int(data.get("processCount") or 0)
        if count:
            return key_value(
                pen, x, top, w, "过程图", f"已生成 {count} 张（画面抽自原始素材代表集）",
                text("storyboardSource"),
            )
        return key_value(pen, x, top, w, "过程图", "未生成")

    y = section(canvas, draw, y, "材料明细", materials)

    # 简介正文卡：没有简介时不画（不留空白卡）；海报缩略图跟在右侧，卡高同时容纳两者。
    synopsis = text("synopsis")
    if synopsis:
        poster = load_poster(text("posterPath"))

        def synopsis_card(pen, x, top, w):
            body_font = font(19)
            text_w = w - POSTER_W - 32
            lines = clamp_lines(wrap(synopsis, body_font, text_w), SYNOPSIS_MAX_LINES, body_font, text_w)
            for index, line in enumerate(lines):
                pen.text((x, top + index * 34), line, body_font)
            px = x + w - POSTER_W
            if poster is None:
                pen.box([px, top, px + POSTER_W, top + POSTER_H], fill=(236, 236, 232), outline=CARD_LINE)
                pen.text((px + POSTER_W // 2, top + POSTER_H // 2), "海报未生成", font(16), TEXT_SUB, "mm")
            else:
                pen.paste(poster, (px, top))
                pen.line(
                    [(px, top), (px + POSTER_W, top), (px + POSTER_W, top + POSTER_H),
                     (px, top + POSTER_H), (px, top)]
                )
            return top + max(len(lines) * 34, POSTER_H) + 6

        y = section(canvas, draw, y, "剧情简介", synopsis_card)

    y = section(
        canvas, draw, y, "成片清单",
        lambda pen, x, top, w: file_table(pen, x, top, w, data.get("outputs"), "未记录（上次深度处理未完成）"),
    )
    y = section(
        canvas, draw, y, "原始素材",
        lambda pen, x, top, w: file_table(pen, x, top, w, data.get("sources"), "未记录（素材指纹未能计算）"),
    )

    def declaration(pen, x, top, w):
        body_font = font(19)
        content = text("declaration") or "未记录"
        for line in wrap(content, body_font, w):
            pen.text((x, top), line, body_font)
            top += 34
        return top

    y = section(canvas, draw, y, "AI 声明", declaration)

    # ---- 签署区：全部留空，由制作方手写 ----
    def signing(pen, x, top, w):
        body_font = font(19)
        col = w // 3
        for index, label in enumerate(("制作方", "签署", "日期")):
            cx = x + index * col
            pen.text((cx, top + 6), label, body_font, TEXT_SUB)
            pen.line([(cx + 80, top + 32), (cx + col - 24, top + 32)], fill=TEXT_SUB)
        return top + 44

    y = section(canvas, draw, y, "制作方签署", signing)

    # ---- 按实际内容裁切画布，页脚落在底部 ----
    height = max(MIN_CANVAS_H, y + FOOTER_H)
    canvas = canvas.crop((0, 0, CANVAS_W, height))
    draw = ImageDraw.Draw(canvas)
    draw.line([MARGIN, height - 96, CANVAS_W - MARGIN, height - 96], fill=CARD_LINE, width=1)
    footer = f"本证明由 DramaDossier v{version} 自动生成 · {text('date')}（北京时间）"
    draw.text((MARGIN, height - 70), footer, font=font(16), fill=TEXT_SUB)
    draw.text((CANVAS_W - MARGIN, height - 70), "剧备 DramaDossier", font=font(16), fill=TEXT_SUB, anchor="ra")
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
