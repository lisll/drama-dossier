"""到期删除（M5b-5）。

总方案的完成标准之一是「过了期限的文件确实被删掉」。这里钉住四件事：

1. 到期素材的**文件真的没了**，不只是数据库标记；
2. 元数据（哈希、大小、时间）**留着**——审计与对账要用；
3. 文件名、权属声明这类可能含敏感信息的内容被清掉；
4. 没到期的**不能动**；放弃的分片上传也要清。
"""

from __future__ import annotations

import io
import os
from datetime import datetime, timedelta, timezone

import pytest

from drama_cloud.config import load_settings
from drama_cloud.db import connect
from drama_cloud.storage import LocalStorage
from drama_cloud.tasks import service


def _settings(tmp_path, **overrides):
    return load_settings(home=tmp_path / "home", **overrides)


def _register(conn, storage, project_id, name="第01集.mp4", declaration=None):
    """登记一份素材，返回 (asset, 磁盘路径)。"""
    asset = service.register_asset(
        conn, storage, project_id, name, io.BytesIO(b"video-bytes"), role="video"
    )
    if declaration is not None:
        conn.execute(
            "UPDATE assets SET source_declaration = %s WHERE id = %s",
            (declaration, asset["id"]),
        )
    return asset, storage.local_path(asset["storage_key"])


def _backdate(conn, asset_id, days):
    old = (datetime.now(timezone.utc) - timedelta(days=days)).replace(microsecond=0).isoformat()
    conn.execute("UPDATE assets SET created_at = %s WHERE id = %s", (old, asset_id))


def test_an_expired_asset_file_is_really_deleted(conn, storage, project):
    project_id, _, _ = project
    asset, path = _register(conn, storage, project_id)
    assert path.exists()
    _backdate(conn, asset["id"], 40)

    settings = _settings_from(storage)
    removed = service.cleanup_expired_assets(conn, storage, settings)
    assert removed == 1
    assert not path.exists(), "到期素材的文件必须真的被删掉"


def test_the_metadata_survives_but_the_sensitive_parts_are_cleared(conn, storage, project):
    project_id, _, _ = project
    asset, path = _register(
        conn, storage, project_id, name="机密剧名-第01集.mp4",
        declaration='{"owner": "某公司"}',
    )
    _backdate(conn, asset["id"], 40)

    settings = _settings_from(storage)
    service.cleanup_expired_assets(conn, storage, settings)

    row = conn.execute(
        "SELECT name, sha256, size_bytes, lifecycle, source_declaration FROM assets"
        " WHERE id = %s",
        (asset["id"],),
    ).fetchone()
    assert row["lifecycle"] == "deleted"
    # 留着：用于对账与排查。
    assert row["sha256"] == asset["sha256"]
    assert row["size_bytes"] == asset["size_bytes"]
    # 清掉：可能含敏感信息的内容。
    assert row["name"] == ""
    assert row["source_declaration"] is None


def test_a_fresh_asset_is_not_touched(conn, storage, project):
    project_id, _, _ = project
    asset, path = _register(conn, storage, project_id)
    settings = _settings_from(storage)

    assert service.cleanup_expired_assets(conn, storage, settings) == 0
    assert path.exists()
    row = conn.execute("SELECT lifecycle FROM assets WHERE id = %s", (asset["id"],)).fetchone()
    assert row["lifecycle"] == "ready"


def test_cleanup_is_idempotent(conn, storage, project):
    project_id, _, _ = project
    asset, _ = _register(conn, storage, project_id)
    _backdate(conn, asset["id"], 40)
    settings = _settings_from(storage)

    assert service.cleanup_expired_assets(conn, storage, settings) == 1
    assert service.cleanup_expired_assets(conn, storage, settings) == 0


def test_zero_retention_disables_asset_cleanup(conn, storage, project):
    """保留期配 0 表示「不自动清理」，不能理解成「立刻删」。"""
    project_id, _, _ = project
    asset, path = _register(conn, storage, project_id)
    _backdate(conn, asset["id"], 999)
    settings = _settings_from(storage, asset_retention_days=0)

    assert service.cleanup_expired_assets(conn, storage, settings) == 0
    assert path.exists()


def test_the_audit_record_does_not_leak_the_file_name(conn, storage, project):
    """审计里只能留文件名的哈希，不能留明文。"""
    project_id, _, _ = project
    asset, _ = _register(conn, storage, project_id, name="机密剧名.mp4")
    _backdate(conn, asset["id"], 40)
    settings = _settings_from(storage)

    service.cleanup_expired_assets(conn, storage, settings)
    rows = conn.execute(
        "SELECT detail_json FROM audit_events WHERE action = 'lifecycle.asset_expired'"
    ).fetchall()
    assert len(rows) == 1
    assert "机密剧名" not in (rows[0]["detail_json"] or "")


def test_an_abandoned_upload_loses_its_parts(conn, storage, project, settings):
    """传了一半就放弃的上传：分片目录要清掉。"""
    from drama_cloud import uploads as uploads_service

    project_id, user_id, _ = project
    session = uploads_service.create(
        conn, storage, project_id, user_id, "第 1 集", "ep01.mp4", 1024, "0" * 64, 1024
    )
    parts = settings.uploads_dir / session.id
    parts.mkdir(parents=True, exist_ok=True)
    (parts / "000000.part").write_bytes(b"x" * 1024)
    assert parts.exists()

    old = (datetime.now(timezone.utc) - timedelta(days=30)).replace(microsecond=0).isoformat()
    conn.execute("UPDATE upload_sessions SET updated_at = %s WHERE id = %s", (old, session.id))

    removed = service.cleanup_abandoned_uploads(conn, storage, settings)
    assert removed == 1
    assert not parts.exists() or not any(parts.iterdir())
    row = conn.execute(
        "SELECT status FROM upload_sessions WHERE id = %s", (session.id,)
    ).fetchone()
    assert row["status"] == "aborted"


def test_a_completed_upload_is_never_touched(conn, storage, project, settings):
    """已完成的上传不该被「放弃清理」误伤。"""
    from drama_cloud import uploads as uploads_service

    project_id, user_id, _ = project
    session = uploads_service.create(
        conn, storage, project_id, user_id, "第 1 集", "ep01.mp4", 4, _sha(b"data"), 1024
    )
    uploads_service.write_chunk(conn, storage, session, 0, b"data")
    uploads_service.complete(conn, storage, session)

    old = (datetime.now(timezone.utc) - timedelta(days=30)).replace(microsecond=0).isoformat()
    conn.execute("UPDATE upload_sessions SET updated_at = %s WHERE id = %s", (old, session.id))

    assert service.cleanup_abandoned_uploads(conn, storage, settings) == 0


def _sha(data: bytes) -> str:
    import hashlib

    return hashlib.sha256(data).hexdigest()


def _settings_from(storage, **overrides):
    """按这个 storage 指向的 home 造一份 settings，便于调保留期。"""
    import dataclasses

    return dataclasses.replace(storage.settings, **overrides) if overrides else storage.settings
