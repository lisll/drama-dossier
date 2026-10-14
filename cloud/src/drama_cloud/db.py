"""SQLite 数据层。

设计文档建议 PostgreSQL；本机没有 PG 服务，改用 SQLite。差异被限制在这个文件与
`docs/DATABASE_SCHEMA.md` 里：DDL 只用到两边都有的类型（TEXT/INTEGER/REAL），
换 PG 时只需要把 `AUTOINCREMENT` 换成 `GENERATED ... AS IDENTITY`、把 `?` 占位符换成 `%s`。
"""

from __future__ import annotations

import contextlib
import json
import sqlite3
from datetime import datetime, timezone
from pathlib import Path

from .config import Settings

#: 文档 §9 的核心实体。JSON 字段一律以 TEXT 存储，读写都经过 json_dump/json_load，
#: 这样将来迁到 PG 的 jsonb 时不需要改调用点。
SCHEMA = """
CREATE TABLE IF NOT EXISTS users (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    role          TEXT NOT NULL CHECK (role IN ('admin', 'operator', 'viewer')),
    token_hash    TEXT NOT NULL UNIQUE,
    created_at    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS projects (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    owner_id      TEXT NOT NULL REFERENCES users(id),
    created_at    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS assets (
    id                 TEXT PRIMARY KEY,
    project_id         TEXT NOT NULL REFERENCES projects(id),
    name               TEXT NOT NULL,
    storage_key        TEXT NOT NULL,
    sha256             TEXT NOT NULL,
    size_bytes         INTEGER NOT NULL,
    probe_json         TEXT,
    source_declaration TEXT,
    lifecycle          TEXT NOT NULL DEFAULT 'ready'
                       CHECK (lifecycle IN ('uploading', 'ready', 'rejected', 'deleted')),
    created_at         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_assets_project ON assets(project_id);

CREATE TABLE IF NOT EXISTS recipes (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL,
    purpose           TEXT NOT NULL,
    published_version INTEGER,
    created_at        TEXT NOT NULL
);

-- 每次发布生成不可变版本：同一 (recipe_id, version) 不允许被覆盖。
CREATE TABLE IF NOT EXISTS recipe_versions (
    recipe_id            TEXT NOT NULL REFERENCES recipes(id),
    version              INTEGER NOT NULL,
    status               TEXT NOT NULL
                         CHECK (status IN ('draft', 'reviewing', 'published', 'deprecated', 'revoked')),
    content_json         TEXT NOT NULL,
    content_sha256       TEXT NOT NULL,
    schema_version       TEXT NOT NULL,
    min_worker_version   TEXT NOT NULL,
    required_capabilities TEXT NOT NULL,
    rollout_percent      INTEGER NOT NULL DEFAULT 100,
    reviewed_by          TEXT,
    published_at         TEXT,
    created_at           TEXT NOT NULL,
    PRIMARY KEY (recipe_id, version)
);

-- 状态取值见 tasks/state.py 的 TaskState；这里用 CHECK 兜底，防止绕过状态机写入。
CREATE TABLE IF NOT EXISTS tasks (
    id                     TEXT PRIMARY KEY,
    project_id             TEXT NOT NULL REFERENCES projects(id),
    state                  TEXT NOT NULL CHECK (state IN (
                               'PENDING', 'QUEUED', 'RUNNING', 'VERIFYING', 'SUCCEEDED',
                               'RETRY_WAITING', 'CANCEL_REQUESTED', 'CANCELLED', 'FAILED')),
    priority               INTEGER NOT NULL DEFAULT 100,
    idempotency_key        TEXT UNIQUE,
    input_asset_ids        TEXT NOT NULL,
    recipe_id              TEXT NOT NULL,
    recipe_version         INTEGER NOT NULL,
    recipe_sha256          TEXT NOT NULL,
    -- 配方快照：任务创建时冻结，执行期间不跟随发布指针变化（§6.1）。
    recipe_snapshot_json   TEXT NOT NULL DEFAULT '{}',
    -- 解析后的 ProcessingPlan（步骤、参数、输入角色），Worker 直接照它执行。
    plan_json              TEXT NOT NULL DEFAULT '{}',
    required_capabilities  TEXT NOT NULL DEFAULT '[]',
    target_json            TEXT NOT NULL,
    user_params_json       TEXT NOT NULL DEFAULT '{}',
    cancel_requested       INTEGER NOT NULL DEFAULT 0,
    attempts               INTEGER NOT NULL DEFAULT 0,
    progress_json          TEXT NOT NULL DEFAULT '{}',
    next_run_at            TEXT,
    lease_owner            TEXT,
    lease_expires_at       TEXT,
    error_class            TEXT,
    error_message          TEXT,
    result_json            TEXT,
    created_at             TEXT NOT NULL,
    started_at             TEXT,
    finished_at            TEXT,
    updated_at             TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tasks_state ON tasks(state, priority, created_at);

CREATE TABLE IF NOT EXISTS task_attempts (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id       TEXT NOT NULL REFERENCES tasks(id),
    attempt       INTEGER NOT NULL,
    worker_id     TEXT,
    started_at    TEXT NOT NULL,
    finished_at   TEXT,
    error_class   TEXT,
    error_message TEXT,
    UNIQUE (task_id, attempt)
);

-- 阶段级检查点：第一版不恢复任意 FFmpeg 进程的内部状态，只记录阶段产物与指纹。
CREATE TABLE IF NOT EXISTS task_checkpoints (
    task_id        TEXT NOT NULL REFERENCES tasks(id),
    stage          TEXT NOT NULL,
    input_sha256   TEXT NOT NULL,
    recipe_version INTEGER NOT NULL,
    engine_version TEXT NOT NULL,
    params_digest  TEXT NOT NULL,
    artifact_key   TEXT,
    artifact_sha256 TEXT,
    completed      INTEGER NOT NULL DEFAULT 0,
    updated_at     TEXT NOT NULL,
    PRIMARY KEY (task_id, stage)
);

CREATE TABLE IF NOT EXISTS artifacts (
    id            TEXT PRIMARY KEY,
    task_id       TEXT NOT NULL REFERENCES tasks(id),
    kind          TEXT NOT NULL,
    storage_key   TEXT NOT NULL,
    sha256        TEXT NOT NULL,
    size_bytes    INTEGER NOT NULL,
    media_json    TEXT,
    quality_json  TEXT,
    retain_until  TEXT,
    created_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_artifacts_task ON artifacts(task_id);

CREATE TABLE IF NOT EXISTS audit_events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    ts         TEXT NOT NULL,
    actor      TEXT NOT NULL,
    action     TEXT NOT NULL,
    subject    TEXT,
    detail_json TEXT
);
CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_events(ts);
"""

