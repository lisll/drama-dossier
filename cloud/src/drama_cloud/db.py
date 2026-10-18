"""PostgreSQL 数据层（M5b 起；此前是 SQLite）。

设计文档建议 PostgreSQL。M5b 之前本机没有 PG 服务，走的是 SQLite；
现在媒体控制面要支持多人使用、按用户隔离、可靠的并发事务，迁到 PG。

**为什么另起一套表结构而不并进 commerce 的库**：两者部署方式、生命周期、
备份策略都不同，硬合会牵动 commerce 已有的十个迁移与全部测试，风险大于收益。
分工是：账号、卡密、积分、支付在 commerce 库（`commerce/migrations/`）；
任务、素材、产物、复核队列在本库（`migrations/`）。

**读写约定**（都封在这个文件里，调用方不用自己拼 SQL）：

* `connect()` 返回 psycopg 连接，`row_factory=dict_row`，所以 `row["col"]` 到处都能用；
* `transaction(conn)` 用 psycopg 自带的事务（比 SQLite 的 `BEGIN IMMEDIATE` 更可靠）；
* JSON 一律以 TEXT 存，读写都过 `json_dump`/`json_load`；
* 时间以 ISO 8601 字符串存（TEXT），与迁移前的数据格式一致，不做无收益的类型切换。
"""

from __future__ import annotations

import contextlib
import json
from datetime import datetime, timezone
from pathlib import Path

import psycopg
from psycopg.rows import dict_row

from .config import Settings

#: 迁移文件目录。编号 SQL，按文件名排序执行，只跑一次。
MIGRATIONS_DIR = Path(__file__).resolve().parent / "migrations"

#: 迁移记账表。commerce 那边用的是同一种做法（`schema_migrations`）。
_MIGRATIONS_TABLE = """
CREATE TABLE IF NOT EXISTS schema_migrations (
    version    TEXT PRIMARY KEY,
    applied_at TEXT NOT NULL
)
"""


class MigrationError(RuntimeError):
    """迁移失败。消息里只出现文件名与数据库给的原因。"""


def now_iso() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat()


def json_dump(value) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def json_load(value, default=None):
    if value is None or value == "":
        return default
    return json.loads(value)


def connect(settings: Settings) -> psycopg.Connection:
    """打开一个连接。调用方负责关闭（或用 `with connect(...) as conn`）。"""
    if not settings.dsn:
        raise RuntimeError(
            "没有配置数据库：请设置 DRAMA_CLOUD_DSN（PostgreSQL 连接串），"
            "开发环境见 cloud/docs/DEVELOPMENT.md"
        )
    settings.ensure_dirs()
    conn = psycopg.connect(settings.dsn, row_factory=dict_row, autocommit=True)
    return conn


def _applied(conn) -> set:
    rows = conn.execute(f"SELECT version FROM schema_migrations").fetchall()
    return {row["version"] for row in rows}


def migrate(dsn: str) -> list:
    """执行尚未应用的迁移，返回本次应用的文件名。

    幂等：已应用的跳过；每个迁移在**独立事务**里执行并记进 `schema_migrations`，
    所以中途失败不会留下半套表。
    """
    applied = []
    MIGRATIONS_DIR.mkdir(parents=True, exist_ok=True)
    files = sorted(MIGRATIONS_DIR.glob("*.sql"))
    with psycopg.connect(dsn, row_factory=dict_row, autocommit=True) as conn:
        conn.execute(_MIGRATIONS_TABLE)
        done = _applied(conn)
        for path in files:
            version = path.name
            if version in done:
                continue
            sql = path.read_text(encoding="utf-8")
            try:
                with conn.transaction():
                    conn.execute(sql)
                    conn.execute(
                        "INSERT INTO schema_migrations (version, applied_at) VALUES (%s, %s)",
                        (version, now_iso()),
                    )
            except psycopg.Error as error:
                raise MigrationError(f"迁移 {version} 失败：{error}") from error
            applied.append(version)
    return applied


def init_db(settings: Settings) -> str:
    """建库（执行迁移）并返回连接串。可重复执行。"""
    if not settings.dsn:
        raise RuntimeError("没有配置数据库：请设置 DRAMA_CLOUD_DSN")
    settings.ensure_dirs()
    migrate(settings.dsn)
    return settings.dsn


def pending(settings: Settings) -> list:
    """还有哪些迁移没跑（给 CLI 与健康检查看）。"""
    if not settings.dsn:
        return [path.name for path in sorted(MIGRATIONS_DIR.glob("*.sql"))]
    with psycopg.connect(settings.dsn, row_factory=dict_row, autocommit=True) as conn:
        conn.execute(_MIGRATIONS_TABLE)
        done = _applied(conn)
    return [p.name for p in sorted(MIGRATIONS_DIR.glob("*.sql")) if p.name not in done]


@contextlib.contextmanager
def transaction(conn):
    """显式事务。调度器与任务服务都依赖它保证状态迁移的原子性。

    psycopg 的 `conn.transaction()` 在进入时 BEGIN、正常退出时 COMMIT、
    异常时 ROLLBACK。连接本身是 autocommit，所以嵌套调用会变成 SAVEPOINT，
    行为与原来的 `BEGIN IMMEDIATE` 一致且更安全（不会有人忘了 COMMIT）。
    """
    with conn.transaction():
        yield conn


def audit(conn, actor: str, action: str, subject: str = "", detail=None) -> None:
    """写审计记录（配方发布/回滚、任务取消、权限变化等关键操作）。"""
    conn.execute(
        "INSERT INTO audit_events (ts, actor, action, subject, detail_json) VALUES (%s,%s,%s,%s,%s)",
        (now_iso(), actor, action, subject, json_dump(detail) if detail is not None else None),
    )
