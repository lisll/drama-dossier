# -*- coding: utf-8 -*-
"""过程图排版：复刻 Grove-Max「AI 分镜创作画布」整屏截图样式。

每张过程图 = 一块深色创作画布截图（2048×1190），自上而下：
  顶部窗口栏（logo / 剧名 tab / 「画面 N」下拉 tab / 分享·调色板 / 会员中心 / Agent）
  左上故事梗概（剧名 / 本剧概要）
  右上故事结构卡（三节点进度线 + 「查看完整故事梗概 >」按钮）
  左列「资产」：人物设定图 / 场景图 / 道具图 三张资产卡（真实图像 cover 铺满）
  中列「分镜提示词」：三张提示词卡（画面描述 + 镜头元数据脚注）
  右列「分镜图帧」：三张真实原片帧卡（contain 居中 + 分辨率标注）
  卡片之间以贝塞尔曲线连线；底部资产管理工具栏 + 居中浮动工具条

四张图共用同一版式：顶部 tab 为「画面 N」、中列提示词文案随 N 变化、
右列图帧取自第 N 个代表集的 10% / 40% / 70% 三帧（与抽帧调用方的
帧清单顺序一致：按「代表集 × 步长」排列，每组 3 帧）。

调用方（桌面端深度处理流程）传入：

    python make_process_images.py \
        --output-dir <产物目录> \
        --title <剧名> \
        --synopsis <本剧概要（可空）> \
        --frames <逗号分隔的候选帧，按「代表集 × 步长」顺序> \
        --poster <海报路径（可空）> \
        --labels <竖线分隔帧标签（兼容保留，忽略）> \
        --episode-label / --summaries <历史参数（兼容保留，忽略）>

输出：<output-dir>/过程图_01.png ~ 过程图_04.png，退出码 0；异常时中文报错退出 1。
"""

import argparse
import os
import sys

from PIL import Image, ImageDraw, ImageStat

from cjk_font import FontMissingError, load_cjk_font, save_png_atomic

IMAGE_COUNT = 4
FRAMES_PER_CANVAS = 3
# 亮度阈值：均值低于它视为黑场/转场帧，不作为过程图素材。
MIN_MEAN_BRIGHTNESS = 22.0

# 画布整体尺寸与关键行高（对齐 Grove-Max 深色工作台的观感比例）。
CANVAS_W, CANVAS_H = 2048, 1190
TOPBAR_H = 60
BOTTOM_Y = 1130
ROW_Y = (292, 538, 784)  # 三列卡片共用的三行
CARD_H = 232
COL_ASSET = (140, 592)   # 资产列左右边界
COL_PROMPT = (636, 1272)  # 提示词列左右边界
COL_FRAME = (1336, 1840)  # 图帧列左右边界

# 配色（取自 Grove-Max 深色主题截图）。
BG = (10, 10, 11)
BAR_BG = (16, 16, 18)
CARD_BG = (23, 23, 26)
CHIP_BG = (16, 16, 18)
CARD_BORDER = (38, 38, 42)
LINE = (58, 58, 64)
TEXT_HI = (236, 236, 238)
TEXT_MID = (176, 176, 182)
TEXT_LOW = (128, 128, 134)
TEXT_FAINT = (92, 92, 98)
FRAME_AREA = (8, 8, 9)
GOLD_TEXT = (232, 184, 75)
GOLD_BG = (42, 33, 20)
GOLD_BORDER = (122, 90, 34)
ACCENT_BLUE = (74, 130, 246)

_FONT_CACHE = {}


def fail(message):
    print(f"过程图生成失败：{message}", file=sys.stderr)
    sys.exit(1)


def font(size, bold=False):
    """中文字体（见 cjk_font）；找不到能画汉字的字体时抛 FontMissingError，由 main 报错退出。"""
    key = (size, bold)
    if key not in _FONT_CACHE:
        _FONT_CACHE[key] = load_cjk_font(size, bold)
    return _FONT_CACHE[key]


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


def clip(text, limit):
    """按字符数截断（画布上所有标题都是短文案，无需按像素精确省略）。"""
    text = (text or "").strip() or "未命名剧目"
    return text if len(text) <= limit else text[: limit - 1] + "…"


def wrap_text(draw, text, name_font, max_w):
    """中文逐字换行：measure 每追加一个字符后的宽度。"""
    lines = []
    line = ""
    for char in text:
        if draw.textlength(line + char, font=name_font) <= max_w:
            line += char
        else:
            lines.append(line)
            line = char
    if line:
        lines.append(line)
    return lines or [""]


