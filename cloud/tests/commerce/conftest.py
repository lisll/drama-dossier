"""账号服务的测试夹具。

数据库测试要用真实 PostgreSQL：设置环境变量 COMMERCE_TEST_DSN，指向一个有建库权限的账号，
例如开发环境 compose 里的 `postgresql://drama:drama-dev-only@127.0.0.1:5432/postgres`。
每次测试会话新建一个临时库并执行迁移，结束后删掉。没设这个变量时，数据库测试自动跳过。
"""

from __future__ import annotations

import contextlib
import hashlib
import os
import re
import secrets
import threading
import time
from dataclasses import dataclass, field
from typing import List, Optional

import psycopg
import pytest
from argon2 import PasswordHasher
from fastapi.testclient import TestClient
from psycopg import sql
from psycopg.conninfo import make_conninfo

from ..pg import temporary_database
from drama_cloud.commerce import cards
from drama_cloud.commerce.api import create_app
from drama_cloud.commerce.config import CommerceSettings, SmtpSettings
from drama_cloud.commerce.db import migrate, open_pool
from drama_cloud.commerce.license_token import Signer, generate_key_file
from drama_cloud.commerce.mailer import MailError

TEST_DSN_ENV = "COMMERCE_TEST_DSN"
CODE_SECRET = "test-code-secret-0123456789abcdef-xyz"#: Mock 支付回调的签名密钥（测试用固定值）。
MOCK_PAY_SECRET = "test-mock-pay-secret-0123456789"
#: 测试用的低开销参数，只为跑得快；生产用 argon2-cffi 的默认参数。
FAST_HASHER = PasswordHasher(time_cost=1, memory_cost=8, parallelism=1)
TABLES = (
    # M7：管理员与运营内容
    "admin_tokens", "admin_sessions", "admins",
    "announcements", "feature_flags", "version_policy", "client_errors",
    "invite_rewards", "invite_settings", "payment_orders", "recharge_tiers",
    "credit_ledger", "price_rules",
    "audit_log", "rate_limits", "refresh_tokens", "access_tokens", "sessions", "email_codes",
    "devices", "entitlements", "cards", "card_batches", "users",
)
_CODE_IN_BODY = re.compile(r"验证码是：([0-9]{6})")


def extract_code(text: str) -> str:
    match = _CODE_IN_BODY.search(text)
    assert match, "邮件正文里没有找到验证码"
    return match.group(1)


def auth(token: str) -> dict:
    return {"Authorization": f"Bearer {token}"}


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
    # 并发测试里会有几个请求同时占着连接等锁，连接池要留够余量。
    pool = open_pool(database_dsn, min_size=1, max_size=8)
    yield pool
    pool.close()


@pytest.fixture()
def pg(pool):
    """先清空所有表，再把一个 autocommit 连接交给测试，用来直接查库、改时间。"""
    with pool.connection() as conn:
        conn.execute("TRUNCATE " + ", ".join(TABLES) + " RESTART IDENTITY CASCADE")
        # price_rules 是迁移里的种子数据，被 TRUNCATE 清掉了要补回去：
        # 积分相关测试依赖它，而且「缺价目」本身是另一种要单独测的状态。
        conn.execute(
            "INSERT INTO price_rules (subject, credits_per_unit, min_units) VALUES"
            " ('version', 5, 1), ('package', 5, 1), ('server_version', 5, 1)"
        )
        # 充值档位同理。
        conn.execute(
            "INSERT INTO recharge_tiers (id, amount_fen, credits, sort_order) VALUES"
            " ('tier-100', 1000, 100, 10), ('tier-500', 5000, 550, 20),"
            " ('tier-2000', 20000, 2400, 30)"
        )
        # 邀请奖励配置（单行表）。
        conn.execute(
            "INSERT INTO invite_settings (inviter_credits, invitee_credits, max_rewards)"
            " VALUES (50, 20, 20)"
        )
        # M7：版本策略与功能开关也是迁移里的种子数据，被 TRUNCATE 清掉了要补回去。
        # 版本策略默认「不强制、不提示」——刚上线就把所有人拦住是最糟的默认。
        conn.execute("INSERT INTO version_policy (id) VALUES (1)")
        conn.execute(
            "INSERT INTO feature_flags (key, enabled, note) VALUES"
            " ('signup', true, '注册开关'),"
            " ('recharge', true, '充值入口'),"
            " ('server_processing', true, '服务端处理'),"
            " ('version_task', true, '本机处理版本')"
        )
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
def license_key_file(tmp_path_factory):
    """测试会话专用的授权签名私钥（每次现生成，不进仓库）。"""
    path = tmp_path_factory.mktemp("license") / "license-ed25519.pem"
    generate_key_file(path)
    return str(path)


@pytest.fixture(scope="session")
def signer(license_key_file):
    return Signer.from_pem_file(license_key_file)


@pytest.fixture(scope="session")
def commerce_settings(database_dsn, license_key_file):
    return CommerceSettings(
        dsn=database_dsn,
        code_secret=CODE_SECRET,
        smtp=SmtpSettings(host="127.0.0.1", port=1025, security="plain", sender="no-reply@drama.test"),
        license_key_file=license_key_file,
        # Mock 支付的回调签名密钥：不给就报「未配置」，所以测试里显式给一个。
        mock_pay_secret=MOCK_PAY_SECRET,
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


# ---------- 卡密、设备、并发 ----------


def device_id(label: str) -> str:
    """测试用机器码：64 位小写十六进制，同一个 label 永远得到同一个值。"""
    return hashlib.sha256(label.encode("utf-8")).hexdigest()


def activate(client, token: str, device: str, **overrides):
    body = {"device_id": device, "name": "测试电脑", "platform": "windows", "app_version": "0.3.0"}
    body.update(overrides)
    return client.post("/api/v1/license/activate", json=body, headers=auth(token))


def new_cards(pg, count: int = 1, days: Optional[int] = 30, name: str = "测试批次") -> List[str]:
    """直接在库里生成一批卡密，返回明文（真实环境只能从导出的 CSV 拿到明文）。"""
    codes = [cards.generate_code() for _ in range(count)]
    cards.create_batch(pg, name=name, days=days, codes=codes, operator="test")
    return codes


@contextlib.contextmanager
def rival_lock(database_dsn: str, pg, lock_sql: str, params, waiting_fragment: str, waiters: int):
    """在另一个连接里锁住一行，等 waiters 个请求都卡在含 waiting_fragment 的语句上，再提交放行。

    用来把「同时」变成确定的事：几个请求一定是在同一把锁前排队，而不是碰巧先后到达。
    yield 出来的列表非空，说明确实等到了；为空说明竞争场景没有构造出来。
    """
    rival = psycopg.connect(database_dsn)  # 不开 autocommit：锁一直占着，直到提交
    rival.execute(lock_sql, params)
    blocked: List[bool] = []

    def release_once_blocked():
        try:
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                waiting = pg.execute(
                    "SELECT count(*) AS n FROM pg_stat_activity"
                    " WHERE datname = current_database() AND wait_event_type = 'Lock'"
                    "   AND pid <> pg_backend_pid() AND position(%s in query) > 0",
                    (waiting_fragment,),
                ).fetchone()["n"]
                if waiting >= waiters:
                    blocked.append(True)
                    return
                time.sleep(0.02)
        finally:
            rival.commit()

    watcher = threading.Thread(target=release_once_blocked)
    watcher.start()
    try:
        yield blocked
    finally:
        watcher.join()
        rival.close()
