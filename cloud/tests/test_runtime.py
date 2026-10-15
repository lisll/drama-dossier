from __future__ import annotations

import hashlib
import json
from pathlib import Path

import pytest

from drama_cloud.runtime import RuntimeManager, RuntimeManifest, RuntimeRole


def _role(name="ffmpeg", version="1", payload=b"binary"):
    return RuntimeRole(
        name=name,
        version=version,
        platform="linux",
        arch="x86_64",
        sha256=hashlib.sha256(payload).hexdigest(),
        size_bytes=len(payload),
        artifact_name=name,
    )


def test_manifest_rejects_non_https_source_and_duplicate_roles(tmp_path: Path):
    payload = {
        "schema": "drama-dossier/runtime-manifest@1",
        "product": "drama-dossier",
        "roles": [{
            "name": "ffmpeg", "version": "1", "platform": "linux", "arch": "x86_64",
            "sha256": "0" * 64, "size_bytes": 0, "source": "http://example.invalid/a",
        }],
    }
    path = tmp_path / "manifest.json"
    path.write_text(json.dumps(payload), encoding="utf-8")
    with pytest.raises(ValueError, match="HTTPS"):
        RuntimeManifest.load(path)


def test_install_rejects_size_or_hash_mismatch(tmp_path: Path):
    source = tmp_path / "input"
    source.write_bytes(b"actual")
    manager = RuntimeManager(tmp_path / "runtime")
    role = _role(payload=b"expected-size")
    with pytest.raises(ValueError, match="大小不匹配"):
        manager.install(role, source=source)


def test_install_activate_and_rollback_are_atomic(tmp_path: Path):
    first = tmp_path / "first"
    second = tmp_path / "second"
    first.write_bytes(b"one")
    second.write_bytes(b"two")
    manager = RuntimeManager(tmp_path / "runtime")
    manager.install(_role(version="1", payload=b"one"), source=first)
    manager.install(_role(version="2", payload=b"two"), source=second)
    assert manager.resolve("ffmpeg").read_bytes() == b"two"
    manager.rollback("ffmpeg")
    assert manager.resolve("ffmpeg").read_bytes() == b"one"


def test_pointer_path_escape_is_rejected(tmp_path: Path):
    manager = RuntimeManager(tmp_path / "runtime")
    pointer = manager.current_dir / "ffmpeg.json"
    pointer.write_text(json.dumps({
        "name": "ffmpeg", "version": "1", "artifact": str(tmp_path / "outside"),
        "sha256": "0" * 64, "size_bytes": 0,
    }), encoding="utf-8")
    with pytest.raises(ValueError, match="越出 runtime root"):
        manager.resolve("ffmpeg")
