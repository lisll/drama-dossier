"""DBNet 文本检测：ONNX Runtime 推理 + 概率图后处理。

设计取舍：
- 推理只依赖 `onnxruntime`，不引入 torch。本机（aarch64、无 CUDA）用 CPU
  EP 即可跑通单帧检测，这对「先定位字幕条、再做时序修复」的两段式流程足够了。
- 只做「把概率图变成矩形」这一件事，跨帧聚合与抖动抑制交给 Rust 侧
  (`dossier-inpaint::detect::stable_roi`)，避免两边各写一套聚合逻辑。

适配的模型：DBNet / PP-OCR 系列导出的 ONNX（输出为单通道概率图）。
RapidOCR 的 `ch_PP-OCRv4_det_infer.onnx` 是常见且体积小的选择，
获取方式见 `fetch_models.sh`。
"""

from __future__ import annotations

from typing import Dict, List, Optional, Sequence, Tuple

import numpy as np


# 预处理用的 ImageNet 统计量。PP-OCR det 模型训练时使用的就是这一组。
_PIXEL_MEAN = np.array([0.485, 0.456, 0.406], dtype=np.float32)
_PIXEL_STD = np.array([0.229, 0.224, 0.225], dtype=np.float32)

# 卷积下采样需要整除的倍数（DBNet 的 backbone 是 4 次下采样）。
_SIZE_MULTIPLE = 32


def round_to_multiple(value: int, multiple: int = _SIZE_MULTIPLE, minimum: int = None) -> int:
    """把边长对齐到 `multiple` 的整数倍；`minimum` 给最小值兜底。"""
    if minimum is None:
        minimum = multiple
    scaled = max(minimum, int(round(value / float(multiple))) * multiple)
    return max(multiple, scaled)


class Box:
    """一个检测框；坐标为整帧像素坐标。"""

    __slots__ = ("x", "y", "width", "height", "score")

    def __init__(self, x: int, y: int, width: int, height: int, score: float) -> None:
        self.x = int(x)
        self.y = int(y)
        self.width = int(width)
        self.height = int(height)
        self.score = float(score)

    def as_dict(self) -> dict:
        return {
            "x": self.x,
            "y": self.y,
            "width": self.width,
            "height": self.height,
            "score": self.score,
        }


def preprocess(frame_bgr: np.ndarray, height: int, width: int) -> Tuple[np.ndarray, float, float]:
    """BGR uint8 → 归一化 float32 NCHW。

    返回 `(tensor, scale_x, scale_y)`，用于把概率图坐标映射回原图。
    """
    source_h, source_w = frame_bgr.shape[0], frame_bgr.shape[1]
    target_h = round_to_multiple(height, _SIZE_MULTIPLE, _SIZE_MULTIPLE)
    target_w = round_to_multiple(width, _SIZE_MULTIPLE, _SIZE_MULTIPLE)

    resized = _resize_bilinear(frame_bgr, target_h, target_w)
    # BGR → RGB，随后按 ImageNet 统计量归一化
    rgb = resized[:, :, ::-1].astype(np.float32) / 255.0
    normalized = (rgb - _PIXEL_MEAN) / _PIXEL_STD
    tensor = np.transpose(normalized, (2, 0, 1))[None, ...]
    tensor = np.ascontiguousarray(tensor, dtype=np.float32)

    scale_x = source_w / float(target_w)
    scale_y = source_h / float(target_h)
    return tensor, scale_x, scale_y


def _resize_bilinear(image: np.ndarray, target_h: int, target_w: int) -> np.ndarray:
    """双线性缩放；只依赖 numpy，避免为了缩放再引入 OpenCV。"""
    source_h, source_w = image.shape[0], image.shape[1]
    if source_h == target_h and source_w == target_w:
        return image

    y_index = np.linspace(0, source_h - 1, target_h, dtype=np.float32)
    x_index = np.linspace(0, source_w - 1, target_w, dtype=np.float32)

    y0 = np.floor(y_index).astype(np.int32)
    x0 = np.floor(x_index).astype(np.int32)
    y1 = np.minimum(y0 + 1, source_h - 1)
    x1 = np.minimum(x0 + 1, source_w - 1)

    wy = (y_index - y0)[:, None, None]
    wx = (x_index - x0)[None, :, None]

    top = image[y0][:, x0] * (1.0 - wx) + image[y0][:, x1] * wx
    bottom = image[y1][:, x0] * (1.0 - wx) + image[y1][:, x1] * wx
    blended = top * (1.0 - wy) + bottom * wy
    return blended.astype(np.uint8)


def binarize(prob_map: np.ndarray, threshold: float) -> np.ndarray:
    """概率图 → 布尔前景图；DBNet 的默认阈值 0.3。"""
    return prob_map >= float(threshold)


