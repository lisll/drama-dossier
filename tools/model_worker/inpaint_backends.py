"""修复后端：numpy 基线，以及 E2FGVI（时序视频 inpainting）适配层。

分层原因：
- E2FGVI 需要 torch 与权重，本机（aarch64 / 无 CUDA）装不上，因此把「模型调用」
  压缩成最薄的一层，其余全部是纯 numpy 的张量变换，可以离线单测。
- 基线后端（列填充）与 Rust 侧 `ColumnFillInpainter` 语义一致，用来在没有模型时
  把整条链路跑通，并作为 worker 协议的可执行参照。

约定：
- 帧为 `(H, W, 3)` uint8，通道顺序 BGR（与 ffmpeg bgr24 一致）；
- 掩码为 `(H, W)` bool，True 表示需要修复（洞）；
- 输出帧的尺寸与输入必须完全相同，由调用方再次校验。
"""

from __future__ import annotations

from typing import List, Optional, Sequence, Tuple

import numpy as np


class InpaintError(RuntimeError):
    """修复后端无法完成请求。"""


def median_value(samples: List[int]) -> int:
    """取中位数；偶数个样本时取偏上的那个，与 Rust 侧 `median` 保持一致。"""
    ordered = sorted(samples)
    return ordered[len(ordered) // 2]


def column_fill(frames: Sequence[np.ndarray], mask: np.ndarray, sample_rows: int = 4) -> List[np.ndarray]:
    """逐列取掩码外邻行的中位数填充。

    与 Rust `ColumnFillInpainter` 同语义：对每一列，从第一行被掩码的位置向上取
    `sample_rows` 个非掩码像素；上方不足时改为向下取。整列都被掩码覆盖时跳过。
    """
    if not frames:
        raise InpaintError("没有可修复的帧")
    height, width = mask.shape
    keep = max(1, int(sample_rows))
    outputs: List[np.ndarray] = []

    for frame in frames:
        if frame.shape[:2] != mask.shape:
            raise InpaintError(f"帧尺寸 {frame.shape[:2]} 与掩码 {mask.shape} 不一致")
        filled = frame.copy()
        for x in range(width):
            column = mask[:, x]
            masked_rows = np.flatnonzero(column).tolist()
            if not masked_rows:
                continue

            source: List[int] = []
            cursor = masked_rows[0]
            while cursor > 0 and len(source) < keep:
                cursor -= 1
                if not column[cursor]:
                    source.append(cursor)
            if not source:
                cursor = masked_rows[-1] + 1
                while cursor < height and len(source) < keep:
                    if not column[cursor]:
                        source.append(cursor)
                    cursor += 1
            if not source:
                continue

            for channel in range(3):
                samples = [int(frame[y, x, channel]) for y in source]
                filled[masked_rows, x, channel] = median_value(samples)
        outputs.append(filled)
    return outputs


def pad_to_multiple(
    frames: np.ndarray, mask: np.ndarray, multiple: int = 8
) -> Tuple[np.ndarray, np.ndarray, Tuple[int, int]]:
    """把 (T,H,W,C) 帧与 (H,W) 掩码右侧/下方补零到 `multiple` 的整数倍。

    返回 `(padded_frames, padded_mask, (pad_h, pad_w))`。
    """
    if frames.ndim != 4:
        raise InpaintError(f"帧数组应为 (T,H,W,C)，实际 {frames.shape}")
    height, width = frames.shape[1], frames.shape[2]
    pad_h = (-height) % multiple
    pad_w = (-width) % multiple
    if pad_h == 0 and pad_w == 0:
        return frames, mask, (0, 0)
    padded_frames = np.pad(
        frames, ((0, 0), (0, pad_h), (0, pad_w), (0, 0)), mode="edge"
    )
    padded_mask = np.pad(mask, ((0, pad_h), (0, pad_w)), mode="edge")
    return padded_frames, padded_mask, (pad_h, pad_w)


def to_model_tensor(frames_bgr: np.ndarray) -> np.ndarray:
    """(T,H,W,3) BGR uint8 → (1,3,T,H,W) float32，取值 [-1,1]，RGB 通道序。"""
    rgb = frames_bgr[:, :, :, ::-1].astype(np.float32) / 255.0
    normalized = rgb * 2.0 - 1.0
    tensor = np.transpose(normalized, (3, 0, 1, 2))[None, ...]
    return np.ascontiguousarray(tensor, dtype=np.float32)


def mask_to_model_tensor(mask: np.ndarray, frame_count: int) -> np.ndarray:
    """(H,W) bool → (1,1,T,H,W) float32，1 表示洞。

    硬字幕在片段内位置固定，因此 T 个时刻共用同一张掩码。
    """
    if frame_count <= 0:
        raise InpaintError("frame_count 必须为正")
    single = mask.astype(np.float32)[None, None, None, ...]
    tiled = np.repeat(single, frame_count, axis=2)
    return np.ascontiguousarray(tiled, dtype=np.float32)


def from_model_tensor(tensor: np.ndarray, frame_count: int) -> np.ndarray:
    """(1,3,T,H,W) float32 [-1,1] → (T,H,W,3) BGR uint8。"""
    array = np.asarray(tensor, dtype=np.float32)
    if array.ndim != 5 or array.shape[0] != 1 or array.shape[1] != 3:
        raise InpaintError(f"模型输出形状应为 (1,3,T,H,W)，实际 {array.shape}")
    if array.shape[2] != frame_count:
        raise InpaintError(
            f"模型输出的帧数 {array.shape[2]} 与输入 {frame_count} 不一致"
        )
    rgb = np.clip((array[0] + 1.0) / 2.0, 0.0, 1.0)
    bgr = np.transpose(rgb, (1, 2, 3, 0))[:, :, :, ::-1]
    return np.rint(bgr * 255.0).astype(np.uint8)


class E2fgviInpainter:
    """E2FGVI 适配层。

    `model` 只需是一个可调用对象：`model(frames_tensor, masks_tensor) -> tensor`，
    张量布局为 (1,3,T,H,W) / (1,1,T,H,W)。把模型隔在这一层之外，使得
    「补边 → 归一化 → 调用 → 只在洞内合成 → 裁回原尺寸」这条链路可以
    用假模型离线单测，而真正不可验证的只剩权重本身。
    """

    def __init__(
        self,
        model,
        neighbor_stride: int = 5,
        num_ref: int = 10,
        multiple: int = 8,
        model_signature: str = "frames+masks",
    ) -> None:
        self._model = model
        self.neighbor_stride = int(neighbor_stride)
        self.num_ref = int(num_ref)
        self.multiple = int(multiple)
        # 记录实际使用的调用约定，便于日志/排查
        self.model_signature = model_signature

    def inpaint(self, frames: Sequence[np.ndarray], mask: np.ndarray) -> List[np.ndarray]:
        if not frames:
            raise InpaintError("没有可修复的帧")
        stacked = np.stack([np.asarray(frame, dtype=np.uint8) for frame in frames], axis=0)
        if stacked.shape[1:3] != mask.shape:
            raise InpaintError(f"帧尺寸 {stacked.shape[1:3]} 与掩码 {mask.shape} 不一致")
        if mask.all():
            # 整块都是洞时模型没有可参考像素，交给调用方决定策略
            raise InpaintError("掩码覆盖了整个 ROI，无法修复")

        padded_frames, padded_mask, (pad_h, pad_w) = pad_to_multiple(
            stacked, mask, self.multiple
        )
        model_input = to_model_tensor(padded_frames)
        mask_input = mask_to_model_tensor(padded_mask, frame_count=len(frames))

        prediction = self._model(model_input, mask_input)
        restored = from_model_tensor(prediction, frame_count=len(frames))

        # 先裁回原尺寸，再只在洞内采用模型结果；洞外保留原像素，避免模型
        # 改动非掩码区域。即便模型整幅重建，这一步也保证回贴内容受控
        # （Rust 侧还会用羽化 alpha 再兜一次）。
        origin_h = stacked.shape[1]
        origin_w = stacked.shape[2]
        cropped = restored[:, :origin_h, :origin_w, :]
        if cropped.shape != stacked.shape:
            raise InpaintError(
                f"裁回后形状 {cropped.shape} 与输入 {stacked.shape} 不一致"
            )
        merged = np.where(mask[:, :, None], cropped, stacked)
        return [np.ascontiguousarray(frame) for frame in merged]


def _accepted_keywords(callable_or_fn):
    """返回可调用对象显式接受的参数名集合；无法探测时返回 None。

    有 `**kwargs` 时无法判断真实意图，返回 None，调用方走保守分支。
    """
    import inspect

    try:
        signature = inspect.signature(callable_or_fn)
    except (TypeError, ValueError):
        return None
    names = set()
    for parameter in signature.parameters.values():
        if parameter.kind is inspect.Parameter.VAR_KEYWORD:
            return None
        if parameter.kind in (
            inspect.Parameter.POSITIONAL_OR_KEYWORD,
            inspect.Parameter.KEYWORD_ONLY,
        ):
            names.add(parameter.name)
    return names


def build_torch_e2fgvi(
    repo_path: Optional[str],
    checkpoint_path: str,
    device: str = "cpu",
    num_ref: int = 10,
):
    """加载官方 E2FGVI 模型。

    需要：torch + 官方仓库（https://github.com/mc-e/Enhancing-Video-Inpainting）。
    本机无法安装 torch，因此这条路径在本仓库内未做端到端验证，仅提供接线；
    权重与仓库都不入库，见 `fetch_models.sh` 与 `docs/SUBTITLE_REMOVAL.md`。
    """
    import sys

    try:
        import torch  # noqa: F401
    except ImportError as error:  # pragma: no cover - 取决于运行环境
        raise InpaintError(
            "未安装 torch，无法运行 E2FGVI；请安装 PyTorch 或改用 --inpaint-engine column"
        ) from error

    if repo_path:
        sys.path.insert(0, repo_path)
    try:
        from model.e2fgvi import InpaintGenerator
    except ImportError as error:  # pragma: no cover
        raise InpaintError(
            f"无法从 {repo_path!r} 导入 model.e2fgvi.InpaintGenerator：{error}"
        ) from error

    import torch

    model = InpaintGenerator(init_weights=False)
    state = torch.load(checkpoint_path, map_location="cpu")
    # 官方权重可能包在 {"model": ...} / {"state_dict": ...} 里
    for key in ("model", "state_dict", "params"):
        if isinstance(state, dict) and key in state and isinstance(state[key], dict):
            state = state[key]
            break
    model.load_state_dict(state)
    model.to(device).eval()

    # E2FGVI 各变体的 forward 签名不一致：官方 `e2fgvi` 版接受
    # neighbor_stride / num_ref 关键字，`e2fgvi_hq` 等版本只接受 (frames, masks)。
    # 这里按签名分派，避免把这两个参数静默丢掉（丢掉会让参考帧窗口与预期不符，
    # 直接影响时序一致性）。
    accepted = _accepted_keywords(getattr(model, "forward", None) or model)
    if accepted is not None and {"neighbor_stride", "num_ref"} <= accepted:
        def call(model, frames, masks):
            return model(
                frames, masks, neighbor_stride=neighbor_stride, num_ref=num_ref
            )
    else:
        def call(model, frames, masks):
            return model(frames, masks)

    def forward(frames_tensor: np.ndarray, masks_tensor: np.ndarray):
        with torch.no_grad():
            frames = torch.from_numpy(np.ascontiguousarray(frames_tensor)).to(device)
            masks = torch.from_numpy(np.ascontiguousarray(masks_tensor)).to(device)
            return call(model, frames, masks).detach().cpu().numpy()

    uses_window = accepted is not None and {"neighbor_stride", "num_ref"} <= accepted
    return E2fgviInpainter(
        forward,
        num_ref=num_ref,
        neighbor_stride=neighbor_stride,
        model_signature="neighbor_stride+num_ref" if uses_window else "frames+masks",
    )
