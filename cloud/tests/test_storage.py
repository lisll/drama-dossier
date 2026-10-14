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
