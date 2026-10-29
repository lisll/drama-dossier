"""scripts/make_latest_json.py 的单元测试。

运行：python3 -m unittest discover -s scripts/tests（需要 cryptography）。
测试里现场生成 minisign 格式的密钥与签名，不依赖真实的升级私钥。
"""

from __future__ import annotations

import base64
import datetime as dt
import hashlib
import json
import sys
import tempfile
import unittest
from pathlib import Path

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import make_latest_json as m  # noqa: E402

KEY_ID = b"\x01\x02\x03\x04\x05\x06\x07\x08"


def make_key(key_id: bytes = KEY_ID):
    private = Ed25519PrivateKey.generate()
    raw_pub = private.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw)
    body = base64.b64encode(b"Ed" + key_id + raw_pub).decode()
    text = f"untrusted comment: minisign public key: TEST\n{body}\n"
    return private, base64.b64encode(text.encode()).decode()


def sign(private, path: Path, version: str, key_id: bytes = KEY_ID, prehash: bool = True) -> str:
    data = path.read_bytes()
    message = hashlib.blake2b(data, digest_size=64).digest() if prehash else data
    signature = private.sign(message)
    trusted = f"timestamp:1700000000\tfile:{path.name}\tversion:{version}"
    global_sig = private.sign(signature + trusted.encode())
    algo = b"ED" if prehash else b"Ed"
    text = (
        "untrusted comment: signature from tauri secret key\n"
        f"{base64.b64encode(algo + key_id + signature).decode()}\n"
        f"trusted comment: {trusted}\n"
        f"{base64.b64encode(global_sig).decode()}\n"
    )
    encoded = base64.b64encode(text.encode()).decode()
    path.with_name(path.name + ".sig").write_text(encoded, encoding="utf-8")
    return encoded