def cover_crop(image, width, height):
    """cover 铺满：等比放大后中心裁切到目标尺寸。"""
    scale = max(width / image.width, height / image.height)
    resized = image.resize(
        (max(1, round(image.width * scale)), max(1, round(image.height * scale))),
        Image.LANCZOS,
    )
    left = (resized.width - width) // 2
    top = (resized.height - height) // 2
    return resized.crop((left, top, left + width, top + height))


def contain_center(image, width, height):
    """contain 居中：完整显示在给定尺寸内（不裁切），返回贴图坐标与缩放图。"""
    scale = min(width / image.width, height / image.height)
    size = (max(1, round(image.width * scale)), max(1, round(image.height * scale)))
    resized = image.resize(size, Image.LANCZOS)
    left = (width - size[0]) // 2
    top = (height - size[1]) // 2
    return resized, (left, top)


def paste_cover(canvas, image, box, radius=10):
    """把图像 cover 裁切后带圆角贴进 box（左上角坐标 + 宽高）。"""
    x, y, w, h = box
    cropped = cover_crop(image, w, h)
    mask = Image.new("L", (w, h), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, w - 1, h - 1], radius=radius, fill=255)
    canvas.paste(cropped, (x, y), mask)


def bezier(draw, start, end, color=LINE, width=2):
    """两点之间的水平贝塞尔连线（Grove-Max 节点连线观感）。"""
    x0, y0 = start
    x1, y1 = end
    cx = (x0 + x1) / 2
    points = []
    for step in range(25):
        t = step / 24
        mt = 1 - t
        x = mt * mt * x0 + 2 * mt * t * cx + t * t * x1
        y = mt * mt * y0 + 2 * mt * t * y1
        points.append((x, y))
    draw.line(points, fill=color, width=width, joint="curve")


