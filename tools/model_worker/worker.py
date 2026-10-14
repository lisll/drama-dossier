#!/usr/bin/env python3
"""dossier-inpaint 的模型 worker：DBNet 检测 + E2FGVI 修复。

调用方式（work_dir 必须是最后一个参数）：

    python3 worker.py [选项] <work_dir>

`work_dir` 内的交换协议见 `protocol.py` 顶部说明；本脚本只做两件事：
读请求 → 跑模型 → 按协议写产物。掩码合成、ROI 裁剪、帧回贴与羽化全部由
Rust 侧 `dossier-inpaint` 负责，因此这里不做任何「贴回去」的判断。

引擎选择：
- 检测：`--detect-engine auto|dbnet|bright-rows`
  - `dbnet`：ONNX Runtime + DBNet 概率图（正式方案）
  - `bright-rows`：纯 numpy 的高亮行启发式，仅用于在没有模型时验证协议
- 修复：`--inpaint-engine auto|e2fgvi|column`
  - `e2fgvi`：时序视频 inpainting（需要 torch + 权重）
  - `column`：逐列中位数基线，与 Rust 侧同语义

`auto` 会用真实模型，缺失依赖时降级到基线并在 stderr 打印告警；
显式指定 `dbnet` / `e2fgvi` 时依赖缺失即报错退出，不做静默降级。
"""

from __future__ import annotations

import argparse
import sys
import traceback
from pathlib import Path
from typing import List, Optional

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))

import inpaint_backends  # noqa: E402
from protocol import (  # noqa: E402
    DETECT_MODE,
    INPAINT_MODE,
    ProtocolError,
    Request,
    frame_name,
    text_box,
    write_boxes,
)

EXIT_PROTOCOL = 2
EXIT_BACKEND = 3


DEFAULT_DBNET_MODEL = Path(__file__).resolve().parent / "models" / "ch_PP-OCRv4_det_infer.onnx"


def default_dbnet_model() -> Optional[str]:
    """未显式指定权重时，使用 `models/` 下的约定路径（fetch_models.sh 会放那里）。"""
    return str(DEFAULT_DBNET_MODEL) if DEFAULT_DBNET_MODEL.is_file() else None


def warn(message: str) -> None:
    print(f"[worker] 警告：{message}", file=sys.stderr)


def load_raw_frame(path: Path, width: int, height: int) -> np.ndarray:
    """读取 bgr24 裸帧，返回 (H, W, 3) uint8。"""
    expected = width * height * 3
    if not path.is_file():
        raise ProtocolError(f"缺少帧文件 {path}")
    data = path.read_bytes()
    if len(data) != expected:
        raise ProtocolError(f"{path} 长度应为 {expected} 字节，实际 {len(data)}")
    return np.frombuffer(data, dtype=np.uint8).reshape(height, width, 3).copy()


def load_mask(path: Path, width: int, height: int) -> np.ndarray:
    """读取 P5 掩码（非 0 视为需要修复）。"""
    if not path.is_file():
        raise ProtocolError(f"缺少掩码文件 {path}")
    raw = path.read_bytes()
    fields: List[bytes] = []
    position = 0
    while len(fields) < 4 and position < len(raw):
        while position < len(raw) and raw[position : position + 1].isspace():
            position += 1
        if raw[position : position + 1] == b"#":
            while position < len(raw) and raw[position : position + 1] != b"\n":
                position += 1
            continue
        start = position
        while position < len(raw) and not raw[position : position + 1].isspace():
            position += 1
        fields.append(raw[start:position])
    if len(fields) < 4 or fields[0] != b"P5":
        raise ProtocolError(f"{path} 不是 P5 灰度 PGM")
    magic_w, magic_h, maxval = int(fields[1]), int(fields[2]), int(fields[3])
    if maxval != 255:
        raise ProtocolError(f"只支持 8 位灰度 PGM，maxval={maxval}")
    if (magic_w, magic_h) != (width, height):
        raise ProtocolError(
            f"掩码尺寸 {magic_w}x{magic_h} 与请求 {width}x{height} 不一致"
        )
    position += 1  # 跳过头部与数据之间的单个空白
    pixels = raw[position : position + width * height]
    if len(pixels) != width * height:
        raise ProtocolError(f"掩码数据不足：期望 {width * height} 字节，实际 {len(pixels)}")
    mask = np.frombuffer(pixels, dtype=np.uint8).reshape(height, width)
    return mask > 0


# --------------------------------------------------------------------------- 检测


