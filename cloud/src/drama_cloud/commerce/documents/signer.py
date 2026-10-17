"""签章接口（总方案 2.1 节，M0 调研报告 2.3 节）。

三家电子签章服务的流程都是异步的：发起签署 → 签署人在网页上签 → 服务商回调 → 取回已签文件。
所以接口是「发起、回调、取回」三步；占位章和上传扫描件这两种实现在第一步就直接完成。

- [`MockSigner`]：一眼能认出的占位图形。每个签章位画灰色虚线框，框内写「样章 · 未签署 · 无法律效力」，
  签名位写「签名占位」；整页加「草稿 · 未签署」斜向水印。**不画红色圆章**：逼真的假章一旦被误交
  或流出，就可能被当作真章使用。
- [`ScanUploadSigner`]：用使用者上传的整页盖章扫描件替换对应页面。章的真伪程序不核验，由使用者负责。
- 真实服务商（e签宝、法大大、契约锁）只按这个协议留位置，本期不写实现，所以不存在假的「签署成功」路径。
"""

from __future__ import annotations

import io
from dataclasses import dataclass, field
from enum import Enum
from typing import Dict, List, Literal, Mapping, Optional, Protocol

from fpdf import FPDF
from PIL import Image, UnidentifiedImageError
from pypdf import PdfReader, PdfWriter
from pypdf.errors import PdfReadError

from .pdf import FONT_FAMILY, Font, new_pdf

#: 单份扫描件的上限，和接口的请求体上限一致。
MAX_SCAN_BYTES = 5 * 1024 * 1024
#: 扫描图片的像素上限：A4 按 300 dpi 扫约 2480 × 3508，留足余量，挡住「解压炸弹」。
MAX_SCAN_PIXELS = 40_000_000


class SignState(str, Enum):
    COMPLETED = "completed"  # 已出结果（占位章、扫描件当场完成）
    PENDING = "pending"  # 等签署人在网页上签（真实服务商才会有）
    FAILED = "failed"


@dataclass(frozen=True)
class SealSlot:
    page: int  # 从 1 开始的页码
    x_mm: float  # 签章框左上角的坐标和大小，单位毫米
    y_mm: float
    width_mm: float
    height_mm: float
    kind: Literal["company_seal", "signature"]

    @classmethod
    def from_model(cls, slot: dict) -> "SealSlot":
        return cls(int(slot["page"]), float(slot["xMm"]), float(slot["yMm"]), float(slot["widthMm"]),
                   float(slot["heightMm"]), slot["kind"])


@dataclass(frozen=True)
class SignRequest:
    document_id: str  # 包里的路径，例如 04_成本配置表/成本配置表.pdf
    pdf: bytes  # 待签的 PDF
    slots: List[SealSlot]
    signer_name: str  # 公司名或签名人，只用于展示


@dataclass(frozen=True)
class SignResult:
    state: SignState
    flow_id: str
    signed_pdf: Optional[bytes] = None  # 只在 COMPLETED 时有值
    sign_url: Optional[str] = None  # 只在 PENDING 时有值
    is_placeholder: bool = False  # True 表示占位章，不具法律效力


class SignerError(ValueError):
    """签章失败。code 给接口层转成错误码。"""

    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code


class Signer(Protocol):
    #: 写进清单的签章方式：mock / scan /（以后的服务商名）。
    name: str

    def start(self, request: SignRequest) -> SignResult: ...

    def verify_callback(self, headers: Mapping[str, str], body: bytes) -> str:
        """验签通过就返回 flow_id；不通过就抛异常，不返回任何结果。"""
        ...

    def fetch(self, flow_id: str) -> SignResult: ...


def _no_async(name: str):
    raise SignerError("not_supported", f"{name} 当场完成签章，没有回调和异步取回")


def _overlay_pages(original: bytes, overlay: bytes) -> bytes:
    """把 overlay 的第 N 页叠到 original 的第 N 页上。"""
    base = PdfReader(io.BytesIO(original))
    stamp = PdfReader(io.BytesIO(overlay))
    writer = PdfWriter()
    for index, page in enumerate(base.pages):
        if index < len(stamp.pages):
            page.merge_page(stamp.pages[index])
        writer.add_page(page)
    out = io.BytesIO()
    writer.write(out)
    return out.getvalue()


