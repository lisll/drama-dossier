"""把 Rust 给的排版模型（`SealedDocument`）画成 A4 PDF，嵌入中文字体。

- 字体：配置项 `DRAMA_COMMERCE_PDF_FONT` → 常见系统路径（Noto Sans CJK SC）。`.ttc` 合集里
  取简体中文那一款，存成单独的字体文件交给 fpdf2（它只认文件路径）。fpdf2 会做子集化，
  嵌进 PDF 的只有用到的字，一份文档十几 KiB。
- 找不到字体时 [`load_font`] 抛 ConfigError，服务拒绝启动：不生成一份方块字的 PDF。
- 正文写到 `contentBottomMm` 为止，下面留给签章位；写超了就报错，不让正文压到章上。
"""

from __future__ import annotations

import atexit
import io
import os
import shutil
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

from fpdf import FPDF

from ..config import ConfigError

#: 没配置时按这个顺序找。都是 SIL OFL 许可的字体，可以嵌入 PDF。
DEFAULT_FONT_PATHS = (
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf",
    "/usr/share/fonts/noto-cjk/NotoSansSC-Regular.otf",
)
_PREFERRED_FAMILIES = ("Noto Sans CJK SC", "Noto Sans SC", "Source Han Sans SC", "Source Han Sans CN")

PAGE_WIDTH_MM = 210.0
MARGIN_MM = 25.0
FONT_FAMILY = "cjk"


@dataclass(frozen=True)
class Font:
    #: 交给 fpdf2 的单个字体文件。
    path: str
    family: str


def load_font(configured: Optional[str]) -> Font:
    candidates = [configured] if configured else list(DEFAULT_FONT_PATHS)
    for candidate in candidates:
        path = Path(candidate)
        if path.is_file():
            return _prepare(path)
    if configured:
        raise ConfigError(f"找不到 DRAMA_COMMERCE_PDF_FONT 指定的字体文件：{configured}")
    raise ConfigError(
        "找不到报审包 PDF 要用的中文字体。请安装 Noto Sans CJK（Debian/Ubuntu：apt install fonts-noto-cjk），"
        "或用 DRAMA_COMMERCE_PDF_FONT 指定一个 .otf / .ttf / .ttc 字体文件"
    )


def _prepare(path: Path) -> Font:
    from fontTools.ttLib import TTCollection, TTFont, TTLibError

    try:
        if path.suffix.lower() == ".ttc":
            faces = TTCollection(str(path)).fonts
            face = _pick_face(faces, path)
        else:
            face = TTFont(str(path))
    except (TTLibError, OSError) as error:
        raise ConfigError(f"{path} 不是可用的字体文件（{type(error).__name__}）") from error
    family = face["name"].getDebugName(1) or path.stem
    if path.suffix.lower() != ".ttc":
        return Font(str(path), family)
    # 合集里的一款字存成单独文件：放在只有本进程能读的临时目录，进程退出时删掉。
    workdir = tempfile.mkdtemp(prefix="dossier-font-")
    atexit.register(shutil.rmtree, workdir, True)
    target = os.path.join(workdir, "font.otf")
    buffer = io.BytesIO()
    face.save(buffer)
    with open(target, "wb") as handle:
        handle.write(buffer.getvalue())
    return Font(target, family)


def _pick_face(faces, path: Path):
    by_name = {face["name"].getDebugName(1): face for face in faces}
    for family in _PREFERRED_FAMILIES:
        if family in by_name:
            return by_name[family]
    if len(faces) == 1:
        return faces[0]
    raise ConfigError(f"{path} 是字体合集，但里面没有简体中文字体（{', '.join(sorted(map(str, by_name)))}）")


class LayoutError(ValueError):
    """正文太长，压到了签章位。"""


def new_pdf(font: Font) -> FPDF:
    pdf = FPDF(unit="mm", format="A4")
    pdf.set_margins(MARGIN_MM, MARGIN_MM, MARGIN_MM)
    pdf.set_auto_page_break(False)
    pdf.add_font(FONT_FAMILY, "", font.path)
    pdf.set_creator("drama-dossier")
    return pdf


def render_sealed_document(document: dict, font: Font, *, footer: str) -> bytes:
    """按排版模型画一份 PDF。签章位上只写说明文字（「盖章处」），章由 Signer 叠加上去。"""
    pdf = new_pdf(font)
    pdf.add_page()
    width = PAGE_WIDTH_MM - 2 * MARGIN_MM
    pdf.set_font(FONT_FAMILY, size=18)
    pdf.set_text_color(20)
    pdf.cell(width, 12, document["title"], align="C", new_x="LMARGIN", new_y="NEXT")
    pdf.ln(4)
    for block in document["blocks"]:
        kind = block["type"]
        if kind == "note":
            pdf.set_font(FONT_FAMILY, size=9.5)
            pdf.set_text_color(110)
            pdf.multi_cell(width, 5.5, block["text"], new_x="LMARGIN", new_y="NEXT")
        elif kind == "paragraph":
            pdf.set_font(FONT_FAMILY, size=11.5)
            pdf.set_text_color(20)
            pdf.multi_cell(width, 7.5, block["text"], new_x="LMARGIN", new_y="NEXT")
        elif kind == "table":
            pdf.set_font(FONT_FAMILY, size=11)
            pdf.set_text_color(20)
            with pdf.table(col_widths=(35, width - 35), text_align="LEFT", first_row_as_headings=False,
                           line_height=7.5) as table:
                for label, value in block["rows"]:
                    row = table.row()
                    row.cell(label)
                    row.cell(value)
        else:
            raise LayoutError(f"不认识的内容块：{kind}")
        pdf.ln(3)
    if pdf.get_y() > float(document["contentBottomMm"]):
        raise LayoutError("文档内容太长，会压到签章位，请缩短填写的内容")
    pdf.set_font(FONT_FAMILY, size=9)
    pdf.set_text_color(90)
    for slot in document["slots"]:
        pdf.set_xy(slot["xMm"], slot["yMm"] - 6)
        pdf.cell(slot["widthMm"], 5, slot["label"], align="C")
    pdf.set_font(FONT_FAMILY, size=8)
    pdf.set_text_color(140)
    pdf.set_xy(MARGIN_MM, 285)
    pdf.cell(width, 4, footer, align="C")
    return bytes(pdf.output())