def band_rows(ratio: np.ndarray, min_gap_merge: int = 4) -> List[tuple]:
    """把「高亮占比高」的行聚成连续行带，并合并被字缝切断的小间隔。

    阈值取 `max(floor, 0.5 * 峰值占比)`，而不是写死的 0.5：
    字幕条往往只占画面宽度的一部分，用固定阈值会把「同一行文字」切成多条，
    这正是锯齿状字缝（上下两段全亮、中间 60% 亮）带来的问题。
    因此这里用相对阈值 + 合并小间隔，尽量把一个文字行还原成一条带。
    """
    if ratio.size == 0:
        return []
    peak = float(ratio.max())
    if peak <= 0.0:
        return []
    threshold = max(0.2, 0.5 * peak)

    bands: List[tuple] = []
    start = None
    for y in range(ratio.size):
        if ratio[y] >= threshold:
            if start is None:
                start = y
        elif start is not None:
            bands.append((start, y - 1))
            start = None
    if start is not None:
        bands.append((start, int(ratio.size) - 1))

    merged: List[tuple] = []
    for band in bands:
        if merged and band[0] - merged[-1][1] - 1 <= min_gap_merge:
            merged[-1] = (merged[-1][0], band[1])
        else:
            merged.append(band)
    return merged


def detect_bright_rows(frame: np.ndarray, min_score: float) -> List[dict]:
    """高亮行启发式：找出「亮像素占比高」的连续行带。

    这是给合成素材/自检用的调试后端，不是 DBNet 的替代品：真实素材里的
    半透明字幕、花体字、复杂背景都不满足「整行高亮」这个假设。
    """
    height, width = frame.shape[0], frame.shape[1]
    gray = frame.astype(np.float32).mean(axis=2)
    bright = gray >= 200.0
    ratio = bright.mean(axis=1)

    boxes: List[dict] = []
    for top, bottom in band_rows(ratio):
        box = _row_band_box(bright, top, bottom, width, min_score)
        if box is not None:
            boxes.append(box)
    return boxes


def _row_band_box(
    bright: np.ndarray, top: int, bottom: int, width: int, min_score: float
) -> Optional[dict]:
    """在行带内按列投影求左右边界。"""
    band = bright[top : bottom + 1, :]
    column_ratio = band.mean(axis=0)
    columns = np.flatnonzero(column_ratio >= 0.5)
    if columns.size == 0:
        return None
    height = bottom - top + 1
    score = float(band.mean())
    if score < min_score:
        return None
    return {
        "x": int(columns.min()),
        "y": int(top),
        "width": int(columns.max() - columns.min() + 1),
        "height": int(height),
        "score": score,
    }


def run_detect(args: argparse.Namespace, work_dir: Path, request: Request) -> int:
    frame = load_raw_frame(work_dir / request.frame_file, request.width, request.height)

    engine = args.detect_engine
    dbnet = None
    if engine in ("auto", "dbnet"):
        model_path = args.dbnet_model or default_dbnet_model()
        if model_path:
            try:
                import dbnet as dbnet_module

                dbnet = dbnet_module.DbnetOnnx(model_path, threads=args.threads)
            except Exception as error:  # 缺 onnxruntime 或模型损坏
                if engine == "dbnet":
                    raise
                warn(f"DBNet 不可用，改用 bright-rows 调试后端：{error}")
        else:
            message = "未找到 DBNet 权重（可用 --dbnet-model 指定）"
            if engine == "dbnet":
                raise ProtocolError(message)
            warn(f"{message}，改用 bright-rows 调试后端")

    if dbnet is None:
        boxes = detect_bright_rows(frame, args.detect_min_score)
        boxes = [box for box in boxes if box is not None]
        for box in boxes:
            box["score"] = max(box["score"], args.detect_min_score)
    else:
        raw_boxes = dbnet.detect(
            frame,
            threshold=args.dbnet_threshold,
            min_score=args.detect_min_score,
            min_area=args.dbnet_min_area,
            unclip_ratio=args.dbnet_unclip_ratio,
            max_side=args.dbnet_max_side,
        )
        boxes = [box.as_dict() for box in raw_boxes]

    write_boxes(work_dir, request, boxes)
    print(f"[worker] 检测完成：{len(boxes)} 个文本框", file=sys.stderr)
    return 0


# --------------------------------------------------------------------------- 修复


