"""排版脚本（AI 制作证明、AI 合规角标、过程图）的单元测试。

运行：python3 -m unittest discover -s tools/process_images/tests（需要 Pillow）。

脚本以子进程方式运行，与桌面端的调用方式一致。缺字体的情形用环境变量
`DOSSIER_CJK_FONT_CANDIDATES` 指定一份不存在的候选列表来模拟。
本机没有任何中文字体时，「正常出图」类用例会跳过，「缺字体必须报错」类用例照常运行。
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from PIL import Image

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import cjk_font  # noqa: E402


def have_cjk_font() -> bool:
    try:
        cjk_font.load_cjk_font(19)
        return True
    except cjk_font.FontMissingError:
        return False


def run(script: str, *args: str, missing_fonts: bool = False) -> subprocess.CompletedProcess:
    env = dict(os.environ, PYTHONDONTWRITEBYTECODE="1", PYTHONUTF8="1")
    if missing_fonts:
        env["DOSSIER_CJK_FONT_CANDIDATES"] = os.pathsep.join(
            ["/不存在/字体A.ttc", "/不存在/字体B.ttf"]
        )
    else:
        env.pop("DOSSIER_CJK_FONT_CANDIDATES", None)
    return subprocess.run(
        [sys.executable, str(SCRIPTS / script), *args],
        capture_output=True,
        text=True,
        encoding="utf-8",
        env=env,
        timeout=120,
    )


PROOF_PAYLOAD = {
    "date": "2026-10-30",
    "version": "0.2.24",
    "dramaName": "测试剧",
    "originalEpisodes": 2,
    "evidence": "本地 ASR/OCR，共 2 集",
    "processingLines": ["逐集微帧", "编码：libx264（2 路并行）"],
    "title": "雪夜进山",
    "titleSource": "AI 生成（模型 m）",
    "synopsis": "医生被困山村。",
    "synopsisSource": "素材目录",
    "posterName": "",
    "posterSource": "未提供",
    "processCount": 0,
    "storyboardSource": "未生成",
    "outputs": [{"fileName": "第01集.mp4", "size": 3, "sha256": "ab" * 32}],
    "sources": None,
    "aiAssisted": True,
    "declaration": "由人工智能生成：剧名。",
}


class FontLookupTest(unittest.TestCase):
    def test_missing_candidates_raise_with_searched_paths(self):
        with self.assertRaises(cjk_font.FontMissingError) as caught:
            cjk_font.load_cjk_font(19, candidates=["/不存在/x.ttc"])
        self.assertIn("/不存在/x.ttc", str(caught.exception))

    def test_font_without_chinese_glyphs_is_rejected(self):
        """Pillow 自带的默认字体没有汉字：每个汉字画成同一个方块，必须判为不可用。"""
        from PIL import ImageFont

        default = ImageFont.load_default()
        self.assertFalse(cjk_font._draws_chinese(default))

    @unittest.skipUnless(have_cjk_font(), "本机没有中文字体")
    def test_found_font_draws_distinct_chinese_glyphs(self):
        face = cjk_font.load_cjk_font(19)
        self.assertTrue(cjk_font._draws_chinese(face))

    def test_env_override_replaces_builtin_candidates(self):
        saved = os.environ.get("DOSSIER_CJK_FONT_CANDIDATES")
        try:
            os.environ["DOSSIER_CJK_FONT_CANDIDATES"] = os.pathsep.join(["/a.ttc", "/b.ttf"])
            self.assertEqual(cjk_font.candidate_paths(), ["/a.ttc", "/b.ttf"])
            del os.environ["DOSSIER_CJK_FONT_CANDIDATES"]
            self.assertIn("C:/Windows/Fonts/msyh.ttc", cjk_font.candidate_paths())
        finally:
            if saved is None:
                os.environ.pop("DOSSIER_CJK_FONT_CANDIDATES", None)
            else:
                os.environ["DOSSIER_CJK_FONT_CANDIDATES"] = saved


class SavePngAtomicTest(unittest.TestCase):
    def test_replaces_target_and_leaves_no_temp(self):
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp) / "a.png"
            target.write_bytes(b"old")
            cjk_font.save_png_atomic(Image.new("RGB", (4, 4)), str(target))
            self.assertEqual(target.read_bytes()[:8], b"\x89PNG\r\n\x1a\n")
            self.assertEqual([p.name for p in Path(tmp).iterdir()], ["a.png"])

    @unittest.skipIf(os.name == "nt", "符号链接需要特权")
    def test_does_not_write_through_symlink(self):
        with tempfile.TemporaryDirectory() as tmp:
            victim = Path(tmp) / "victim.txt"
            victim.write_bytes(b"original")
            link = Path(tmp) / "AI制作证明.png"
            link.symlink_to(victim)
            cjk_font.save_png_atomic(Image.new("RGB", (4, 4)), str(link))
            self.assertEqual(victim.read_bytes(), b"original")
            self.assertFalse(link.is_symlink())


class ProofImageTest(unittest.TestCase):
    def _data(self, tmp: str) -> str:
        path = Path(tmp) / "data.json"
        path.write_text(json.dumps(PROOF_PAYLOAD, ensure_ascii=False), encoding="utf-8")
        return str(path)

    def test_missing_fonts_fail_without_writing_image(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "AI制作证明.png"
            result = run(
                "make_proof_image.py", f"--data={self._data(tmp)}", f"--output={out}",
                missing_fonts=True,
            )
            self.assertEqual(result.returncode, 1, result.stdout)
            self.assertIn("找不到可显示中文的字体", result.stderr)
            self.assertFalse(out.exists(), "缺字体时不能产出看不清的证明图")

    @unittest.skipUnless(have_cjk_font(), "本机没有中文字体")
    def test_renders_proof_image(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "AI制作证明.png"
            result = run("make_proof_image.py", f"--data={self._data(tmp)}", f"--output={out}")
            self.assertEqual(result.returncode, 0, result.stderr)
            with Image.open(out) as image:
                self.assertEqual(image.width, 1240)
                self.assertGreaterEqual(image.height, 1754)
            leftovers = [p.name for p in Path(tmp).iterdir() if p.name.endswith(".tmp")]
            self.assertEqual(leftovers, [])


class BadgeTest(unittest.TestCase):
    def test_missing_fonts_fail_without_writing_badge(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / ".ai-badge.png"
            result = run(
                "make_ai_badge.py", f"--output={out}", "--text=内容由AI生成", missing_fonts=True
            )
            self.assertEqual(result.returncode, 1, result.stdout)
            self.assertIn("找不到可显示中文的字体", result.stderr)
            self.assertFalse(out.exists(), "缺字体时不能产出会烧进成片的方块角标")

    @unittest.skipUnless(have_cjk_font(), "本机没有中文字体")
    def test_renders_badge_with_dash_leading_text(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / ".ai-badge.png"
            result = run("make_ai_badge.py", f"--output={out}", "--text=-内容由AI生成-")
            self.assertEqual(result.returncode, 0, result.stderr)
            with Image.open(out) as image:
                self.assertEqual(image.mode, "RGBA")


class ProcessImagesTest(unittest.TestCase):
    def _frames(self, tmp: str) -> str:
        paths = []
        for index in range(3):
            path = Path(tmp) / f"frame{index}.png"
            Image.new("RGB", (320, 568), (120 + index * 20, 90, 60)).save(path)
            paths.append(str(path))
        return ",".join(paths)

    def test_missing_fonts_fail_without_writing_any_image(self):
        with tempfile.TemporaryDirectory() as tmp:
            out_dir = Path(tmp) / "out"
            result = run(
                "make_process_images.py", f"--output-dir={out_dir}",
                f"--frames={self._frames(tmp)}", "--title=测试剧", missing_fonts=True,
            )
            self.assertEqual(result.returncode, 1, result.stdout)
            self.assertIn("找不到可显示中文的字体", result.stderr)
            self.assertEqual(sorted(out_dir.glob("过程图_*.png")), [], "缺字体时一张都不能写出")

    @unittest.skipUnless(have_cjk_font(), "本机没有中文字体")
    def test_renders_four_images_with_dash_leading_title(self):
        with tempfile.TemporaryDirectory() as tmp:
            out_dir = Path(tmp) / "out"
            result = run(
                "make_process_images.py", f"--output-dir={out_dir}",
                f"--frames={self._frames(tmp)}", "--title=-序章-",
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len(list(out_dir.glob("过程图_*.png"))), 4)


if __name__ == "__main__":
    unittest.main()