def tri_down(draw, cx, cy, size, color):
    draw.polygon(
        [(cx - size, cy - size // 2), (cx + size, cy - size // 2), (cx, cy + size)],
        fill=color,
    )


def chip(draw, x, y, text, name_font=None, pad=10):
    """深色小徽章（资产卡标题 / 尺寸标注）。返回右边界 x。"""
    name_font = name_font or font(15)
    w = draw.textlength(text, font=name_font) + pad * 2
    h = 26
    draw.rounded_rectangle([x, y, x + w, y + h], radius=6, fill=CHIP_BG)
    draw.text((x + pad, y + h // 2), text, font=name_font, fill=TEXT_MID, anchor="lm")
    return x + w


def chip_right(draw, right, y, text, name_font=None, pad=10):
    name_font = name_font or font(14)
    w = draw.textlength(text, font=name_font) + pad * 2
    chip(draw, right - w, y, text, name_font, pad)


# ---------------------------------------------------------------------------
# 文案：分镜提示词与镜头元数据（按画面序号确定性变化，只描述拍摄技法，
# 不虚构具体剧情，避免与真实帧画面冲突）。
# ---------------------------------------------------------------------------

SHOT = ["固定机位", "手持跟拍", "缓慢推镜", "横移镜头", "低角度仰拍", "过肩视角"]
SCALE = ["中景", "近景", "远景", "特写", "全景", "半身景"]
LIGHT = ["自然侧光", "清晨柔光", "黄昏暖光", "室内灯光", "阴天漫射光", "侧逆光"]
TONE = ["色调偏暖", "色调偏冷", "低饱和灰调", "高对比明暗", "暖冷对比", "自然还原"]
COMP = ["三分法构图", "中心对称构图", "前景框式构图", "留白构图", "纵深引导构图", "对角线构图"]
MOOD = ["生活流质感", "克制叙事氛围", "沉静情绪铺陈", "戏剧张力收敛", "烟火气氛围", "年代生活质感"]


def pick(pool, index):
    return pool[index % len(pool)]


def prompt_text(index):
    return (
        f"画面描述: {pick(SHOT, index // 2)}下的{pick(SCALE, index)}，"
        f"主体保持在画面视觉中心，{pick(LIGHT, index * 2 + 1)}塑造人物立体感，"
        f"背景保留环境纵深与年代生活质感；{pick(TONE, index * 3 + 2)}，"
        f"原生硬字幕原样保留，突出对白张力与{pick(MOOD, index * 5 + 3)}。"
    )


def meta_rows(index):
    return [
        f"景别: {pick(SCALE, index)}　镜头: {pick(SHOT, index // 2)}　光线: {pick(LIGHT, index * 2 + 1)}",
        f"构图: {pick(COMP, index * 5 + 3)}　色调: {pick(TONE, index * 3 + 2)}　氛围: {pick(MOOD, index * 7 + 4)}",
        "画质: 1080P　帧率: 30fps　字幕: 原生硬字幕",
    ]


# ---------------------------------------------------------------------------
# 画布分区绘制
# ---------------------------------------------------------------------------


def draw_top_bar(draw, title, number):
    draw.rectangle([0, 0, CANVAS_W, TOPBAR_H], fill=BAR_BG)
    draw.line([(0, TOPBAR_H), (CANVAS_W, TOPBAR_H)], fill=CARD_BORDER, width=1)

    # logo 圆角块 + 下拉三角
    draw.rounded_rectangle([24, 12, 88, 48], radius=18, fill=(240, 240, 242))
    draw.text((56, 30), "G", font=font(24, True), fill=(20, 20, 22), anchor="mm")
    tri_down(draw, 102, 30, 7, TEXT_LOW)

    # 剧名 tab（未选中）+ 关闭叉
    tab_title = clip(title, 8)
    draw.text((124, 30), tab_title, font=font(22), fill=(150, 150, 156), anchor="lm")
    cross_x = 124 + draw.textlength(tab_title, font=font(22)) + 18
    draw.line([(cross_x - 5, 25), (cross_x + 5, 35)], fill=TEXT_FAINT, width=2)
    draw.line([(cross_x - 5, 35), (cross_x + 5, 25)], fill=TEXT_FAINT, width=2)

    # 「画面 N ▾」选中下拉
    sel_x = cross_x + 34
    sel_w = 168
    draw.rounded_rectangle(
        [sel_x, 10, sel_x + sel_w, 50], radius=10, fill=(30, 30, 34)
    )
    draw.text(
        (sel_x + 20, 30), f"画面 {number}", font=font(22), fill=TEXT_HI, anchor="lm"
    )
    tri_down(draw, sel_x + sel_w - 24, 30, 7, TEXT_LOW)

    # 右侧：分享、调色板、会员中心、Agent
    share_x = 1580
    draw.rounded_rectangle(
        [share_x - 11, 22, share_x + 11, 44], radius=5, outline=TEXT_LOW, width=2
    )
    draw.line([(share_x, 32), (share_x, 12)], fill=TEXT_LOW, width=2)
    draw.polygon(
        [(share_x - 6, 17), (share_x + 6, 17), (share_x, 9)], fill=TEXT_LOW
    )

    palette_x = 1646
    for offset, color in [(-8, (235, 90, 70)), (0, (240, 200, 80)), (8, (90, 160, 240))]:
        draw.ellipse(
            [palette_x + offset - 6, 24, palette_x + offset + 6, 36],
            fill=color,
        )

    gold_x, gold_w = 1696, 204
    draw.rounded_rectangle(
        [gold_x, 10, gold_x + gold_w, 50],
        radius=20,
        fill=GOLD_BG,
        outline=GOLD_BORDER,
        width=1,
    )
    draw.polygon(
        [(gold_x + 26, 36), (gold_x + 34, 24), (gold_x + 42, 36)], fill=GOLD_TEXT
    )
    draw.text(
        (gold_x + 54, 30), "会员中心", font=font(20), fill=GOLD_TEXT, anchor="lm"
    )
    draw.text(
        (gold_x + gold_w - 20, 30), "974", font=font(20), fill=GOLD_TEXT, anchor="rm"
    )

    agent_x = agent_w = None
    agent_x, agent_w = 1916, 104
    draw.rounded_rectangle(
        [agent_x, 10, agent_x + agent_w, 50], radius=20, fill=(30, 30, 34)
    )
    draw.text(
        (agent_x + agent_w // 2, 30), "Agent", font=font(20), fill=TEXT_HI, anchor="mm"
    )


def draw_story_block(draw, title, synopsis):
    x, y = 64, 78
    draw.text((x, y), "故事梗概", font=font(18), fill=TEXT_FAINT, anchor="la")
    y += 36
    draw.text((x, y), "剧名:", font=font(22), fill=TEXT_LOW, anchor="la")
    draw.text((x + 74, y - 3), clip(title, 18), font=font(26, True), fill=TEXT_HI, anchor="la")
    if synopsis:
        y += 44
        draw.text((x, y), "本剧概要:", font=font(20), fill=TEXT_LOW, anchor="la")
        lines = wrap_text(draw, synopsis, font(20), 1170)
        truncated = len(lines) > 3
        for index, line in enumerate(lines[:3]):
            if truncated and index == 2:
                line = line[:-1] + "…" if len(line) > 1 else "…"
            draw.text((x, y + 30 + index * 30), line, font=font(20), fill=TEXT_MID, anchor="la")


def draw_structure_card(draw, title):
    x, y, w, h = COL_FRAME[0], 76, COL_FRAME[1] - COL_FRAME[0], 170
    draw.rounded_rectangle(
        [x, y, x + w, y + h], radius=12, fill=CARD_BG, outline=CARD_BORDER, width=1
    )
    draw.text((x + w / 2, y + 26), clip(title, 12), font=font(20), fill=TEXT_MID, anchor="mm")

    line_y = y + 86
    dot_xs = (x + 70, x + w / 2, x + w - 70)
    draw.line([(dot_xs[0], line_y), (dot_xs[2], line_y)], fill=(52, 52, 58), width=2)
    for index, dot_x in enumerate(dot_xs):
        draw.ellipse([dot_x - 5, line_y - 5, dot_x + 5, line_y + 5], fill=(120, 120, 126))
        label = ("剧情介绍", "素材资产", "分镜图帧")[index]
        draw.text((dot_x, line_y + 16), label, font=font(14), fill=TEXT_FAINT, anchor="ma")

    btn_w, btn_h = 236, 36
    btn_x, btn_y = x + (w - btn_w) / 2, y + h - btn_h - 12
    draw.rounded_rectangle(
        [btn_x, btn_y, btn_x + btn_w, btn_y + btn_h], radius=8, fill=(34, 34, 38)
    )
    draw.text(
        (btn_x + btn_w / 2, btn_y + btn_h / 2),
        "查看完整故事梗概 >",
        font=font(17),
        fill=(150, 150, 156),
        anchor="mm",
    )


def draw_connections():
    """连线画在卡片之下：资产→提示词、提示词→图帧、梗概→结构卡。"""
    layer = Image.new("RGB", (CANVAS_W, CANVAS_H), BG)
    draw = ImageDraw.Draw(layer)
    for row_y in ROW_Y:
        cy = row_y + CARD_H // 2
        bezier(draw, (COL_ASSET[1], cy), (COL_PROMPT[0], cy))
        bezier(draw, (COL_PROMPT[1], cy), (COL_FRAME[0], cy))
    bezier(draw, (1240, 140), (COL_FRAME[0], 140))
    return layer


def draw_assets_column(canvas, draw, poster, scene, prop):
    draw.text((COL_ASSET[0], 258), "资产", font=font(20), fill=TEXT_LOW, anchor="la")
    cards = [
        (poster or scene, "人物设定图"),
        (scene, "场景图"),
        (prop, "道具图"),
    ]
    for index, (image, label) in enumerate(cards):
        y = ROW_Y[index]
        x0, x1 = COL_ASSET
        draw.rounded_rectangle(
            [x0, y, x1, y + CARD_H], radius=10, fill=CARD_BG, outline=CARD_BORDER, width=1
        )
        if image is not None:
            paste_cover(canvas, image, (x0 + 1, y + 1, x1 - x0 - 2, CARD_H - 2))
        chip(draw, x0 + 10, y + 10, label)
        chip_right(draw, x1 - 10, y + 10, "2048 × 1152")


def draw_prompts_column(draw, number):
    draw.text((COL_PROMPT[0], 258), "分镜提示词", font=font(20), fill=TEXT_LOW, anchor="la")
    x0, x1 = COL_PROMPT
    width = x1 - x0
    for row in range(FRAMES_PER_CANVAS):
        index = (number - 1) * FRAMES_PER_CANVAS + row
        y = ROW_Y[row]
        draw.rounded_rectangle(
            [x0, y, x1, y + CARD_H], radius=10, fill=CARD_BG, outline=CARD_BORDER, width=1
        )
        # 卡头：菱形 + 标题
        head_y = y + 26
        diamond = 6
        draw.polygon(
            [(x0 + 20, head_y - diamond), (x0 + 20 + diamond, head_y),
             (x0 + 20, head_y + diamond), (x0 + 20 - diamond, head_y)],
            outline=TEXT_LOW, width=1,
        )
        draw.text(
            (x0 + 38, head_y), f"分镜提示词 #{row + 1}", font=font(20), fill=TEXT_HI, anchor="lm"
        )
        # 正文：画面描述（最多 4 行，超出省略）
        body_font = font(17)
        lines = wrap_text(draw, prompt_text(index), body_font, width - 44)
        truncated = len(lines) > 4
        for line_index, line in enumerate(lines[:4]):
            if truncated and line_index == 3:
                line = line[:-1] + "…" if len(line) > 1 else "…"
            draw.text(
                (x0 + 22, y + 52 + line_index * 25), line, font=body_font, fill=TEXT_MID, anchor="la"
            )
        # 脚注：镜头元数据
        divider_y = y + CARD_H - 82
        draw.line([(x0 + 22, divider_y), (x1 - 22, divider_y)], fill=(32, 32, 36), width=1)
        meta_font = font(14)
        for meta_index, meta in enumerate(meta_rows(index)):
            draw.text(
                (x0 + 22, divider_y + 12 + meta_index * 21),
                meta,
                font=meta_font,
                fill=TEXT_LOW,
                anchor="la",
            )


def draw_frames_column(canvas, draw, frames, number):
    draw.text((COL_FRAME[0], 258), "分镜图帧", font=font(20), fill=TEXT_LOW, anchor="la")
    x0, x1 = COL_FRAME
    for row in range(FRAMES_PER_CANVAS):
        y = ROW_Y[row]
        draw.rounded_rectangle(
            [x0, y, x1, y + CARD_H], radius=10, fill=CARD_BG, outline=CARD_BORDER, width=1
        )
        # 卡头：相机图标 + 标题 + 真实分辨率
        head_y = y + 24
        draw.rounded_rectangle(
            [x0 + 16, head_y - 7, x0 + 40, head_y + 7], radius=3, outline=TEXT_LOW, width=2
        )
        draw.ellipse([x0 + 24, head_y - 3, x0 + 32, head_y + 3], outline=TEXT_LOW, width=2)
        draw.text(
            (x0 + 52, head_y), f"分镜图帧 #{row + 1}", font=font(18), fill=TEXT_MID, anchor="lm"
        )
        if frames:
            fw, fh = frames[min(row, len(frames) - 1)].size
            draw.text(
                (x1 - 16, head_y), f"{fw} × {fh}", font=font(14), fill=TEXT_FAINT, anchor="rm"
            )
        # 帧区：contain 完整显示真实原片帧（竖屏帧两侧留黑边，与参考一致）
        area = (x0 + 10, y + 38, x1 - x0 - 20, CARD_H - 48)
        draw.rounded_rectangle(
            [area[0], area[1], area[0] + area[2], area[1] + area[3]],
            radius=6,
            fill=FRAME_AREA,
        )
        frame = frames[min(row, len(frames) - 1)] if frames else None
        if frame is not None:
            resized, (left, top) = contain_center(frame, area[2] - 2, area[3] - 2)
            canvas.paste(
                resized,
                (round(area[0] + 1 + left), round(area[1] + 1 + top)),
            )


def draw_bottom_bar(draw):
    draw.rectangle([0, BOTTOM_Y, CANVAS_W, CANVAS_H], fill=BAR_BG)
    draw.line([(0, BOTTOM_Y), (CANVAS_W, BOTTOM_Y)], fill=CARD_BORDER, width=1)

    # 左侧：资产管理 + 工具位 + 缩放
    grid_x, grid_y = 30, BOTTOM_Y + 30
    for dx in (0, 10):
        for dy in (0, 10):
            draw.rectangle([grid_x + dx, grid_y - 8 + dy, grid_x + dx + 7, grid_y - 1 + dy], fill=TEXT_LOW)
    draw.text((64, BOTTOM_Y + 30), "资产管理", font=font(20), fill=TEXT_MID, anchor="lm")
    for index in range(4):
        icon_x = 190 + index * 44
        draw.ellipse(
            [icon_x, BOTTOM_Y + 16, icon_x + 28, BOTTOM_Y + 44],
            outline=(56, 56, 62),
            width=2,
        )
        draw.ellipse([icon_x + 11, BOTTOM_Y + 27, icon_x + 17, BOTTOM_Y + 33], fill=(56, 56, 62))
    draw.text((392, BOTTOM_Y + 30), "30%", font=font(20), fill=TEXT_MID, anchor="lm")

    # 居中浮动工具条（选中相机位带蓝色指示点）
    pill_x0, pill_x1 = CANVAS_W / 2 - 260, CANVAS_W / 2 + 260
    draw.rounded_rectangle(
        [pill_x0, BOTTOM_Y + 10, pill_x1, BOTTOM_Y + 54],
        radius=22,
        fill=(30, 30, 34),
        outline=CARD_BORDER,
        width=1,
    )
    for index in range(9):
        icon_x = pill_x0 + 34 + index * 57
        active = index == 4
        draw.ellipse(
            [icon_x - 9, BOTTOM_Y + 22, icon_x + 9, BOTTOM_Y + 40],
            outline=(140, 140, 146) if active else (70, 70, 76),
            width=2 if active else 1,
        )
        if active:
            draw.ellipse(
                [icon_x + 6, BOTTOM_Y + 14, icon_x + 12, BOTTOM_Y + 20], fill=ACCENT_BLUE
            )


def build_canvas(number, title, synopsis, frames, poster, scene, prop):
    canvas = draw_connections()
    draw = ImageDraw.Draw(canvas)
    draw_top_bar(draw, title, number)
    draw_story_block(draw, title, synopsis)
    draw_structure_card(draw, title)
    draw_assets_column(canvas, draw, poster, scene, prop)
    draw_prompts_column(draw, number)
    draw_frames_column(canvas, draw, frames, number)
    draw_bottom_bar(draw)
    return canvas


def group_frames(paths):
    """把「代表集 × 步长」顺序的帧清单按每 3 帧分组，过滤坏帧/黑场。"""
    slots = [
        paths[i : i + FRAMES_PER_CANVAS] for i in range(0, len(paths), FRAMES_PER_CANVAS)
    ]
    usable = []
    for slot in slots:
        frames = [
            frame
            for frame in (load_frame(path) for path in slot)
            if frame is not None and mean_brightness(frame) >= MIN_MEAN_BRIGHTNESS
        ]
        usable.append(frames)
    pool = [frame for frames in usable for frame in frames]
    if not pool:
        return []
    grouped = []
    for index, frames in enumerate(usable):
        if not frames:
            frames = [pool[index % len(pool)]]
        grouped.append(frames[:FRAMES_PER_CANVAS])
    return grouped


def main():
    parser = argparse.ArgumentParser(description="生成 4 张过程图（Grove-Max 分镜画布截图样式）")
    parser.add_argument("--output-dir", required=True)
    parser.add_argument("--title", default="")
    parser.add_argument("--synopsis", default="")
    parser.add_argument("--frames", required=True, help="逗号分隔的候选帧图路径")
    parser.add_argument("--poster", default="")
    parser.add_argument("--labels", default="", help="兼容保留，忽略")
    parser.add_argument("--episode-label", default="", help="兼容保留，忽略")
    parser.add_argument("--summaries", default="", help="兼容保留，忽略")
    args = parser.parse_args()

    os.makedirs(args.output_dir, exist_ok=True)
    paths = [path for path in args.frames.split(",") if path.strip()]
    if not paths:
        fail("未提供任何帧图路径")

    grouped = group_frames(paths)
    if not grouped:
        fail("可用的候选帧不足（全部缺失或为黑场）")

    poster = load_frame(args.poster) if args.poster else None
    # 资产卡四图共用：场景/道具取自第 1、2 个代表集的中位帧（无第二组时复用第一组）。
    scene = grouped[0][len(grouped[0]) // 2]
    prop = grouped[1][len(grouped[1]) // 2] if len(grouped) > 1 else grouped[0][0]

    # 先把 4 张都画好再写盘：缺中文字体时在画第一张就报错，不会只写出一部分。
    canvases = []
    try:
        for index in range(IMAGE_COUNT):
            frames = list(grouped[index % len(grouped)])
            fill = 0
            while len(frames) < FRAMES_PER_CANVAS:
                frames.append(grouped[index % len(grouped)][fill % len(grouped[index % len(grouped)])])
                fill += 1
            canvases.append(
                build_canvas(
                    index + 1, args.title, args.synopsis, frames[:FRAMES_PER_CANVAS], poster, scene, prop
                )
            )
    except FontMissingError as error:
        fail(str(error))
    for index, canvas in enumerate(canvases):
        out_path = os.path.join(args.output_dir, f"过程图_{index + 1:02d}.png")
        try:
            save_png_atomic(canvas, out_path)
        except OSError as error:
            fail(f"写入 {out_path} 失败：{error}")
        print(f"已生成 {out_path}")


if __name__ == "__main__":
    main()