def connected_components(mask: np.ndarray) -> Tuple[np.ndarray, int]:
    """4 邻域连通域标记，返回 (label 图, 连通域数量)，背景为 0。

    只遍历前景像素（先用 `np.flatnonzero` 取出来），因此代价与文字面积成正比，
    而不是与整帧面积成正比。用并查集合并，避免为了一处标记引入 scipy。
    """
    height, width = mask.shape
    labels = np.zeros(height * width, dtype=np.int32)
    parent: List[int] = [0]

    def find(label: int) -> int:
        root = label
        while parent[root] != root:
            root = parent[root]
        while parent[label] != root:
            parent[label], label = root, parent[label]
        return root

    def union(a: int, b: int) -> None:
        ra, rb = find(a), find(b)
        if ra != rb:
            parent[max(ra, rb)] = min(ra, rb)

    next_label = 1
    for flat in np.flatnonzero(mask.reshape(-1)).tolist():
        y, x = divmod(flat, width)
        up = labels[flat - width] if y > 0 else 0
        left = labels[flat - 1] if x > 0 else 0
        if up and left:
            labels[flat] = min(up, left)
            union(up, left)
        elif up or left:
            labels[flat] = up or left
        else:
            parent.append(next_label)
            labels[flat] = next_label
            next_label += 1

    # 把并查集根映射成从 1 开始的紧凑编号
    compact = np.zeros_like(labels)
    root_to_new: Dict[int, int] = {}
    count = 0
    for flat in np.flatnonzero(labels).tolist():
        root = find(int(labels[flat]))
        new = root_to_new.get(root)
        if new is None:
            count += 1
            new = count
            root_to_new[root] = new
        compact[flat] = new
    return compact.reshape(height, width), count


def boxes_from_probability(
    prob_map: np.ndarray,
    scale_x: float,
    scale_y: float,
    threshold: float = 0.3,
    min_score: float = 0.5,
    min_area: int = 16,
    unclip_ratio: float = 1.5,
) -> List[Box]:
    """概率图 → 检测框。

    简化处理：DBNet 论文用「收缩后的轮廓 + unclip」还原文本区域；这里直接用
    连通域外接矩形并按 `unclip_ratio` 向外扩张，对「整行字幕条」这种长条目标
    已经足够，也更容易解释。
    """
    foreground = binarize(prob_map, threshold)
    if not foreground.any():
        return []

    labels, count = connected_components(foreground)
    boxes: List[Box] = []
    for label in range(1, count + 1):
        ys, xs = np.nonzero(labels == label)
        if ys.size < min_area:
            continue
        score = float(prob_map[ys, xs].mean())
        if score < min_score:
            continue

        x0, x1 = int(xs.min()), int(xs.max())
        y0, y1 = int(ys.min()), int(ys.max())
        width = x1 - x0 + 1
        height = y1 - y0 + 1

        # DBNet 的 unclip：概率图对应的是收缩后的文本区域，按
        # offset = area * ratio / perimeter 向外扩张还原真实边界。
        perimeter = 2.0 * (width + height)
        offset = 0.0 if perimeter <= 0.0 else (width * height * unclip_ratio) / perimeter
        pad = int(round(offset))
        x0 = max(0, x0 - pad)
        y0 = max(0, y0 - pad)
        width += 2 * pad
        height += 2 * pad

        # 概率图坐标 → 原图坐标
        boxes.append(
            Box(
                x=int(round(x0 * scale_x)),
                y=int(round(y0 * scale_y)),
                width=max(1, int(round(width * scale_x))),
                height=max(1, int(round(height * scale_y))),
                score=score,
            )
        )
    return boxes


class DbnetOnnx:
    """DBNet ONNX 会话的薄封装。"""

    def __init__(self, model_path: str, threads: Optional[int] = None) -> None:
        import onnxruntime  # 延迟导入：缺依赖时给出清晰报错

        options = onnxruntime.SessionOptions()
        options.intra_op_num_threads = int(threads or 0)
        options.graph_optimization_level = onnxruntime.GraphOptimizationLevel.ORT_ENABLE_ALL
        self._session = onnxruntime.InferenceSession(
            model_path, sess_options=options, providers=["CPUExecutionProvider"]
        )
        inputs = self._session.get_inputs()
        if len(inputs) != 1:
            raise RuntimeError(f"DBNet 模型应只有 1 个输入，实际 {len(inputs)} 个")
        self._input_name = inputs[0].name
        outputs = self._session.get_outputs()
        if not outputs:
            raise RuntimeError("DBNet 模型没有输出")
        self._output_name = outputs[0].name

    def probability(self, tensor: np.ndarray) -> np.ndarray:
        """运行一次推理，返回 (H, W) 概率图。"""
        result = self._session.run([self._output_name], {self._input_name: tensor})[0]
        prob = np.asarray(result, dtype=np.float32)
        if prob.ndim == 4:
            prob = prob[0, 0]
        elif prob.ndim == 3:
            prob = prob[0]
        elif prob.ndim != 2:
            raise RuntimeError(f"无法解释 DBNet 输出形状：{prob.shape}")
        return prob

    def detect(
        self,
        frame_bgr: np.ndarray,
        threshold: float = 0.3,
        min_score: float = 0.5,
        min_area: int = 16,
        unclip_ratio: float = 1.5,
        max_side: int = 960,
    ) -> List[Box]:
        """整帧检测，返回原图坐标下的框。`max_side` 是长边上限。"""
        height, width = frame_bgr.shape[0], frame_bgr.shape[1]
        target_h, target_w = _fit_within(height, width, max_side, max_side)
        tensor, scale_x, scale_y = preprocess(frame_bgr, target_h, target_w)
        prob = self.probability(tensor)
        return boxes_from_probability(
            prob,
            scale_x=scale_x,
            scale_y=scale_y,
            threshold=threshold,
            min_score=min_score,
            min_area=min_area,
            unclip_ratio=unclip_ratio,
        )


def _fit_within(height: int, width: int, max_h: int, max_w: int) -> Tuple[int, int]:
    """等比缩放到不超过给定上限。"""
    if height <= 0 or width <= 0:
        return max_h, max_w
    ratio = min(max_h / float(height), max_w / float(width), 1.0)
    return max(1, int(round(height * ratio))), max(1, int(round(width * ratio)))
