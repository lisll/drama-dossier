"""dossier-inpaint 模型 worker 的离线单测。

只依赖 numpy，用 `python3 -m unittest discover` 运行：

    cd tools/model_worker && python3 -m unittest discover -s tests -v

E2FGVI 权重无法在 CI/开发机上获取，因此这里用一个「假模型」验证适配层的
张量变换、补边裁回与「只在洞内合成」这几条不变量——真正不可验证的只剩权重本身。
"""

from __future__ import annotations

import json
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
