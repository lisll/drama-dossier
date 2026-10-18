"""测试用的临时 PostgreSQL 库。

commerce 与媒体控制面（M5b 起）都需要真 PostgreSQL。两边的测试各建一个随机名的
库、跑完删掉，互不干扰。这段逻辑抽出来共用，避免两份实现慢慢跑偏。

环境变量 `COMMERCE_TEST_DSN` 指向一个**有建库权限**的账号，
例如开发环境 compose 里的 `postgresql://drama:drama-dev-only@127.0.0.1:5432/postgres`。
没设时相关测试自动跳过（而不是失败），方便只想跑纯逻辑测试的人。
"""

from __future__ import annotations

import contextlib
import os
import secrets

import psycopg
import pytest
from psycopg import sql
from psycopg.conninfo import make_conninfo

TEST_DSN_ENV = "COMMERCE_TEST_DSN"


@contextlib.contextmanager
def temporary_database(prefix: str = "commerce_test_"):
    admin_dsn = os.environ.get(TEST_DSN_ENV)
    if not admin_dsn:
        pytest.skip(f"需要 PostgreSQL：设置 {TEST_DSN_ENV}（见 cloud/docs/COMMERCE.md）")
    name = prefix + secrets.token_hex(4)
    with psycopg.connect(admin_dsn, autocommit=True) as admin:
        admin.execute(sql.SQL("CREATE DATABASE {}").format(sql.Identifier(name)))
    try:
        yield make_conninfo(admin_dsn, dbname=name)
    finally:
        # WITH (FORCE) 只作用于**本测试自己刚建的**这个随机库，
        # 清理时它可能还有未断开的连接（Worker 线程），强制断开是安全的。
        with psycopg.connect(admin_dsn, autocommit=True) as admin:
            admin.execute(
                sql.SQL("DROP DATABASE IF EXISTS {} WITH (FORCE)").format(sql.Identifier(name))
            )
