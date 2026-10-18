"""分片上传与断点续传（M5b-2）。

要钉住的四件事：

1. 断点续传真的能用：传一半、查状态、只补缺的块、完成；
2. 整文件哈希不符**不能**产出可用素材（否则坏素材会流进处理流程）；
3. 分片大小不对要拒绝（否则拼出来的文件会错位）；
4. 别人的上传会话，猜到了 id 也不能读、不能写、不能完成。
"""

from __future__ import annotations

import hashlib

import pytest

from drama_cloud import uploads as uploads_service
from drama_cloud.api import create_app

from fastapi.testclient import TestClient

PAYLOAD = b"0123456789abcdef" * 256  # 4096 字节
CHUNK = 1024  # 4 块


@pytest.fixture()
def client(settings, conn, fake_commerce):
    return TestClient(create_app(settings))


def auth(token: str) -> dict:
    return {"Authorization": f"Bearer {token}"}


def _sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _create(client, token, **overrides):
    body = {
        "name": "第 1 集",
        "role": "video",
        "filename": "ep01.mp4",
        "size_bytes": len(PAYLOAD),
        "sha256": _sha(PAYLOAD),
        "chunk_size": CHUNK,
    }
    body.update(overrides)
    return client.post("/api/v1/assets/uploads", headers=auth(token), json=body)


def _chunk(client, token, upload_id, index, data):
    return client.put(
        f"/api/v1/assets/uploads/{upload_id}/chunks/{index}",
        headers={**auth(token), "Content-Type": "application/octet-stream"},
        content=data,
    )


def _pieces(data: bytes, size: int):
    return [data[offset:offset + size] for offset in range(0, len(data), size)]


def test_a_full_upload_creates_one_asset(client, project, fake_commerce):
    _, _, token = project
    created = _create(client, token)
    assert created.status_code == 201, created.text
    session = created.json()
    assert session["total_chunks"] == 4
    assert session["missing"] == [0, 1, 2, 3]

    for index, piece in enumerate(_pieces(PAYLOAD, CHUNK)):
        assert _chunk(client, token, session["upload_id"], index, piece).status_code == 200

    done = client.post(
        f"/api/v1/assets/uploads/{session['upload_id']}/complete", headers=auth(token)
    )
    assert done.status_code == 200, done.text
    assert done.json()["upload"]["status"] == "completed"
    asset_id = done.json()["asset_id"]
    assert asset_id.startswith("as_")

    assets = client.get("/api/v1/assets", headers=auth(token)).json()["assets"]
    assert any(asset["id"] == asset_id for asset in assets)
    stored = next(asset for asset in assets if asset["id"] == asset_id)
    assert stored["sha256"] == _sha(PAYLOAD)
    assert stored["size_bytes"] == len(PAYLOAD)


def test_resuming_only_needs_the_missing_chunks(client, project):
    """断点续传：传前两块 → 查状态 → 只补后两块。"""
    _, _, token = project
    session = _create(client, token).json()
    pieces = _pieces(PAYLOAD, CHUNK)

    assert _chunk(client, token, session["upload_id"], 0, pieces[0]).status_code == 200
    assert _chunk(client, token, session["upload_id"], 1, pieces[1]).status_code == 200

    status = client.get(
        f"/api/v1/assets/uploads/{session['upload_id']}", headers=auth(token)
    ).json()
    assert status["received"] == [0, 1]
    assert status["missing"] == [2, 3]

    for index in status["missing"]:
        assert _chunk(client, token, session["upload_id"], index, pieces[index]).status_code == 200

    done = client.post(
        f"/api/v1/assets/uploads/{session['upload_id']}/complete", headers=auth(token)
    )
    assert done.status_code == 200, done.text


def test_reuploading_a_chunk_overwrites_it(client, project):
    """同一块重传是覆盖，不是报错——网络重试就是这种情况。"""
    _, _, token = project
    session = _create(client, token).json()
    pieces = _pieces(PAYLOAD, CHUNK)

    assert _chunk(client, token, session["upload_id"], 0, pieces[0]).status_code == 200
    again = _chunk(client, token, session["upload_id"], 0, pieces[0])
    assert again.status_code == 200
    assert again.json()["received"] == [0]


