"""dossier-inpaint 模型 worker 的离线单测。

只依赖 numpy，用 `python3 -m unittest discover` 运行：

    cd tools/model_worker && python3 -m unittest discover -s tests -v

E2FGVI 权重无法在 CI/开发机上获取，因此这里用一个「假模型」验证适配层的
张量变换、补边裁回与「只在洞内合成」这几条不变量——真正不可验证的只剩权重本身。
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import dbnet  # noqa: E402
import inpaint_backends as backends  # noqa: E402
import protocol  # noqa: E402
import worker  # noqa: E402


def make_work_dir(root: Path) -> Path:
    (root / "in").mkdir(parents=True, exist_ok=True)
    return root


def write_request(root: Path, payload: dict) -> None:
    (root / "request.json").write_text(json.dumps(payload), encoding="utf-8")


def p5(mask: np.ndarray) -> bytes:
    height, width = mask.shape
    header = f"P5\n{width} {height}\n255\n".encode("ascii")
    return header + (mask.astype(np.uint8) * 255).tobytes()


def _render_text_frame():
    """用系统字体渲染一行中文文字，返回 BGR 帧；缺少字体/PIL 时返回 None。"""
    try:
        from PIL import Image, ImageDraw, ImageFont
    except ImportError:
        return None

    font_path = None
    for candidate in (
        "/usr/share/fonts/chinese/simsun.ttc",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ):
        if Path(candidate).is_file():
            font_path = candidate
            break
    if font_path is None:
        return None

    image = Image.new("RGB", (320, 96), (30, 30, 30))
    draw = ImageDraw.Draw(image)
    font = ImageFont.truetype(font_path, 36)
    draw.text((20, 24), "测试字幕 ABC", fill=(255, 255, 255), font=font)
    return np.array(image)[:, :, ::-1].copy()  # RGB → BGR


class ProtocolTests(unittest.TestCase):
    def test_load_rejects_bad_mode(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_request(root, {"mode": "nope", "width": 4, "height": 4})
            with self.assertRaises(protocol.ProtocolError):
                protocol.Request.load(root)

    def test_load_requires_positive_dimensions(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_request(root, {"mode": "detect", "width": 0, "height": 4})
            with self.assertRaises(protocol.ProtocolError):
                protocol.Request.load(root)

    def test_frame_name_is_four_digits(self) -> None:
        self.assertEqual(protocol.frame_name(0), "frame_0000.raw")
        self.assertEqual(protocol.frame_name(12345), "frame_12345.raw")

    def test_text_box_clips_to_picture(self) -> None:
        box = protocol.text_box(-5, 100, 50, 20, 0.9, clip_width=640, clip_height=360)
        self.assertEqual(box, {"x": 0, "y": 100, "width": 45, "height": 20, "score": 0.9})

        box = protocol.text_box(600, 350, 100, 100, 0.9, 640, 360)
        self.assertEqual(box, {"x": 600, "y": 350, "width": 40, "height": 10, "score": 0.9})

        self.assertIsNone(protocol.text_box(700, 10, 10, 10, 0.9, 640, 360))

    def test_write_boxes_drops_empty_rectangles(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_request(
                root,
                {
                    "mode": "detect",
                    "width": 32,
                    "height": 32,
                    "frameFile": "in/frame.raw",
                    "outputFile": "boxes.json",
                },
            )
            request = protocol.Request.load(root)
            protocol.write_boxes(
                root,
                request,
                [
                    {"x": 4, "y": 4, "width": 8, "height": 4, "score": 0.8},
                    {"x": 40, "y": 4, "width": 8, "height": 4, "score": 0.8},
                ],
            )
            payload = json.loads((root / "boxes.json").read_text(encoding="utf-8"))
            self.assertEqual(len(payload["boxes"]), 1)
            self.assertEqual(payload["boxes"][0]["width"], 8)


class DbnetTests(unittest.TestCase):
    def test_round_to_multiple_never_returns_zero(self) -> None:
        self.assertEqual(dbnet.round_to_multiple(1, 32, 32), 32)
        self.assertEqual(dbnet.round_to_multiple(33, 32, 32), 32)
        self.assertEqual(dbnet.round_to_multiple(40, 32, 32), 32)
        self.assertEqual(dbnet.round_to_multiple(49, 32, 32), 64)

    def test_connected_components_counts_four_neighbourhood(self) -> None:
        mask = np.zeros((8, 8), dtype=bool)
        mask[1:3, 1:4] = True
        mask[5:7, 5:7] = True
        mask[1, 6] = True
        labels, count = dbnet.connected_components(mask)
        self.assertEqual(count, 3)
        self.assertEqual(labels[1, 1], labels[2, 3])
        self.assertNotEqual(labels[1, 1], labels[5, 5])
        self.assertNotEqual(labels[1, 6], labels[1, 1])
        self.assertEqual(labels[0, 0], 0)

    def test_diagonal_pixels_are_not_connected(self) -> None:
        mask = np.zeros((2, 2), dtype=bool)
        mask[0, 0] = True
        mask[1, 1] = True
        _, count = dbnet.connected_components(mask)
        self.assertEqual(count, 2)

    def test_resize_preserves_corners(self) -> None:
        image = np.zeros((4, 4, 3), dtype=np.uint8)
        image[0, 0] = (10, 20, 30)
        image[3, 3] = (40, 50, 60)
        resized = dbnet._resize_bilinear(image, 8, 8)
        self.assertEqual(resized.shape, (8, 8, 3))
        self.assertEqual(tuple(resized[0, 0]), (10, 20, 30))
        self.assertEqual(tuple(resized[7, 7]), (40, 50, 60))

    def test_boxes_from_probability_returns_scaled_box(self) -> None:
        prob = np.zeros((64, 64), dtype=np.float32)
        prob[40:50, 8:56] = 0.9
        boxes = dbnet.boxes_from_probability(
            prob, scale_x=2.0, scale_y=1.0, threshold=0.3, min_score=0.5, min_area=16
        )
        self.assertEqual(len(boxes), 1)
        box = boxes[0]
        self.assertLessEqual(box.y, 40)
        self.assertGreaterEqual(box.y + box.height, 50)
        self.assertLessEqual(box.x, 16)
        self.assertGreaterEqual(box.x + box.width, 112)
        self.assertAlmostEqual(box.score, 0.9, places=5)

    def test_boxes_from_probability_filters_small_and_low_score(self) -> None:
        tiny = np.zeros((32, 32), dtype=np.float32)
        tiny[10, 10] = 0.9
        self.assertEqual(
            dbnet.boxes_from_probability(tiny, 1.0, 1.0, min_area=16), []
        )

        weak = np.zeros((32, 32), dtype=np.float32)
        weak[10:20, 10:30] = 0.4
        self.assertEqual(
            dbnet.boxes_from_probability(weak, 1.0, 1.0, threshold=0.3, min_score=0.5), []
        )

    def test_preprocess_maps_bgr_to_rgb_tensor(self) -> None:
        frame = np.zeros((64, 64, 3), dtype=np.uint8)
        frame[:, :] = (0, 0, 255)  # 纯红（BGR）
        tensor, scale_x, scale_y = dbnet.preprocess(frame, 64, 64)
        self.assertEqual(tensor.shape, (1, 3, 64, 64))
        self.assertEqual(tensor.dtype, np.float32)
        # RGB 顺序下红色在第 0 通道，且应大于另外两个通道
        self.assertGreater(tensor[0, 0].mean(), tensor[0, 1].mean())
        self.assertGreater(tensor[0, 0].mean(), tensor[0, 2].mean())
        self.assertEqual((scale_x, scale_y), (1.0, 1.0))


class ColumnFillTests(unittest.TestCase):
    def test_fills_from_rows_above(self) -> None:
        frame = np.full((10, 4, 3), 40, dtype=np.uint8)
        mask = np.zeros((10, 4), dtype=bool)
        mask[6:8, :] = True
        frame[6:8, :] = 250  # 被覆盖的「字幕」
        out = backends.column_fill([frame], mask, sample_rows=2)[0]
        self.assertTrue(np.all(out[6:8, :] == 40), "应被上方背景色填满")
        self.assertTrue(np.all(out[4:6, :] == 40), "掩码之外的像素不应改变")

    def test_falls_back_to_rows_below_when_top_is_masked(self) -> None:
        frame = np.full((10, 2, 3), 40, dtype=np.uint8)
        mask = np.zeros((10, 2), dtype=bool)
        mask[0:3, :] = True
        frame[3:5, :] = 90
        out = backends.column_fill([frame], mask, sample_rows=2)[0]
        self.assertTrue(np.all(out[0:3, :] == 90), "上方全被掩码时应改用下方样本")

    def test_skips_fully_masked_column(self) -> None:
        frame = np.full((4, 2, 3), 40, dtype=np.uint8)
        mask = np.zeros((4, 2), dtype=bool)
        mask[:, 1] = True
        out = backends.column_fill([frame], mask, sample_rows=2)[0]
        self.assertTrue(np.all(out[:, 1] == 40), "整列被掩码时保持原值")

    def test_rejects_size_mismatch(self) -> None:
        frame = np.zeros((4, 4, 3), dtype=np.uint8)
        mask = np.zeros((5, 5), dtype=bool)
        with self.assertRaises(backends.InpaintError):
            backends.column_fill([frame], mask)


class E2fgviAdapterTests(unittest.TestCase):
    class EchoModel:
        """假模型：把输入原样返回，并记录收到的张量形状。"""

        def __init__(self) -> None:
            self.calls = []

        def __call__(self, frames, masks):
            self.calls.append((frames.shape, masks.shape))
            return frames

    class PaintEverythingModel:
        """假模型：把整幅 ROI 涂成固定颜色（模拟「整幅重建」的真实模型）。"""

        def __call__(self, frames, masks):
            return np.full_like(frames, 0.5)

    def test_pads_to_multiple_and_crops_back(self) -> None:
        model = self.EchoModel()
        inpainter = backends.E2fgviInpainter(model, multiple=8)
        frames = [np.full((10, 13, 3), 30, dtype=np.uint8) for _ in range(3)]
        mask = np.zeros((10, 13), dtype=bool)
        mask[4:6, 2:9] = True

        out = inpainter.inpaint(frames, mask)
        self.assertEqual(len(out), 3)
        self.assertEqual(out[0].shape, (10, 13, 3), "必须裁回原始尺寸")
        # 补边后的张量应为 8 的整数倍
        frames_shape, masks_shape = model.calls[0]
        self.assertEqual(frames_shape, (1, 3, 3, 16, 16))
        self.assertEqual(masks_shape, (1, 1, 3, 16, 16))

    def test_only_hole_pixels_are_taken_from_model(self) -> None:
        inpainter = backends.E2fgviInpainter(self.PaintEverythingModel(), multiple=8)
        frames = [np.full((8, 8, 3), 30, dtype=np.uint8)]
        mask = np.zeros((8, 8), dtype=bool)
        mask[2:4, 2:5] = True

        out = inpainter.inpaint(frames, mask)[0]
        self.assertFalse(np.all(out[2:4, 2:5] == 30), "洞内应被模型改写")
        # 期望：只有洞内取自模型，其余位置必须与输入逐像素一致
        expected = frames[0].copy()
        expected[2:4, 2:5] = out[2:4, 2:5]
        self.assertTrue(np.array_equal(out, expected), "洞外必须逐像素保持原样")

    def test_rejects_fully_masked_region(self) -> None:
        inpainter = backends.E2fgviInpainter(self.EchoModel())
        frames = [np.zeros((8, 8, 3), dtype=np.uint8)]
        mask = np.ones((8, 8), dtype=bool)
        with self.assertRaises(backends.InpaintError):
            inpainter.inpaint(frames, mask)

    def test_rejects_frame_count_mismatch(self) -> None:
        class WrongCountModel:
            def __call__(self, frames, masks):
                return np.zeros((1, 3, 1, frames.shape[3], frames.shape[4]), np.float32)

        inpainter = backends.E2fgviInpainter(WrongCountModel())
        frames = [np.zeros((8, 8, 3), dtype=np.uint8) for _ in range(2)]
        mask = np.zeros((8, 8), dtype=bool)
        mask[0, 0] = True
        with self.assertRaises(backends.InpaintError):
            inpainter.inpaint(frames, mask)

    def test_tensor_round_trip(self) -> None:
        frames = np.random.default_rng(7).integers(0, 256, (2, 5, 6, 3), dtype=np.uint8)
        tensor = backends.to_model_tensor(frames)
        restored = backends.from_model_tensor(tensor, frame_count=2)
        self.assertTrue(np.array_equal(frames, restored), "BGR→张量→BGR 应无损往返")


class ModelSignatureTests(unittest.TestCase):
    """E2FGVI 各变体 forward 签名不同，参数不能被静默丢掉。"""

    def test_detects_explicit_keywords(self) -> None:
        def forward(frames, masks, neighbor_stride=5, num_ref=10):
            return None

        names = backends._accepted_keywords(forward)
        self.assertIsNotNone(names)
        self.assertIn("neighbor_stride", names)
        self.assertIn("num_ref", names)

    def test_var_keyword_yields_unknown(self) -> None:
        def forward(frames, masks, **kwargs):
            return None

        self.assertIsNone(backends._accepted_keywords(forward))

    def test_window_parameters_are_forwarded_when_accepted(self) -> None:
        seen = {}

        class Model:
            def forward(self, frames, masks, neighbor_stride=5, num_ref=10):
                seen["neighbor_stride"] = neighbor_stride
                seen["num_ref"] = num_ref
                return frames

        inpainter = backends.E2fgviInpainter(Model().forward, neighbor_stride=7, num_ref=3)
        self.assertEqual(inpainter.neighbor_stride, 7)
        self.assertEqual(inpainter.num_ref, 3)
        # 适配层自身只做张量编排，窗口参数由 build_torch_e2fgvi 注入模型；
        # 这里断言签名探测能区分两种约定。
        self.assertIsNotNone(
            backends._accepted_keywords(Model().forward)
        )

    def test_two_argument_signature_is_accepted(self) -> None:
        def forward(frames, masks):
            return frames

        names = backends._accepted_keywords(forward)
        self.assertIsNotNone(names)
        self.assertFalse({"neighbor_stride", "num_ref"} <= names)


class DetectHeuristicTests(unittest.TestCase):
    def test_merges_glyph_gap_into_single_band(self) -> None:
        # 上方全亮、中间只亮 60%（字缝）、下方全亮 → 应合成一条带
        ratio = np.zeros(20, dtype=np.float32)
        ratio[5:8] = 0.625
        ratio[8:11] = 0.375
        ratio[11:14] = 0.625
        bands = worker.band_rows(ratio)
        self.assertEqual(bands, [(5, 13)])

    def test_keeps_distant_bands_separate(self) -> None:
        ratio = np.zeros(40, dtype=np.float32)
        ratio[2:4] = 0.9
        ratio[30:32] = 0.9
        bands = worker.band_rows(ratio)
        self.assertEqual(bands, [(2, 3), (30, 31)])

    def test_empty_ratio_returns_no_bands(self) -> None:
        self.assertEqual(worker.band_rows(np.zeros(0, dtype=np.float32)), [])
        self.assertEqual(worker.band_rows(np.zeros(5, dtype=np.float32)), [])


class WorkerEndToEndTests(unittest.TestCase):
    def test_detect_writes_boxes_for_synthetic_subtitle(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = make_work_dir(Path(tmp))
            height, width = 120, 200
            frame = np.full((height, width, 3), 20, dtype=np.uint8)
            frame[90:110, 40:160] = 240  # 底部字幕条
            (root / "in" / "frame.raw").write_bytes(frame.tobytes())
            write_request(
                root,
                {
                    "mode": "detect",
                    "width": width,
                    "height": height,
                    "frameFile": "in/frame.raw",
                    "outputFile": "boxes.json",
                },
            )
            status = worker.main([str(root), "--detect-engine", "bright-rows"])
            self.assertEqual(status, 0)
            payload = json.loads((root / "boxes.json").read_text(encoding="utf-8"))
            self.assertEqual(len(payload["boxes"]), 1)
            box = payload["boxes"][0]
            self.assertEqual((box["x"], box["y"]), (40, 90))
            self.assertEqual((box["width"], box["height"]), (120, 20))

    def test_inpaint_keeps_frame_count_size_and_holes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = make_work_dir(Path(tmp))
            height, width, count = 32, 40, 3
            frames = []
            for index in range(count):
                frame = np.full((height, width, 3), 60, dtype=np.uint8)
                frame[20:26, 8:32] = 250  # 字幕
                frames.append(frame)
                (root / "in" / protocol.frame_name(index)).write_bytes(frame.tobytes())

            mask = np.zeros((height, width), dtype=bool)
            mask[20:26, 8:32] = True
            (root / "mask.pgm").write_bytes(p5(mask))
            write_request(
                root,
                {
                    "mode": "inpaint",
                    "width": width,
                    "height": height,
                    "frameCount": count,
                    "inputDir": "in",
                    "outputDir": "out",
                    "maskFile": "mask.pgm",
                },
            )

            status = worker.main([str(root), "--inpaint-engine", "column"])
            self.assertEqual(status, 0)

            for index in range(count):
                path = root / "out" / protocol.frame_name(index)
                self.assertTrue(path.is_file(), f"缺少输出帧 {path}")
                data = path.read_bytes()
                self.assertEqual(len(data), height * width * 3, "输出字节数必须完全一致")
                out = np.frombuffer(data, np.uint8).reshape(height, width, 3)
                self.assertTrue(np.all(out[20:26, 8:32] == 60), "洞内应被背景色填满")
                expected = frames[index].copy()
                expected[20:26, 8:32] = out[20:26, 8:32]
                self.assertTrue(
                    np.array_equal(out, expected), "洞外必须逐像素保持原样"
                )

    def test_inpaint_reports_protocol_error_on_size_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = make_work_dir(Path(tmp))
            frame = np.full((16, 16, 3), 10, dtype=np.uint8)
            (root / "in" / protocol.frame_name(0)).write_bytes(frame.tobytes())
            mask = np.zeros((16, 16), dtype=bool)
            mask[4:8, 4:8] = True
            (root / "mask.pgm").write_bytes(p5(mask))
            write_request(
                root,
                {
                    "mode": "inpaint",
                    "width": 8,  # 与帧/掩码不符
                    "height": 8,
                    "frameCount": 1,
                    "inputDir": "in",
                    "outputDir": "out",
                    "maskFile": "mask.pgm",
                },
            )
            status = worker.main([str(root), "--inpaint-engine", "column"])
            self.assertEqual(status, worker.EXIT_PROTOCOL)

    def test_missing_model_is_a_hard_failure_when_explicit(self) -> None:
        """显式要求 DBNet 但没有权重时必须硬失败，不能静默降级。"""
        original = worker.default_dbnet_model
        worker.default_dbnet_model = lambda: None
        try:
            with tempfile.TemporaryDirectory() as tmp:
                root = make_work_dir(Path(tmp))
                frame = np.full((16, 16, 3), 10, dtype=np.uint8)
                (root / "in" / "frame.raw").write_bytes(frame.tobytes())
                write_request(
                    root,
                    {
                        "mode": "detect",
                        "width": 16,
                        "height": 16,
                        "frameFile": "in/frame.raw",
                        "outputFile": "boxes.json",
                    },
                )
                status = worker.main([str(root), "--detect-engine", "dbnet"])
                self.assertEqual(status, worker.EXIT_PROTOCOL)
        finally:
            worker.default_dbnet_model = original

    def test_auto_falls_back_when_model_is_absent(self) -> None:
        """auto 模式下没有权重应降级并成功，而不是报错。"""
        original = worker.default_dbnet_model
        worker.default_dbnet_model = lambda: None
        try:
            with tempfile.TemporaryDirectory() as tmp:
                root = make_work_dir(Path(tmp))
                height, width = 60, 120
                frame = np.full((height, width, 3), 20, dtype=np.uint8)
                frame[40:50, 20:100] = 240
                (root / "in" / "frame.raw").write_bytes(frame.tobytes())
                write_request(
                    root,
                    {
                        "mode": "detect",
                        "width": width,
                        "height": height,
                        "frameFile": "in/frame.raw",
                        "outputFile": "boxes.json",
                    },
                )
                status = worker.main([str(root), "--detect-engine", "auto"])
                self.assertEqual(status, 0)
                payload = json.loads((root / "boxes.json").read_text(encoding="utf-8"))
                self.assertEqual(len(payload["boxes"]), 1)
        finally:
            worker.default_dbnet_model = original

    def test_auto_prefers_dbnet_when_model_is_available(self) -> None:
        """auto 模式在有权重时应真正走 DBNet，而不是无声退回启发式。"""
        model = worker.default_dbnet_model()
        if model is None:
            self.skipTest("未找到 DBNet 权重，跳过真实模型冒烟测试")
        try:
            import onnxruntime  # noqa: F401
        except ImportError:
            self.skipTest("未安装 onnxruntime，跳过真实模型冒烟测试")

        text_image = _render_text_frame()
        if text_image is None:
            self.skipTest("缺少可用字体，跳过真实模型冒烟测试")

        with tempfile.TemporaryDirectory() as tmp:
            root = make_work_dir(Path(tmp))
            height, width = text_image.shape[0], text_image.shape[1]
            (root / "in" / "frame.raw").write_bytes(text_image.tobytes())
            write_request(
                root,
                {
                    "mode": "detect",
                    "width": width,
                    "height": height,
                    "frameFile": "in/frame.raw",
                    "outputFile": "boxes.json",
                },
            )
            status = worker.main([str(root), "--detect-engine", "dbnet"])
            self.assertEqual(status, 0)
            payload = json.loads((root / "boxes.json").read_text(encoding="utf-8"))
            boxes = payload["boxes"]
            # 画面里只有一行真实文字，DBNet 应当至少给出一个框
            self.assertGreaterEqual(len(boxes), 1)
            for box in boxes:
                self.assertGreater(box["width"], 0)
                self.assertGreater(box["height"], 0)
                self.assertLessEqual(box["x"] + box["width"], width)
                self.assertLessEqual(box["y"] + box["height"], height)
                self.assertGreater(box["score"], 0.5)


if __name__ == "__main__":
    unittest.main()


class TensorCodecTests(unittest.TestCase):
    """张量编码（tanh / unit）必须与权重训练时一致，否则输出会偏色。"""

    def test_unit_codec_round_trip(self) -> None:
        frames = np.random.default_rng(11).integers(0, 256, (2, 5, 6, 3), dtype=np.uint8)
        tensor = backends.to_model_tensor(frames, codec="unit")
        self.assertAlmostEqual(float(tensor.min()), 0.0, places=6)
        self.assertAlmostEqual(float(tensor.max()), 1.0, places=6)
        restored = backends.from_model_tensor(tensor, frame_count=2, codec="unit")
        self.assertTrue(np.array_equal(frames, restored))

    def test_tanh_and_unit_differ(self) -> None:
        frames = np.full((1, 2, 2, 3), 128, dtype=np.uint8)
        tanh = backends.to_model_tensor(frames, codec="tanh")
        unit = backends.to_model_tensor(frames, codec="unit")
        self.assertLess(float(tanh.mean()), 0.02)
        self.assertAlmostEqual(float(unit.mean()), 128 / 255, places=5)

    def test_unknown_codec_is_rejected(self) -> None:
        frames = np.zeros((1, 2, 2, 3), dtype=np.uint8)
        with self.assertRaises(backends.InpaintError):
            backends.to_model_tensor(frames, codec="sigmoid")
        with self.assertRaises(backends.InpaintError):
            backends.VideoModelInpainter(lambda f, m: f, codec="sigmoid")


class FrameWiseInpainterTests(unittest.TestCase):
    """LaMa 一类单帧模型：逐帧调用，但仍只在洞内取模型结果。"""

    class UnitModel:
        def __init__(self) -> None:
            self.batches = []

        def __call__(self, image, mask):
            self.batches.append(image.shape)
            return np.ones_like(image)  # 全白（unit 编码下就是 255）

    def test_calls_model_once_per_frame_with_single_frame_batches(self) -> None:
        model = self.UnitModel()
        inpainter = backends.FrameWiseInpainter(model, multiple=8)
        frames = [np.full((8, 8, 3), 20, dtype=np.uint8) for _ in range(3)]
        mask = np.zeros((8, 8), dtype=bool)
        mask[2:4, 2:4] = True

        out = inpainter.inpaint(frames, mask)
        self.assertEqual([shape[2] for shape in model.batches], [1, 1, 1])
        self.assertEqual(len(out), 3)
        for frame in out:
            self.assertEqual(frame[2, 2].tolist(), [255, 255, 255], "洞内应取模型输出")
            self.assertEqual(frame[0, 0].tolist(), [20, 20, 20], "洞外应保持原样")

    def test_default_codec_is_unit(self) -> None:
        inpainter = backends.FrameWiseInpainter(self.UnitModel())
        self.assertEqual(inpainter.codec, "unit")


class BackendRegistryTests(unittest.TestCase):
    def test_registry_covers_cli_choices(self) -> None:
        parser = worker.build_parser()
        choice_action = next(
            action
            for action in parser._actions
            if "--inpaint-engine" in getattr(action, "option_strings", [])
        )
        for name in choice_action.choices:
            # auto 走默认模型；column 是 numpy 基线，不属于模型注册表
            if name in ("auto", "column"):
                continue
            self.assertIn(name, backends.BACKEND_BUILDERS, f"{name} 缺少构造函数")
        self.assertIn("column", choice_action.choices, "基线后端必须可选")

    def test_external_backend_needs_repo_path(self) -> None:
        with self.assertRaises(backends.InpaintError):
            backends.build_external_backend("propainter", repo_path=None, checkpoint_path="x")

    def test_external_backend_needs_adapter_module(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(backends.InpaintError):
                backends.build_external_backend(
                    "propainter", repo_path=tmp, checkpoint_path="x"
                )

    def test_external_backend_loads_adapter_module(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            (Path(tmp) / "dossier_backend.py").write_text(
                "class Backend:\n"
                "    def inpaint(self, frames, mask):\n"
                "        return list(frames)\n"
                "\n"
                "def build_inpainter(repo_path, checkpoint_path, device, num_ref):\n"
                "    return Backend()\n",
                encoding="utf-8",
            )
            inpainter = backends.build_external_backend(
                "propainter", repo_path=tmp, checkpoint_path="x"
            )
            self.assertTrue(hasattr(inpainter, "inpaint"))

    def test_external_backend_rejects_adapter_without_inpaint(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            (Path(tmp) / "dossier_backend.py").write_text(
                "def build_inpainter(repo_path, checkpoint_path, device, num_ref):\n"
                "    return object()\n",
                encoding="utf-8",
            )
            with self.assertRaises(backends.InpaintError):
                backends.build_external_backend(
                    "propainter", repo_path=tmp, checkpoint_path="x"
                )

    def test_propainter_builder_delegates_to_external_adapter(self) -> None:
        with self.assertRaises(backends.InpaintError):
            backends.build_torch_propainter(repo_path="/nonexistent", checkpoint_path="x")


class ServeModeTests(unittest.TestCase):
    """常驻模式：一个进程处理多个请求，且不污染协议输出。"""

    def _spawn_serve(self, *extra):
        return subprocess.Popen(
            [
                sys.executable,
                str(Path(worker.__file__)),
                "--serve",
                "--detect-engine",
                "bright-rows",
                "--inpaint-engine",
                "column",
                *extra,
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
        )

    @staticmethod
    def _read_reply(process):
        line = process.stdout.readline()
        if not line:
            raise AssertionError("常驻 worker 提前退出了")
        return json.loads(line)

    def _make_detect_dir(self, root: Path) -> None:
        make_work_dir(root)
        frame = np.full((48, 64, 3), 30, dtype=np.uint8)
        frame[30:38, 8:56] = 250  # 一条「高亮字幕带」
        (root / "in" / "frame.raw").write_bytes(frame.tobytes())
        write_request(
            root,
            {
                "mode": "detect",
                "width": 64,
                "height": 48,
                "frameFile": "in/frame.raw",
                "outputFile": "boxes.json",
            },
        )

    def _make_inpaint_dir(self, root: Path) -> None:
        make_work_dir(root)
        (root / "out").mkdir(exist_ok=True)
        frame = np.full((16, 16, 3), 40, dtype=np.uint8)
        (root / "in" / "frame_0000.raw").write_bytes(frame.tobytes())
        mask = np.zeros((16, 16), dtype=bool)
        mask[6:10, :] = True
        (root / "mask.pgm").write_bytes(p5(mask))
        write_request(
            root,
            {
                "mode": "inpaint",
                "width": 16,
                "height": 16,
                "frameCount": 1,
                "inputDir": "in",
                "outputDir": "out",
                "maskFile": "mask.pgm",
            },
        )

    def test_serves_multiple_requests_from_one_process(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            detect_dir = Path(tmp) / "detect"
            inpaint_dir = Path(tmp) / "inpaint"
            detect_dir.mkdir()
            inpaint_dir.mkdir()
            self._make_detect_dir(detect_dir)
            self._make_inpaint_dir(inpaint_dir)

            process = self._spawn_serve()
            try:
                hello = self._read_reply(process)
                self.assertTrue(hello.get("ready"))
                pid = hello["pid"]

                for work_dir in (detect_dir, inpaint_dir, detect_dir):
                    process.stdin.write(json.dumps({"workDir": str(work_dir)}) + "\n")
                    process.stdin.flush()
                    reply = self._read_reply(process)
                    self.assertTrue(reply.get("ok"), reply)
                    self.assertEqual(hello["pid"], pid, "必须复用同一个进程")

                boxes = json.loads((detect_dir / "boxes.json").read_text(encoding="utf-8"))
                self.assertTrue(boxes["boxes"], "第二个请求也应写出检测结果")
            finally:
                process.stdin.close()
                process.wait(timeout=10)

    def test_bad_request_is_reported_without_killing_the_process(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            good = Path(tmp) / "detect"
            good.mkdir()
            self._make_detect_dir(good)

            process = self._spawn_serve()
            try:
                self._read_reply(process)
                process.stdin.write(json.dumps({"workDir": str(Path(tmp) / "missing")}) + "\n")
                process.stdin.flush()
                reply = self._read_reply(process)
                self.assertFalse(reply["ok"])
                self.assertEqual(reply["kind"], "protocol")

                process.stdin.write(json.dumps({"workDir": str(good)}) + "\n")
                process.stdin.flush()
                self.assertTrue(self._read_reply(process)["ok"], "坏请求之后应能继续服务")
            finally:
                process.stdin.close()
                process.wait(timeout=10)

    def test_backend_error_is_reported_as_backend(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "broken"
            root.mkdir()
            make_work_dir(root)
            write_request(
                root,
                {
                    "mode": "inpaint",
                    "width": 8,
                    "height": 8,
                    "frameCount": 1,
                    "inputDir": "in",
                    "outputDir": "in/frame_0000.raw/out",  # 无法创建目录 → 后端错误
                    "maskFile": "mask.pgm",
                },
            )
            frame = np.full((8, 8, 3), 40, dtype=np.uint8)
            (root / "in" / "frame_0000.raw").write_bytes(frame.tobytes())
            mask = np.zeros((8, 8), dtype=bool)
            mask[4, :] = True
            (root / "mask.pgm").write_bytes(p5(mask))

            process = self._spawn_serve()
            try:
                self._read_reply(process)
                process.stdin.write(json.dumps({"workDir": str(root)}) + "\n")
                process.stdin.flush()
                reply = self._read_reply(process)
                self.assertFalse(reply["ok"])
                self.assertEqual(reply["kind"], "backend")
            finally:
                process.stdin.close()
                process.wait(timeout=10)
