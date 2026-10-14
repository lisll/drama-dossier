"""Rust 侧与 Python worker 之间的目录交换协议。

协议由 `crates/dossier-inpaint` 定义，这里把它固化下来，避免两边各写一遍
字面量而慢慢走偏。所有路径都在同一个 `work_dir` 内，方便人工复现与排查。

检测（mode = "detect"）
    输入：in/frame.raw        整帧 bgr24，长度 = width*height*3
          request.json        {"mode","width","height","frameFile","outputFile"}
    输出：boxes.json          {"boxes":[{"x","y","width","height","score"}]}

修复（mode = "inpaint"）
    输入：in/frame_%04d.raw   ROI 尺寸 bgr24，从 frame_0000 连续编号
          mask.pgm            P5 灰度，255 表示需要修复，0 表示保留
          request.json        {"mode","width","height","frameCount","inputDir","outputDir","maskFile"}
    输出：out/frame_%04d.raw  帧数与尺寸必须与输入完全一致

坐标与像素约定：
- 所有矩形都是整数像素、左上角为原点，且必须满足 x+width<=picture_w、y+height<=picture_h；
- 颜色通道顺序统一为 BGR（与 ffmpeg 的 bgr24 对齐），写回时不得改成 RGB。
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Optional


DETECT_MODE = "detect"
INPAINT_MODE = "inpaint"


class ProtocolError(RuntimeError):
    """请求或产物不符合协议。"""


@dataclass
class Request:
    """解析后的 request.json。"""

    mode: str
    width: int
    height: int
    raw: Dict[str, Any] = field(default_factory=dict)

    # ---- detect ----
    @property
    def frame_file(self) -> Path:
        return Path(self._require("frameFile"))

    @property
    def output_file(self) -> Path:
        return Path(self._require("outputFile"))

    # ---- inpaint ----
    @property
    def frame_count(self) -> int:
        value = self.raw.get("frameCount")
        if not isinstance(value, int) or value <= 0:
            raise ProtocolError(f"frameCount 非法：{value!r}")
        return value

    @property
    def input_dir(self) -> Path:
        return Path(self._require("inputDir"))

    @property
    def output_dir(self) -> Path:
        return Path(self._require("outputDir"))

    @property
    def mask_file(self) -> Path:
        return Path(self._require("maskFile"))

    def _require(self, key: str) -> str:
        value = self.raw.get(key)
        if not isinstance(value, str) or not value:
            raise ProtocolError(f"request.json 缺少字段 {key}")
        return value

    @classmethod
    def load(cls, work_dir: Path) -> "Request":
        path = work_dir / "request.json"
        if not path.is_file():
            raise ProtocolError(f"缺少请求文件 {path}")
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
        except json.JSONDecodeError as error:
            raise ProtocolError(f"request.json 不是合法 JSON：{error}") from error
        if not isinstance(payload, dict):
            raise ProtocolError("request.json 顶层必须是对象")

        mode = payload.get("mode")
        if mode not in (DETECT_MODE, INPAINT_MODE):
            raise ProtocolError(f"未知 mode：{mode!r}")

        width = payload.get("width")
        height = payload.get("height")
        for name, value in (("width", width), ("height", height)):
            if not isinstance(value, int) or value <= 0:
                raise ProtocolError(f"{name} 非法：{value!r}")

        return cls(mode=mode, width=width, height=height, raw=payload)


def frame_name(index: int) -> str:
    """与 Rust 侧 `frame_name` 保持一致。"""
    return f"frame_{index:04d}.raw"


def text_box(
    x: int,
    y: int,
    width: int,
    height: int,
    score: float,
    clip_width: int,
    clip_height: int,
) -> Optional[Dict[str, Any]]:
    """构造一个矩形，并裁到画面内；空矩形返回 None。"""
    x0 = max(0, min(int(x), clip_width))
    y0 = max(0, min(int(y), clip_height))
    x1 = max(x0, min(int(x) + int(width), clip_width))
    y1 = max(y0, min(int(y) + int(height), clip_height))
    if x1 <= x0 or y1 <= y0:
        return None
    return {
        "x": x0,
        "y": y0,
        "width": x1 - x0,
        "height": y1 - y0,
        "score": round(float(score), 4),
    }


def write_boxes(work_dir: Path, request: Request, boxes: List[Dict[str, Any]]) -> None:
    """写出 boxes.json；坐标已裁到画面内。"""
    cleaned: List[Dict[str, Any]] = []
    for box in boxes:
        item = text_box(
            box["x"],
            box["y"],
            box["width"],
            box["height"],
            box["score"],
            request.width,
            request.height,
        )
        if item is not None:
            cleaned.append(item)
    payload = {"boxes": cleaned}
    (work_dir / request.output_file).write_text(
        json.dumps(payload, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