class ReleaseFixture(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.private, self.pubkey = make_key()
        self.artifacts = {}
        for platform, name in [
            ("windows-x86_64", "DramaDossier_1.2.3_x64-setup.exe"),
            ("darwin-aarch64", "DramaDossier_1.2.3_aarch64.app.tar.gz"),
            ("darwin-x86_64", "DramaDossier_1.2.3_x64.app.tar.gz"),
        ]:
            path = self.dir / name
            path.write_bytes(f"payload-{platform}".encode())
            sign(self.private, path, "1.2.3")
            self.artifacts[platform] = path

    def tearDown(self):
        self.tmp.cleanup()

    def build(self, **overrides):
        args = dict(
            tag="app-v1.2.3",
            repo="owner/repo",
            artifacts=self.artifacts,
            encoded_pubkey=self.pubkey,
            notes="修复若干问题",
            now=dt.datetime(2026, 10, 29, 3, 4, 5, 678, tzinfo=dt.timezone.utc),
        )
        args.update(overrides)
        return m.build_latest(**args)


class BuildLatestTest(ReleaseFixture):
    def test_builds_manifest_for_all_platforms(self):
        latest = self.build()
        self.assertEqual(latest["version"], "1.2.3")
        self.assertEqual(latest["notes"], "修复若干问题")
        self.assertEqual(latest["pub_date"], "2026-10-29T03:04:05Z")
        self.assertEqual(set(latest["platforms"]), set(m.REQUIRED_PLATFORMS))
        win = latest["platforms"]["windows-x86_64"]
        self.assertEqual(
            win["url"],
            "https://github.com/owner/repo/releases/download/app-v1.2.3/DramaDossier_1.2.3_x64-setup.exe",
        )
        sig_file = self.artifacts["windows-x86_64"].with_name("DramaDossier_1.2.3_x64-setup.exe.sig")
        self.assertEqual(win["signature"], sig_file.read_text().strip())
        json.dumps(latest)  # 可序列化

    def test_missing_platform_fails(self):
        del self.artifacts["darwin-x86_64"]
        with self.assertRaisesRegex(m.ReleaseError, "缺少平台产物：darwin-x86_64"):
            self.build()

    def test_unknown_platform_fails(self):
        self.artifacts["linux-x86_64"] = self.artifacts["windows-x86_64"]
        with self.assertRaisesRegex(m.ReleaseError, "未知平台"):
            self.build()

    def test_missing_signature_file_fails(self):
        self.artifacts["darwin-aarch64"].with_name(
            self.artifacts["darwin-aarch64"].name + ".sig"
        ).unlink()
        with self.assertRaisesRegex(m.ReleaseError, "签名不存在"):
            self.build()

    def test_tampered_artifact_fails(self):
        self.artifacts["windows-x86_64"].write_bytes(b"evil")
        with self.assertRaisesRegex(m.ReleaseError, "签名校验失败"):
            self.build()

    def test_signature_from_other_key_fails(self):
        other, _ = make_key(b"\x09" * 8)
        sign(other, self.artifacts["darwin-x86_64"], "1.2.3", key_id=b"\x09" * 8)
        with self.assertRaisesRegex(m.ReleaseError, "key id 不符"):
            self.build()

    def test_other_key_with_same_key_id_fails(self):
        other, _ = make_key()
        sign(other, self.artifacts["darwin-x86_64"], "1.2.3")
        with self.assertRaisesRegex(m.ReleaseError, "签名校验失败"):
            self.build()

    def test_signed_version_mismatch_fails(self):
        sign(self.private, self.artifacts["darwin-aarch64"], "1.2.2")
        with self.assertRaisesRegex(m.ReleaseError, "签名记录的版本是 '1.2.2'"):
            self.build()

    def test_tampered_trusted_comment_fails(self):
        sig_path = self.artifacts["windows-x86_64"].with_name(
            self.artifacts["windows-x86_64"].name + ".sig"
        )
        text = base64.b64decode(sig_path.read_text()).decode()
        text = text.replace("version:1.2.3", "version:9.9.9")
        sig_path.write_text(base64.b64encode(text.encode()).decode())
        with self.assertRaisesRegex(m.ReleaseError, "trusted comment 被篡改"):
            self.build(tag="app-v9.9.9")

    def test_legacy_non_prehashed_signature_is_accepted(self):
        sign(self.private, self.artifacts["windows-x86_64"], "1.2.3", prehash=False)
        self.assertIn("windows-x86_64", self.build()["platforms"])


class VersionTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.conf = self.dir / "tauri.conf.json"
        self.cargo = self.dir / "Cargo.toml"
        self.conf.write_text(json.dumps({"version": "1.2.3"}), encoding="utf-8")
        self.cargo.write_text(
            '[package]\nname = "x"\nversion = "1.2.3"\n\n[dependencies]\nfoo = { version = "9.9.9" }\n',
            encoding="utf-8",
        )

    def tearDown(self):
        self.tmp.cleanup()

    def test_matching_versions_pass(self):
        self.assertEqual(m.check_version("app-v1.2.3", self.conf, self.cargo), "1.2.3")

    def test_mismatch_names_the_file(self):
        self.cargo.write_text('[package]\nversion = "1.2.2"\n', encoding="utf-8")
        with self.assertRaisesRegex(m.ReleaseError, "Cargo.toml 是 1.2.2"):
            m.check_version("app-v1.2.3", self.conf, self.cargo)

    def test_bad_tags_are_rejected(self):
        for tag in ["v1.2.3", "app-v1.2", "app-v1.2.3-beta", "app-v"]:
            with self.assertRaises(m.ReleaseError, msg=tag):
                m.version_from_tag(tag)

    def test_release_notes_fallback(self):
        notes_dir = self.dir / "notes"
        self.assertEqual(m.release_notes("1.2.3", notes_dir), "剧备 v1.2.3")
        notes_dir.mkdir()
        (notes_dir / "v1.2.3.md").write_text("  新增自动升级\n", encoding="utf-8")
        self.assertEqual(m.release_notes("1.2.3", notes_dir), "新增自动升级")


class RealConfigTest(unittest.TestCase):
    """仓库里的 pubkey 必须是能解析的 minisign Ed25519 公钥。"""

    def test_repo_pubkey_parses(self):
        conf = json.loads(m.TAURI_CONF.read_text(encoding="utf-8"))
        updater = conf["plugins"]["updater"]
        key_id, _ = m.parse_public_key(updater["pubkey"])
        self.assertEqual(len(key_id), 8)
        self.assertTrue(updater["requireSignedVersion"])
        self.assertTrue(all(url.startswith("https://") for url in updater["endpoints"]))
        self.assertTrue(conf["bundle"]["createUpdaterArtifacts"])


if __name__ == "__main__":
    unittest.main()
