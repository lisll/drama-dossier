"""账号服务的测试夹具。

数据库测试要用真实 PostgreSQL：设置环境变量 COMMERCE_TEST_DSN，指向一个有建库权限的账号，
例如开发环境 compose 里的 `postgresql://drama:drama-dev-only@127.0.0.1:5432/postgres`。
每次测试会话新建一个临时库并执行迁移，结束后删掉。没设这个变量时，数据库测试自动跳过。
"""

from __future__ import annotations

import contextlib
import os
import re
import secrets
import time
from dataclasses import dataclass, field
from typing import List

import psycopg
import pytest
from argon2 import PasswordHasher
from fastapi.testclient import TestClient
from psycopg import sql
from psycopg.conninfo import make_conninfo

from drama_cloud.commerce.api import create_app
from drama_cloud.commerce.config import CommerceSettings, SmtpSettings
from drama_cloud.commerce.db import migrate, open_pool
from drama_cloud.commerce.mailer import MailError

TEST_DSN_ENV = "COMMERCE_TEST_DSN"
CODE_SECRET = "test-code-secret-0123456789abcdef-xyz"
#: 测试用的低开销参数，只为跑得快；生产用 argon2-cffi 的默认参数。
FAST_HASHER = PasswordHasher(time_cost=1, memory_cost=8, parallelism=1)
TABLES = ("audit_log", "rate_limits", "refresh_tokens", "access_tokens", "sessions", "email_codes", "users")
_CODE_IN_BODY = re.compile(r"验证码是：([0-9]{6})")


def extract_code(text: str) -> str:
    match = _CODE_IN_BODY.search(text)
    assert match, "邮件正文里没有找到验证码"
    return match.group(1)


def auth(token: str) -> dict:
    return {"Authorization": f"Bearer {token}"}


@contextlib.contextmanager
def temporary_database():
    admin_dsn = os.environ.get(TEST_DSN_ENV)
    if not admin_dsn:
        pytest.skip(f"需要 PostgreSQL：设置 {TEST_DSN_ENV}（见 cloud/docs/COMMERCE.md）")
    name = "commerce_test_" + secrets.token_hex(4)
    with psycopg.connect(admin_dsn, autocommit=True) as admin:
        admin.execute(sql.SQL("CREATE DATABASE {}").format(sql.Identifier(name)))
    try:
        yield make_conninfo(admin_dsn, dbname=name)
    finally:
        with psycopg.connect(admin_dsn, autocommit=True) as admin:
            admin.execute(sql.SQL("DROP DATABASE IF EXISTS {} WITH (FORCE)").format(sql.Identifier(name)))


@pytest.fixture(scope="session")
def database_dsn():
    with temporary_database() as dsn:
        migrate(dsn)
        yield dsn


@pytest.fixture()
def scratch_dsn():
    """一个没执行过迁移的空库，给迁移测试随便折腾。"""
    with temporary_database() as dsn:
        yield dsn


@pytest.fixture(scope="session")
def pool(database_dsn):
    pool = open_pool(database_dsn, min_size=1, max_size=4)
    yield pool
    pool.close()


@pytest.fixture()
def pg(pool):
    """先清空所有表，再把一个 autocommit 连接交给测试，用来直接查库、改时间。"""
    with pool.connection() as conn:
        conn.execute("TRUNCATE " + ", ".join(TABLES) + " RESTART IDENTITY CASCADE")
        yield conn


@dataclass
class MemoryMailer:
    """把邮件存在内存里的发信器；fail=True 时模拟 SMTP 故障。"""

    sent: List[dict] = field(default_factory=list)
    fail: bool = False

    def send(self, to: str, subject: str, body: str) -> None:
        if self.fail:
            raise MailError("模拟的 SMTP 故障")
        self.sent.append({"to": to, "subject": subject, "body": body})

    def last_code(self, to: str) -> str:
        messages = [item for item in self.sent if item["to"] == to]
        assert messages, f"没有发给 {to} 的邮件"
        return extract_code(messages[-1]["body"])


@pytest.fixture()
def mailer():
    return MemoryMailer()


@pytest.fixture(scope="session")
def commerce_settings(database_dsn):
    return CommerceSettings(
        dsn=database_dsn,
        code_secret=CODE_SECRET,
        smtp=SmtpSettings(host="127.0.0.1", port=1025, security="plain", sender="no-reply@drama.test"),
    )


@pytest.fixture()
def client(commerce_settings, pool, pg, mailer):
    app = create_app(commerce_settings, pool=pool, mailer=mailer, hasher=FAST_HASHER)
    with TestClient(app) as test_client:
        yield test_client


@pytest.fixture()
def register(client, mailer):
    """注册一个账号（邮箱要传规范的小写形式），返回注册接口的响应体。"""

    def _register(email: str = "user@example.com", password: str = "correct-horse-1") -> dict:
        sent = client.post("/api/v1/auth/codes", json={"email": email, "purpose": "register"})
        assert sent.status_code == 202, sent.text
        response = client.post(
            "/api/v1/auth/register",
            json={"email": email, "code": mailer.last_code(email), "password": password, "accept_terms": True},
        )
        assert response.status_code == 201, response.text
        return response.json()

    return _register


@pytest.fixture()
def fresh_window(pg):
    """限流按固定时间窗计数。离窗口结束太近就先等到下一个窗口，免得测试跑到一半换了窗。"""

    def _wait(window_seconds: int, needed_seconds: int = 10) -> None:
        left = pg.execute(
            "SELECT %s::int - (extract(epoch FROM now()) %% %s::int) AS left",
            (window_seconds, window_seconds),
        ).fetchone()["left"]
        if left < needed_seconds:
            time.sleep(float(left) + 0.5)

    return _wait
