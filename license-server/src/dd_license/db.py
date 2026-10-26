"""SQLite 访问：建表迁移、连接、写事务。

所有 SQL 都用参数化查询；写事务用 BEGIN IMMEDIATE，让并发写操作串行，
杜绝"两人同时兑换同一张卡"。
"""

from __future__ import annotations

import json
import sqlite3
import threading
import time
from contextlib import contextmanager
from pathlib import Path

SCHEMA_VERSION = 1

_SCHEMA = """
CREATE TABLE IF NOT EXISTS batches (
    id          INTEGER PRIMARY KEY,
    plan        TEXT    NOT NULL,
    days        INTEGER,
    count       INTEGER NOT NULL,
    note        TEXT    NOT NULL DEFAULT '',
    created_by  TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    revoked_at  INTEGER
);
CREATE TABLE IF NOT EXISTS cards (
    digest        TEXT    PRIMARY KEY,
    batch_id      INTEGER NOT NULL REFERENCES batches(id),
    plan          TEXT    NOT NULL,
    days          INTEGER,
    created_at    INTEGER NOT NULL,
    redeemed_mid  TEXT,
    redeemed_at   INTEGER,
    revoked_at    INTEGER,
    CHECK ((redeemed_mid IS NULL) = (redeemed_at IS NULL))
);
CREATE INDEX IF NOT EXISTS idx_cards_batch ON cards(batch_id);
CREATE INDEX IF NOT EXISTS idx_cards_mid ON cards(redeemed_mid);
CREATE TABLE IF NOT EXISTS machines (
    id                  INTEGER PRIMARY KEY,
    mid                 TEXT    NOT NULL UNIQUE,
    plan                TEXT,
    until               INTEGER,
    first_activated_at  INTEGER,
    last_seen_at        INTEGER NOT NULL,
    last_app_version    TEXT    NOT NULL DEFAULT '',
    revoked_at          INTEGER,
    revoke_reason       TEXT
);
CREATE TABLE IF NOT EXISTS audit (
    id           INTEGER PRIMARY KEY,
    at           INTEGER NOT NULL,
    actor        TEXT    NOT NULL,
    action       TEXT    NOT NULL,
    ip           TEXT    NOT NULL DEFAULT '',
    mid_prefix   TEXT,
    card_prefix  TEXT,
    detail       TEXT    NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_audit_at ON audit(at);
CREATE TABLE IF NOT EXISTS admins (
    id             INTEGER PRIMARY KEY,
    username       TEXT    NOT NULL UNIQUE,
    pw_hash        TEXT    NOT NULL,
    created_at     INTEGER NOT NULL,
    disabled_at    INTEGER,
    last_login_at  INTEGER,
    failed_count   INTEGER NOT NULL DEFAULT 0,
    locked_until   INTEGER
);
CREATE TABLE IF NOT EXISTS admin_sessions (
    token_digest  TEXT    PRIMARY KEY,
    admin_id      INTEGER NOT NULL REFERENCES admins(id),
    csrf          TEXT    NOT NULL,
    created_at    INTEGER NOT NULL,
    last_used_at  INTEGER NOT NULL,
    expires_at    INTEGER NOT NULL,
    ip            TEXT    NOT NULL
);
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
"""


def now() -> int:
    return int(time.time())


class Database:
    """每个线程一个连接；FastAPI 的同步路由跑在线程池里。"""

    def __init__(self, path: Path | str):
        self.path = str(path)
        self._local = threading.local()

    def connect(self) -> sqlite3.Connection:
        conn = getattr(self._local, "conn", None)
        if conn is None:
            conn = sqlite3.connect(self.path, isolation_level=None, timeout=15, check_same_thread=False)
            conn.row_factory = sqlite3.Row
            conn.execute("PRAGMA foreign_keys = ON")
            conn.execute("PRAGMA busy_timeout = 15000")
            if self.path != ":memory:":
                conn.execute("PRAGMA journal_mode = WAL")
                conn.execute("PRAGMA synchronous = FULL")
            self._local.conn = conn
        return conn

    def migrate(self) -> None:
        if self.path != ":memory:":
            Path(self.path).parent.mkdir(parents=True, exist_ok=True)
        conn = self.connect()
        conn.executescript(_SCHEMA)
        row = conn.execute("SELECT version FROM schema_version").fetchone()
        if row is None:
            conn.execute("INSERT INTO schema_version(version) VALUES (?)", (SCHEMA_VERSION,))
        elif row["version"] > SCHEMA_VERSION:
            raise RuntimeError(f"数据库版本 {row['version']} 比程序新，拒绝启动")

    @contextmanager
    def write(self):
        conn = self.connect()
        conn.execute("BEGIN IMMEDIATE")
        try:
            yield conn
        except BaseException:
            conn.execute("ROLLBACK")
            raise
        else:
            conn.execute("COMMIT")

    def read(self) -> sqlite3.Connection:
        return self.connect()


def audit(
    conn: sqlite3.Connection,
    actor: str,
    action: str,
    ip: str = "",
    mid: str | None = None,
    card_digest: str | None = None,
    **detail,
) -> None:
    conn.execute(
        "INSERT INTO audit(at, actor, action, ip, mid_prefix, card_prefix, detail) VALUES (?,?,?,?,?,?,?)",
        (
            now(),
            actor,
            action,
            ip or "",
            mid[:16] if mid else None,
            card_digest[:8] if card_digest else None,
            json.dumps(detail, ensure_ascii=False, sort_keys=True),
        ),
    )