@dataclass
class MockSigner:
    """占位章：灰色虚线框 + 「样章 · 未签署 · 无法律效力」+ 整页斜向水印。"""

    font: Font
    name: str = field(default="mock", init=False)

    def start(self, request: SignRequest) -> SignResult:
        page_count = len(PdfReader(io.BytesIO(request.pdf)).pages)
        overlay = new_pdf(self.font)
        for page in range(1, page_count + 1):
            overlay.add_page()
            self._watermark(overlay)
            for slot in (s for s in request.slots if s.page == page):
                self._slot(overlay, slot)
        signed = _overlay_pages(request.pdf, bytes(overlay.output()))
        return SignResult(SignState.COMPLETED, f"mock:{request.document_id}", signed_pdf=signed,
                          is_placeholder=True)

    @staticmethod
    def _watermark(pdf: FPDF) -> None:
        pdf.set_font(FONT_FAMILY, size=44)
        pdf.set_text_color(215)
        with pdf.rotation(35, x=105, y=150):
            pdf.text(38, 160, "草稿 · 未签署")

    @staticmethod
    def _slot(pdf: FPDF, slot: SealSlot) -> None:
        pdf.set_draw_color(128)
        pdf.set_line_width(0.4)
        pdf.set_dash_pattern(dash=2, gap=1.5)
        pdf.rect(slot.x_mm, slot.y_mm, slot.width_mm, slot.height_mm)
        pdf.set_dash_pattern()
        pdf.set_text_color(110)
        pdf.set_font(FONT_FAMILY, size=10)
        text = "样章 · 未签署\n无法律效力" if slot.kind == "company_seal" else "签名占位\n未签署"
        pdf.set_xy(slot.x_mm, slot.y_mm + slot.height_mm / 2 - 6)
        pdf.multi_cell(slot.width_mm, 6, text, align="C")

    def verify_callback(self, headers: Mapping[str, str], body: bytes) -> str:
        _no_async("占位章")

    def fetch(self, flow_id: str) -> SignResult:
        _no_async("占位章")


@dataclass
class ScanUploadSigner:
    """上传已经盖好真章的整页扫描件，原样替换对应页面。`scans`：文档路径 → 扫描件字节。

    支持**混合签章**：`scans` 里有的文档用扫描件，没有的仍走占位章
    （用户只上传了其中一份时，另一份不该因此整批失败）。
    占位的那份同样带水印、文件名加 `_未签署` 后缀，调用方据此逐份标明签章方式。
    """

    font: Font
    scans: Dict[str, bytes]
    name: str = field(default="scan", init=False)

    def start(self, request: SignRequest) -> SignResult:
        scan = self.scans.get(request.document_id)
        if scan is None:
            # 没给这份的扫描件：退回占位章，而不是报错让整个报审包生成失败。
            return MockSigner(self.font).start(request)
        original = PdfReader(io.BytesIO(request.pdf))
        replacement = scan_to_pdf(scan, self.font)
        pages = PdfReader(io.BytesIO(replacement)).pages
        if len(pages) != len(original.pages):
            raise SignerError(
                "scan_invalid",
                f"{request.document_id} 有 {len(original.pages)} 页，扫描件有 {len(pages)} 页，页数要一致",
            )
        return SignResult(SignState.COMPLETED, f"scan:{request.document_id}", signed_pdf=replacement,
                          is_placeholder=False)

    def verify_callback(self, headers: Mapping[str, str], body: bytes) -> str:
        _no_async("扫描件签章")

    def fetch(self, flow_id: str) -> SignResult:
        _no_async("扫描件签章")


def scan_to_pdf(data: bytes, font: Font) -> bytes:
    """扫描件转成 PDF。按文件头判断类型（不信扩展名）：PDF 原样检查后返回，PNG / JPEG 铺满一页 A4。"""
    if len(data) > MAX_SCAN_BYTES:
        raise SignerError("scan_invalid", "扫描件不能超过 5 MB")
    if data.startswith(b"%PDF-"):
        try:
            reader = PdfReader(io.BytesIO(data))
            if reader.is_encrypted or not reader.pages:
                raise SignerError("scan_invalid", "扫描件 PDF 不能加密，也不能是空的")
        except PdfReadError as error:
            raise SignerError("scan_invalid", "扫描件 PDF 打不开") from error
        writer = PdfWriter(clone_from=reader)
        out = io.BytesIO()
        writer.write(out)
        return out.getvalue()
    if not (data.startswith(b"\x89PNG\r\n\x1a\n") or data.startswith(b"\xff\xd8\xff")):
        raise SignerError("scan_invalid", "扫描件只支持 PNG、JPEG 或 PDF")
    try:
        with Image.open(io.BytesIO(data)) as image:
            width, height = image.size
            if width * height > MAX_SCAN_PIXELS:
                raise SignerError("scan_invalid", "扫描件分辨率太高，请用 300 dpi 以内扫描")
            image.verify()
    except (UnidentifiedImageError, OSError, Image.DecompressionBombError) as error:
        raise SignerError("scan_invalid", "扫描件图片打不开") from error
    pdf = new_pdf(font)
    pdf.add_page()
    pdf.image(io.BytesIO(data), x=0, y=0, w=210, h=297)
    return bytes(pdf.output())
