"""存储适配层：键越界、哈希、短时签名令牌。"""

from __future__ import annotations

import io

import pytest

from drama_cloud.errors import ErrorClass, WorkError


def test_put_and_read_roundtrip(storage):
    stat = storage.put_stream("assets/a1/data.bin", io.BytesIO(b"hello"))
    assert stat.size_bytes == 5
    assert storage.exists("assets/a1/data.bin")
    assert storage.read_bytes("assets/a1/data.bin") == b"hello"
    assert storage.sha256("assets/a1/data.bin") == stat.sha256


def test_asset_and_artifact_keys_are_namespaced(storage):
    assert storage.asset_key("ast_1", "a.mp4") == "assets/ast_1/a.mp4"
    assert storage.artifact_key("tsk_1", "out.mp4") == "artifacts/tsk_1/out.mp4"
    # 文件名里的目录成分必须被剥掉，避免拼出意外的键。
    assert storage.asset_key("ast_1", "../../etc/passwd") == "assets/ast_1/passwd"


@pytest.mark.parametrize("key", ["../escape.bin", "assets/../../escape.bin", "/etc/passwd"])
def test_path_traversal_is_blocked(storage, key):
    with pytest.raises(WorkError) as error:
        storage.local_path(key)
    assert error.value.error_class is ErrorClass.PARAM_OUT_OF_RANGE


def test_signed_payload_roundtrip(storage):
    token = storage.sign_payload({"scope": "upload", "name": "a"}, ttl_seconds=60)
    assert storage.verify_payload(token, scope="upload")["name"] == "a"


def test_signed_payload_rejects_wrong_scope(storage):
    token = storage.sign_payload({"scope": "upload"}, ttl_seconds=60)
    with pytest.raises(WorkError):
        storage.verify_payload(token, scope="artifact")


def test_signed_payload_rejects_tampering(storage):
    token = storage.sign_payload({"scope": "upload", "name": "a"}, ttl_seconds=60)
    body, expires, mac = token.split(".")
    tampered = f"{body}.{expires}.{'0' * len(mac)}"
    with pytest.raises(WorkError):
        storage.verify_payload(tampered, scope="upload")


def test_signed_payload_expires(storage):
    token = storage.sign_payload({"scope": "upload"}, ttl_seconds=-1)
    with pytest.raises(WorkError) as error:
        storage.verify_payload(token, scope="upload")
    assert "过期" in error.value.message


def test_object_key_signature_verify(storage):
    token = storage.sign("assets/a1/x.bin", ttl_seconds=60)
    storage.verify("assets/a1/x.bin", token)
    with pytest.raises(WorkError):
        storage.verify("assets/a1/other.bin", token)


def test_secret_key_is_created_0600_and_reused(settings):
    """签名密钥一步以 0600 创建；第二个实例读到同一把密钥，不会覆盖。"""
    import os
    import stat

    from drama_cloud.storage import LocalStorage

    first = LocalStorage(settings)
    path = settings.home / "secret.key"
    if os.name == "posix":
        assert stat.S_IMODE(path.stat().st_mode) == 0o600
    second = LocalStorage(settings)
    assert first._secret == second._secret and len(first._secret) == 32


def test_truncated_secret_key_is_refused(settings):
    from drama_cloud.storage import LocalStorage

    (settings.home / "secret.key").write_bytes(b"short")
    with pytest.raises(RuntimeError, match="长度异常"):
        LocalStorage(settings)


def test_secret_key_is_never_overwritten_and_leaves_no_temp_files(settings):
    """正式文件已存在时不覆盖；生成过程不残留临时文件。"""
    from drama_cloud.storage import LocalStorage

    existing = b"k" * 32
    (settings.home / "secret.key").write_bytes(existing)
    assert LocalStorage(settings)._secret == existing

    (settings.home / "secret.key").unlink()
    LocalStorage(settings)
    leftovers = [p.name for p in settings.home.iterdir() if p.name.startswith("secret.key.tmp-")]
    assert leftovers == []


def test_secret_key_link_race_keeps_the_winner(settings, monkeypatch):
    """模拟另一进程抢先放好密钥：本进程 link 失败时必须读对方的，不覆盖。"""
    import os

    from drama_cloud.storage import LocalStorage

    winner = b"w" * 32
    real_link = os.link

    def racing_link(src, dst):
        (settings.home / "secret.key").write_bytes(winner)  # 对方在这一刻放好了
        return real_link(src, dst)

    monkeypatch.setattr(os, "link", racing_link)
    assert LocalStorage(settings)._secret == winner
    assert not [p for p in settings.home.iterdir() if p.name.startswith("secret.key.tmp-")]
