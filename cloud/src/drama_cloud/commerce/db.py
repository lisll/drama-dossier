"""PostgreSQL：连接与连接池、编号 SQL 迁移、审计日志。

连接一律开 autocommit。需要事务的地方显式写 `with conn.transaction():`，
避免「执行过一条查询就悄悄开了事务、一直不提交」。
"""

from __future__ import annotations

import hashlib
import re
from dataclasses import dataclass
from pathlib import Path
from typing import List, Optional

import psycopg
from psycopg.rows import dict_row
from psycopg.types.json import Jsonb
from psycopg_pool import ConnectionPool

MIGRATIONS_DIR = Path(__file__).resolve().parent / "migrations"
_MIGRATION_NAME = re.compile(r"^(\d{4})_[a-z0-9_]+\.sql$")
#: 迁移用的会话级咨询锁，防止两个进程同时迁移。取值是任意固定的整数。
_MIGRATION_LOCK = 20261015001
_SCHEMA_TABLE = """
CREATE TABLE IF NOT EXISTS schema_migrations (
    version     integer PRIMARY KEY,
    name        text NOT NULL,
    checksum    text NOT NULL,
    applied_at  timestamptz NOT NULL DEFAULT now()
)
"""


class MigrationError(RuntimeError):
    pass


@dataclass(frozen=True)
class Migration:
    version: int
    name: str
    sql: str
    checksum: str


def connect(dsn: str) -> psycopg.Connection:
    return psycopg.connect(dsn, autocommit=True, row_factory=dict_row)


def open_pool(dsn: str, min_size: int = 1, max_size: int = 10) -> ConnectionPool:
    """打开连接池；数据库连不上就在启动时报错，而不是等第一个请求才发现。"""
    pool = ConnectionPool(
        dsn,
        min_size=min_size,
        max_size=max_size,
        kwargs={"autocommit": True, "row_factory": dict_row},
        check=ConnectionPool.check_connection,
        open=False,
    )
    pool.open(wait=True, timeout=10.0)
    return pool


def discover(directory: Path = MIGRATIONS_DIR) -> List[Migration]:
    migrations = []
    for path in directory.glob("*.sql"):
        match = _MIGRATION_NAME.match(path.name)
        if match is None:
            raise MigrationError(f"迁移文件名不符合「0001_说明.sql」格式：{path.name}")
        sql = path.read_text(encoding="utf-8")
        checksum = hashlib.sha256(sql.encode("utf-8")).hexdigest()
        migrations.append(Migration(int(match.group(1)), path.name, sql, checksum))
    migrations.sort(key=lambda item: item.version)
    versions = [item.version for item in migrations]
    if len(versions) != len(set(versions)):
        raise MigrationError("迁移编号重复")
    return migrations


def migrate(dsn: str, directory: Path = MIGRATIONS_DIR) -> List[str]:
    """按编号执行还没执行过的迁移，每个迁移一个事务。返回本次执行的文件名。"""
    migrations = discover(directory)
    known = {item.version for item in migrations}
    executed = []
    with connect(dsn) as conn:
        conn.execute("SELECT pg_advisory_lock(%s)", (_MIGRATION_LOCK,))
        try:
            conn.execute(_SCHEMA_TABLE)
            applied = {
                row["version"]: row["checksum"]
                for row in conn.execute("SELECT version, checksum FROM schema_migrations")
            }
            unknown = sorted(set(applied) - known)
            if unknown:
                raise MigrationError(f"数据库里有程序不认识的迁移版本 {unknown}，程序可能比数据库旧")
            for migration in migrations:
                if migration.version in applied:
                    if applied[migration.version] != migration.checksum:
                        raise MigrationError(
                            f"已执行过的迁移被改动了：{migration.name}。新改动请写成新的迁移文件"
                        )
                    continue
                with conn.transaction():
                    conn.execute(migration.sql)
                    conn.execute(
                        "INSERT INTO schema_migrations (version, name, checksum) VALUES (%s, %s, %s)",
                        (migration.version, migration.name, migration.checksum),
                    )
                executed.append(migration.name)
        finally:
            conn.execute("SELECT pg_advisory_unlock(%s)", (_MIGRATION_LOCK,))
    return executed


def pending(conn, directory: Path = MIGRATIONS_DIR) -> List[str]:
    """还没执行的迁移文件名。服务启动时检查，数据库没迁移到位就拒绝启动。"""
    migrations = discover(directory)
    row = conn.execute("SELECT to_regclass('schema_migrations') IS NOT NULL AS ready").fetchone()
    applied = set()
    if row["ready"]:
        applied = {item["version"] for item in conn.execute("SELECT version FROM schema_migrations")}
    return [item.name for item in migrations if item.version not in applied]


def audit(conn, actor: str, action: str, subject: str = "", detail: Optional[dict] = None) -> None:
    """记一条审计日志。detail 里只放 ID 和计数，不放邮箱、令牌、验证码。"""
    conn.execute(
        "INSERT INTO audit_log (actor, action, subject, detail) VALUES (%s, %s, %s, %s)",
        (actor, action, subject, Jsonb(detail or {})),
    )