def run_inpaint(args: argparse.Namespace, work_dir: Path, request: Request) -> int:
    width, height = request.width, request.height
    in_dir = work_dir / request.input_dir
    out_dir = work_dir / request.output_dir
    out_dir.mkdir(parents=True, exist_ok=True)

    frames = [
        load_raw_frame(in_dir / frame_name(index), width, height)
        for index in range(request.frame_count)
    ]
    mask = load_mask(work_dir / request.mask_file, width, height)
    if not mask.any():
        raise ProtocolError("掩码为空，没有任何需要修复的像素")

    engine = args.inpaint_engine
    restored: Optional[List[np.ndarray]] = None

    if engine in ("auto", "e2fgvi"):
        if args.e2fgvi_ckpt:
            try:
                inpainter = inpaint_backends.build_torch_e2fgvi(
                    repo_path=args.e2fgvi_repo,
                    checkpoint_path=args.e2fgvi_ckpt,
                    device=args.device,
                    num_ref=args.e2fgvi_num_ref,
                )
                restored = inpainter.inpaint(frames, mask)
            except Exception as error:
                if engine == "e2fgvi":
                    raise
                warn(f"E2FGVI 不可用，改用 column 基线：{error}")
        else:
            message = "未提供 --e2fgvi-ckpt，跳过 E2FGVI"
            if engine == "e2fgvi":
                raise ProtocolError(message)
            warn(f"{message}，改用 column 基线")

    if restored is None:
        restored = inpaint_backends.column_fill(frames, mask, sample_rows=args.sample_rows)

    for index, frame in enumerate(restored):
        if frame.shape != frames[index].shape:
            raise ProtocolError(
                f"第 {index} 帧尺寸 {frame.shape} 与输入 {frames[index].shape} 不一致"
            )
        (out_dir / frame_name(index)).write_bytes(np.ascontiguousarray(frame).tobytes())

    print(
        f"[worker] 修复完成：{len(restored)} 帧，掩码占比 {mask.mean() * 100:.2f}%",
        file=sys.stderr,
    )
    return 0


# --------------------------------------------------------------------------- 入口


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="dossier-inpaint 模型 worker（DBNet 检测 + E2FGVI 修复）"
    )
    parser.add_argument("work_dir", help="交换目录，必须包含 request.json")
    parser.add_argument("--mode", choices=[DETECT_MODE, INPAINT_MODE], default=None,
                        help="覆盖 request.json 里的 mode（调试用）")

    detect = parser.add_argument_group("检测")
    detect.add_argument("--detect-engine", choices=["auto", "dbnet", "bright-rows"], default="auto")
    detect.add_argument("--dbnet-model", default=None, help="DBNet ONNX 权重路径")
    detect.add_argument("--dbnet-threshold", type=float, default=0.3)
    detect.add_argument("--dbnet-min-area", type=int, default=16)
    detect.add_argument("--dbnet-unclip-ratio", type=float, default=1.5)
    detect.add_argument("--dbnet-max-side", type=int, default=960)
    detect.add_argument("--detect-min-score", type=float, default=0.5)

    inpaint = parser.add_argument_group("修复")
    inpaint.add_argument("--inpaint-engine", choices=["auto", "e2fgvi", "column"], default="auto")
    inpaint.add_argument("--e2fgvi-repo", default=None, help="E2FGVI 官方仓库路径")
    inpaint.add_argument("--e2fgvi-ckpt", default=None, help="E2FGVI 权重路径")
    inpaint.add_argument("--e2fgvi-num-ref", type=int, default=10)
    inpaint.add_argument("--sample-rows", type=int, default=4, help="column 基线的采样行数")
    inpaint.add_argument("--device", default="cpu")

    parser.add_argument("--threads", type=int, default=0, help="ONNX Runtime 线程数，0 表示自动")
    return parser


def main(argv: Optional[List[str]] = None) -> int:
    args = build_parser().parse_args(argv)
    work_dir = Path(args.work_dir).resolve()
    if not work_dir.is_dir():
        print(f"[worker] 错误：{work_dir} 不是目录", file=sys.stderr)
        return EXIT_PROTOCOL

    try:
        request = Request.load(work_dir)
        mode = args.mode or request.mode
        if mode == DETECT_MODE:
            return run_detect(args, work_dir, request)
        return run_inpaint(args, work_dir, request)
    except ProtocolError as error:
        print(f"[worker] 协议错误：{error}", file=sys.stderr)
        return EXIT_PROTOCOL
    except Exception as error:  # 模型/依赖/IO 故障
        print(f"[worker] 后端错误：{error}", file=sys.stderr)
        if "WORKER_DEBUG" in __import__("os").environ:
            traceback.print_exc()
        return EXIT_BACKEND


if __name__ == "__main__":
    raise SystemExit(main())