#: 旧库升级用：库已存在时 `CREATE TABLE IF NOT EXISTS` 不会补列，所以显式声明要补的列。
#: 学习原型直接用 ALTER TABLE ADD COLUMN（SQLite 允许），换 PG 时这里替换成迁移工具。
MIGRATIONS = {
    "tasks": {
        "recipe_snapshot_json": "TEXT NOT NULL DEFAULT '{}'",
        "plan_json": "TEXT NOT NULL DEFAULT '{}'",
        "required_capabilities": "TEXT NOT NULL DEFAULT '[]'",
        "progress_json": "TEXT NOT NULL DEFAULT '{}'",
        "started_at": "TEXT",
        "finished_at": "TEXT",
    },
}


def now_iso() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat()


def json_dump(value) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def json_load(value, default=None):
    if value is None or value == "":
        return default
    return json.loads(value)


def connect(settings: Settings) -> sqlite3.Connection:
    settings.ensure_dirs()
    conn = sqlite3.connect(str(settings.db_path), timeout=30.0, isolation_level=None)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA foreign_keys=ON")
    conn.execute("PRAGMA busy_timeout=30000")
    return conn


def _ensure_columns(conn: sqlite3.Connection) -> None:
    """给已存在的表补上后加的列，避免学习者每次改 schema 都要删库重建。"""
    for table, columns in MIGRATIONS.items():
        existing = {row["name"] for row in conn.execute(f"PRAGMA table_info({table})")}
        for name, ddl in columns.items():
            if name not in existing:
                conn.execute(f"ALTER TABLE {table} ADD COLUMN {name} {ddl}")


def init_db(settings: Settings) -> Path:
    conn = connect(settings)
    try:
        conn.executescript(SCHEMA)
        _ensure_columns(conn)
    finally:
        conn.close()
    return settings.db_path


@contextlib.contextmanager
def transaction(conn: sqlite3.Connection):
    """显式事务。调度器与任务服务都依赖它保证状态迁移的原子性。"""
    conn.execute("BEGIN IMMEDIATE")
    try:
        yield conn
    except Exception:
        conn.execute("ROLLBACK")
        raise
    else:
        conn.execute("COMMIT")


def audit(conn, actor: str, action: str, subject: str = "", detail=None) -> None:
    """写审计记录（配方发布/回滚、任务取消、权限变化等关键操作）。"""
    conn.execute(
        "INSERT INTO audit_events (ts, actor, action, subject, detail_json) VALUES (?,?,?,?,?)",
        (now_iso(), actor, action, subject, json_dump(detail) if detail is not None else None),
    )
