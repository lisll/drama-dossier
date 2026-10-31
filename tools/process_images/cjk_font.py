# -*- coding: utf-8 -*-
"""排版脚本共用：中文字体查找与 PNG 原子保存。

三个排版脚本（AI 制作证明、AI 合规角标、过程图）都要画中文。以前找不到字体时退回
Pillow 内置默认字体：它没有汉字，每个汉字都画成同一个方块，脚本却照常「成功」，
证明图看不清、角标烧进成片也看不清。现在找不到能画汉字的字体就直接报错，交给调用方
（证明退回文本版、角标让深度处理在编码前停下），不再产出看不清的图。

本文件随安装包与其他脚本放在同一目录；Python 运行脚本时会把脚本所在目录加进
模块搜索路径，所以直接 `from cjk_font import …` 即可。
"""

import os
import tempfile

from PIL import ImageFont

# 候选字体：常规体在前，粗体单列。路径来自各系统默认安装位置；
# 找不到的跳过，全部找不到时报错（不退回默认字体）。
_REGULAR = [
    # macOS：苹方在 10.11 起位于 /System/Library/Fonts；更早或精简系统退回黑体 / 冬青黑体
    "/System/Library/Fonts/PingFang.ttc",
    "/System/Library/Fonts/STHeiti Light.ttc",
    "/System/Library/Fonts/STHeiti Medium.ttc",
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    "/Library/Fonts/Arial Unicode.ttf",
    "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
    # Windows：微软雅黑 → 黑体 → 宋体（英文版 Windows 默认也装雅黑）
    "C:/Windows/Fonts/msyh.ttc",
    "C:/Windows/Fonts/msyh.ttf",
    "C:/Windows/Fonts/simhei.ttf",
    "C:/Windows/Fonts/simsun.ttc",
    # Linux 开发机（发行包只面向 Windows / macOS）
    "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
]
_BOLD = [
    "/System/Library/Fonts/PingFang.ttc",
    "/System/Library/Fonts/STHeiti Medium.ttc",
    "C:/Windows/Fonts/msyhbd.ttc",
    "C:/Windows/Fonts/msyhbd.ttf",
    "C:/Windows/Fonts/simhei.ttf",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc",
]

# 用来判断字体能不能画汉字的探针字：字形必须互不相同，且与「缺字」方块不同。
_PROBE_CHARS = "中剧证"


class FontMissingError(RuntimeError):
    """找不到能画汉字的字体。"""


def _draws_chinese(face):
    """字体真的含这几个汉字：每个字都有笔画，且三个字的字形互不相同（缺字时都画成同一个方块）。"""
    masks = []
    for char in _PROBE_CHARS:
        mask = face.getmask(char)
        if mask.getbbox() is None:
            return False
        masks.append((mask.size, bytes(mask)))
    return len(set(masks)) == len(masks)


def candidate_paths(bold=False):
    """查找顺序：粗体先找粗体候选，再退回常规候选。

    环境变量 `DOSSIER_CJK_FONT_CANDIDATES`（系统路径分隔符分隔）存在时只用它列出的路径，
    供测试模拟「找不到字体」或指定字体；发行版里没有任何地方设置它。
    """
    override = os.environ.get("DOSSIER_CJK_FONT_CANDIDATES")
    if override is not None:
        return [path for path in override.split(os.pathsep) if path]
    return (_BOLD + _REGULAR) if bold else list(_REGULAR)


def load_cjk_font(size, bold=False, candidates=None):
    """返回能画汉字的 FreeType 字体；一个都找不到时抛 FontMissingError（带上找过的路径）。"""
    paths = list(candidates) if candidates is not None else candidate_paths(bold)
    for path in paths:
        if not os.path.isfile(path):
            continue
        try:
            face = ImageFont.truetype(path, size)
        except OSError:
            continue
        if _draws_chinese(face):
            return face
    raise FontMissingError(
        "找不到可显示中文的字体（已查找："
        + "、".join(paths)
        + "）。请安装系统中文字体（如微软雅黑、苹方）后重试"
    )


def save_png_atomic(image, path):
    """先存到同目录的独占临时文件，再改名替换目标。

    写到一半失败时旧文件不受影响；临时文件用 mkstemp 独占创建，不会顺着输出目录里
    预先放好的符号链接写到别处；改名替换的是路径本身，目标是链接时换掉的是链接。
    """
    directory = os.path.dirname(os.path.abspath(path)) or "."
    fd, temp = tempfile.mkstemp(prefix=".", suffix=".png.tmp", dir=directory)
    try:
        with os.fdopen(fd, "wb") as handle:
            image.save(handle, "PNG")
        os.replace(temp, path)
    except BaseException:
        try:
            os.remove(temp)
        except OSError:
            pass
        raise


__all__ = [
    "FontMissingError",
    "candidate_paths",
    "load_cjk_font",
    "save_png_atomic",
]
