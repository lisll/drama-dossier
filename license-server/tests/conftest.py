from __future__ import annotations

import hashlib
import ipaddress
from pathlib import Path

import pytest
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from fastapi.testclient import TestClient

from dd_license.admin_auth import AdminAuth
from dd_license.api import create_app
from dd_license.config import Settings
from dd_license.db import Database
from dd_license.ratelimit import RateLimiter
from dd_license.service import LicenseService

# 公开的测试种子：只用于测试与测试向量，绝不能出现在客户端 PRODUCTION_KEYS 里。
TEST_SEED = bytes(range(32))
OTHER_SEED = bytes(range(32, 64))
ADMIN_PW = "Correct-Horse-Battery-Staple-42"


def mid(n: int) -> str:
    return hashlib.sha256(f"machine-{n}".encode()).hexdigest()


@pytest.fixture
def key():
    return Ed25519PrivateKey.from_private_bytes(TEST_SEED)


@pytest.fixture
def db(tmp_path: Path):
    d = Database(tmp_path / "t.sqlite3")
    d.migrate()
    return d


@pytest.fixture
def service(db, key):
    return LicenseService(db, key, token_days=7)


@pytest.fixture
def auth(db):
    a = AdminAuth(db)
    a.create_admin("boss", ADMIN_PW)
    return a


def make_settings(tmp_path: Path, **kw) -> Settings:
    base = dict(
        db_path=tmp_path / "t.sqlite3",
        key_file=tmp_path / "key.pem",
        admin_allow_ips=[ipaddress.ip_network("127.0.0.0/8"), ipaddress.ip_network("10.9.9.9/32")],
        trust_proxy=True,
        cookie_secure=False,  # TestClient 走 http://testserver
    )
    base.update(kw)
    return Settings(**base)


@pytest.fixture
def settings(tmp_path):
    return make_settings(tmp_path)


@pytest.fixture
def limiter():
    return RateLimiter()


@pytest.fixture
def app(settings, service, auth, limiter):
    return create_app(settings, service, auth, limiter)


@pytest.fixture
def client(app):
    # TestClient 的对端地址是 "testclient"，不是 IP；用 client=(...) 模拟本机 Caddy
    return TestClient(app, client=("127.0.0.1", 50000))


ORIGIN = {"Origin": "http://testserver"}


@pytest.fixture
def admin(client):
    """已登录的管理员客户端，返回 (client, csrf)。"""
    r = client.post("/admin/api/login", json={"username": "boss", "password": ADMIN_PW}, headers=ORIGIN)
    assert r.status_code == 200, r.text
    return client, r.json()["csrf"]


def write_headers(csrf: str) -> dict:
    return {**ORIGIN, "X-CSRF-Token": csrf}