def test_a_wrong_total_hash_is_rejected_and_no_asset_appears(client, project, conn):
    """哈希不符必须拒绝，且不能留下可用素材。"""
    _, _, token = project
    # 声明一个「正确的长度、错误的哈希」。
    session = _create(client, token, sha256="0" * 64).json()
    for index, piece in enumerate(_pieces(PAYLOAD, CHUNK)):
        assert _chunk(client, token, session["upload_id"], index, piece).status_code == 200

    done = client.post(
        f"/api/v1/assets/uploads/{session['upload_id']}/complete", headers=auth(token)
    )
    assert done.status_code == 422, done.text
    assert "校验和" in done.text

    assert client.get("/api/v1/assets", headers=auth(token)).json()["assets"] == []
    row = conn.execute(
        "SELECT status FROM upload_sessions WHERE id = %s", (session["upload_id"],)
    ).fetchone()
    assert row["status"] == "aborted"


def test_completing_with_missing_chunks_is_refused(client, project):
    _, _, token = project
    session = _create(client, token).json()
    pieces = _pieces(PAYLOAD, CHUNK)
    assert _chunk(client, token, session["upload_id"], 0, pieces[0]).status_code == 200

    done = client.post(
        f"/api/v1/assets/uploads/{session['upload_id']}/complete", headers=auth(token)
    )
    assert done.status_code == 400
    assert "没传" in done.text


def test_a_chunk_of_the_wrong_size_is_refused(client, project):
    """除最后一块外必须正好是 chunk_size，否则拼出来会错位。"""
    _, _, token = project
    session = _create(client, token).json()
    bad = _chunk(client, token, session["upload_id"], 0, b"short")
    assert bad.status_code == 400
    assert "大小" in bad.text


def test_a_chunk_index_out_of_range_is_refused(client, project):
    _, _, token = project
    session = _create(client, token).json()
    out = _chunk(client, token, session["upload_id"], 99, b"x" * CHUNK)
    assert out.status_code == 400
    assert "越界" in out.text


def test_the_last_chunk_may_be_shorter(client, project):
    """最后一块按剩余字节算：4096 不能被 1500 整除时，末块应更短。"""
    _, _, token = project
    session = _create(client, token, chunk_size=1500).json()
    pieces = _pieces(PAYLOAD, 1500)
    assert len(pieces) == 3
    assert len(pieces[-1]) == len(PAYLOAD) - 1500 * 2
    for index, piece in enumerate(pieces):
        assert _chunk(client, token, session["upload_id"], index, piece).status_code == 200
    done = client.post(
        f"/api/v1/assets/uploads/{session['upload_id']}/complete", headers=auth(token)
    )
    assert done.status_code == 200, done.text


def test_aborting_removes_the_parts(client, project, settings):
    _, _, token = project
    session = _create(client, token).json()
    pieces = _pieces(PAYLOAD, CHUNK)
    assert _chunk(client, token, session["upload_id"], 0, pieces[0]).status_code == 200

    parts = settings.uploads_dir / session["upload_id"]
    assert parts.exists() and any(parts.iterdir())

    assert client.delete(
        f"/api/v1/assets/uploads/{session['upload_id']}", headers=auth(token)
    ).status_code == 204
    assert not parts.exists() or not any(parts.iterdir())


def test_another_user_cannot_touch_the_session(client, project, fake_commerce, conn):
    """别人的会话：读、写、完成、放弃，全部当作「找不到」。"""
    _, _, owner_token = project
    session = _create(client, owner_token).json()

    other = fake_commerce.user(conn, "intruder@example.com", "operator")
    headers = auth(other)
    assert client.get(
        f"/api/v1/assets/uploads/{session['upload_id']}", headers=headers
    ).status_code == 404
    assert client.put(
        f"/api/v1/assets/uploads/{session['upload_id']}/chunks/0",
        headers={**headers, "Content-Type": "application/octet-stream"},
        content=b"x" * CHUNK,
    ).status_code == 404
    assert client.post(
        f"/api/v1/assets/uploads/{session['upload_id']}/complete", headers=headers
    ).status_code == 404
    assert client.delete(
        f"/api/v1/assets/uploads/{session['upload_id']}", headers=headers
    ).status_code == 404


def test_a_bad_sha256_shape_is_refused(client, project):
    _, _, token = project
    bad = _create(client, token, sha256="NOT-A-HASH")
    assert bad.status_code == 422


def test_too_many_chunks_is_refused(client, project):
    _, _, token = project
    # 1 字节的块 + 1 MiB 的文件 = 超过上限的分片数（这里用最小允许块大小 1024）。
    huge = uploads_service.MAX_CHUNKS * 1024 + 1
    response = _create(
        client, token, size_bytes=huge, sha256="0" * 64, chunk_size=1024
    )
    assert response.status_code == 400
    assert "上限" in response.text


def test_the_session_reports_chunk_size_and_totals(client, project):
    _, _, token = project
    session = _create(client, token).json()
    assert session["chunk_size"] == CHUNK
    assert session["total_chunks"] == 4
    assert session["status"] == "open"
    assert session["asset_id"] is None
