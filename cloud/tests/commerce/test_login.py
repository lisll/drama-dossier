"""登录：成功、失败提示一致、按邮箱限流、旧哈希升级、哈希损坏时拒绝登录；健康检查与数据库故障。"""

from __future__ import annotations

import psycopg
from argon2 import PasswordHasher

from drama_cloud.commerce.api import LOGIN_PER_EMAIL

from .conftest import FAST_HASHER, auth

EMAIL = "user@example.com"
PASSWORD = "correct-horse-1"


def login(client, email=EMAIL, password=PASSWORD):
    return client.post("/api/v1/auth/login", json={"email": email, "password": password})


def test_login_success_and_uniform_failures(client, register):
    user_id = register(EMAIL, PASSWORD)["user"]["id"]
    ok = login(client)
    assert ok.status_code == 200 and ok.json()["user"]["id"] == user_id
    # 密码错误和邮箱不存在给出完全相同的回应。
    for response in (login(client, password="wrong-password"), login(client, email="nobody@example.com")):
        assert response.status_code == 401
        assert response.json() == {"error": "invalid_credentials", "message": "邮箱或密码不正确"}


def test_login_rate_limited_per_email(client, register, fresh_window):
    fresh_window(LOGIN_PER_EMAIL.window_seconds)
    register(EMAIL, PASSWORD)
    for _ in range(LOGIN_PER_EMAIL.limit):
        assert login(client, password="wrong-password").status_code == 401
    blocked = login(client)  # 超过上限后，密码对了也一样被挡
    assert blocked.status_code == 429 and blocked.json()["error"] == "rate_limited"
    assert int(blocked.headers["Retry-After"]) >= 1


def test_login_upgrades_outdated_password_hash(client, register, pg):
    register(EMAIL, PASSWORD)
    outdated = PasswordHasher(time_cost=1, memory_cost=8, parallelism=1, hash_len=16).hash(PASSWORD)
    pg.execute("UPDATE users SET password_hash = %s", (outdated,))
    assert FAST_HASHER.check_needs_rehash(outdated)
    assert login(client).status_code == 200
    upgraded = pg.execute("SELECT password_hash FROM users").fetchone()["password_hash"]
    assert upgraded != outdated and not FAST_HASHER.check_needs_rehash(upgraded)
    assert FAST_HASHER.verify(upgraded, PASSWORD)


def test_corrupted_password_hash_fails_closed(client, register, pg):
    register(EMAIL, PASSWORD)
    pg.execute("UPDATE users SET password_hash = 'not-an-argon2-hash'")
    response = login(client)
    assert response.status_code == 401 and response.json()["error"] == "invalid_credentials"


def test_health(client):
    assert client.get("/api/v1/health").json() == {"ok": True}


def test_database_failure_returns_503(client, register, monkeypatch):
    token = register(EMAIL, PASSWORD)["tokens"]["access_token"]

    def outage(*args, **kwargs):
        raise psycopg.OperationalError("simulated outage")

    monkeypatch.setattr(client.app.state.service, "me", outage)
    response = client.get("/api/v1/me", headers=auth(token))
    assert response.status_code == 503
    assert response.json() == {"error": "service_unavailable", "message": "服务暂时不可用，请稍后重试"}
