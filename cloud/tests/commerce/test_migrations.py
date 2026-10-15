"""迁移：可重复执行、改动过的旧迁移被拒、失败整体回滚、数据库没迁移到位时服务拒绝启动。"""

from __future__ import annotations

import dataclasses

import psycopg
import pytest
from fastapi.testclient import TestClient

from drama_cloud.commerce.api import create_app
from drama_cloud.commerce.db import MigrationError, connect, discover, migrate, pending

from .conftest import FAST_HASHER, MemoryMailer


def test_migrate_is_idempotent(scratch_dsn):
    names = [item.name for item in discover()]
    assert names[:2] == ["0001_accounts.sql", "0002_licensing.sql"]
    assert migrate(scratch_dsn) == names
    assert migrate(scratch_dsn) == []
    with connect(scratch_dsn) as conn:
        tables = {
            row["tablename"]
            for row in conn.execute("SELECT tablename FROM pg_tables WHERE schemaname = 'public'")
        }
        assert pending(conn) == []
    assert {"users", "email_codes", "sessions", "access_tokens", "refresh_tokens", "rate_limits",
            "audit_log", "schema_migrations", "card_batches", "cards", "entitlements", "devices"} <= tables


def test_licensing_schema_backstops(pg):
    """应用层之外，表结构本身也挡住几类坏数据。"""
    user = pg.execute(
        "INSERT INTO users (email, password_hash, terms_version, terms_accepted_at)"
        " VALUES ('c@example.com', 'x', 'v', now()) RETURNING id"
    ).fetchone()["id"]
    device = "f" * 64
    insert = ("INSERT INTO devices (user_id, device_id, name, platform, app_version)"
              " VALUES (%s, %s, 'n', 'linux', '1.0')")
    pg.execute(insert, (user, device))
    with pytest.raises(psycopg.errors.UniqueViolation):
        pg.execute(insert, (user, device))  # 同一台电脑不能有两条有效绑定
    pg.execute("UPDATE devices SET unbound_at = now(), unbind_reason = 'user'")
    pg.execute(insert, (user, device))  # 解绑以后可以重新绑定，历史行保留
    assert pg.execute("SELECT count(*) AS n FROM devices").fetchone()["n"] == 2
    bad_statements = [
        (insert, (user, "F" * 64)),
        (insert, (user, "f" * 63)),
        ("UPDATE devices SET unbound_at = now() WHERE unbound_at IS NULL", None),  # 解绑必须写原因
        ("INSERT INTO entitlements (user_id, kind) VALUES (%s, 'timed')", (user,)),
        ("INSERT INTO entitlements (user_id, kind, expires_at) VALUES (%s, 'lifetime', now())", (user,)),
        ("INSERT INTO card_batches (name, kind, days, created_by) VALUES ('b', 'lifetime', 30, 't')", None),
        ("INSERT INTO card_batches (name, kind, created_by) VALUES ('b', 'timed', 't')", None),
    ]
    for statement, params in bad_statements:
        with pytest.raises(psycopg.errors.CheckViolation):
            pg.execute(statement, params)


def test_service_refuses_to_start_before_migrations(scratch_dsn, commerce_settings):
    settings = dataclasses.replace(commerce_settings, dsn=scratch_dsn)
    with pytest.raises(MigrationError, match="drama-commerce migrate"):
        create_app(settings, mailer=MemoryMailer(), hasher=FAST_HASHER)
    migrate(scratch_dsn)
    # 迁移之后能正常启动；连接池由应用自己打开，退出时自己关闭。
    with TestClient(create_app(settings, mailer=MemoryMailer(), hasher=FAST_HASHER)) as client:
        assert client.get("/api/v1/health").json() == {"ok": True}


def test_changed_or_unknown_migration_rejected(scratch_dsn, tmp_path):
    path = tmp_path / "0001_demo.sql"
    path.write_text("CREATE TABLE demo (id integer);", encoding="utf-8")
    assert migrate(scratch_dsn, tmp_path) == ["0001_demo.sql"]
    path.write_text("CREATE TABLE demo (id bigint);", encoding="utf-8")
    with pytest.raises(MigrationError, match="被改动"):
        migrate(scratch_dsn, tmp_path)
    path.unlink()
    with pytest.raises(MigrationError, match="不认识"):
        migrate(scratch_dsn, tmp_path)


def test_failed_migration_rolls_back_completely(scratch_dsn, tmp_path):
    (tmp_path / "0001_broken.sql").write_text(
        "CREATE TABLE half_done (id integer);\nSELECT * FROM no_such_table;\n", encoding="utf-8"
    )
    with pytest.raises(psycopg.errors.UndefinedTable):
        migrate(scratch_dsn, tmp_path)
    with connect(scratch_dsn) as conn:
        assert conn.execute("SELECT to_regclass('half_done') IS NULL AS gone").fetchone()["gone"]
        assert conn.execute("SELECT count(*) AS n FROM schema_migrations").fetchone()["n"] == 0
