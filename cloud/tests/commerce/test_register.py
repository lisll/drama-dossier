"""注册：正常流程、只存摘要、重复注册（含并发竞争）、参数校验。"""

from __future__ import annotations

import threading
import time

import psycopg
import pytest

from drama_cloud.commerce import security
from drama_cloud.commerce.config import CommerceSettings

from .conftest import auth

EMAIL = "user@example.com"
PASSWORD = "correct-horse-1"


def send_code(client, email=EMAIL):
    return client.post("/api/v1/auth/codes", json={"email": email, "purpose": "register"})


def register_with(client, code, email=EMAIL):
    return client.post(
        "/api/v1/auth/register",
        json={"email": email, "code": code, "password": PASSWORD, "accept_terms": True},
    )


def test_register_normalizes_email_and_returns_working_tokens(client, mailer, pg):
    assert send_code(client, "  User@Example.COM ").status_code == 202
    response = register_with(client, mailer.last_code(EMAIL), "USER@example.com")
    assert response.status_code == 201, response.text
    body = response.json()
    assert body["user"]["email"] == EMAIL
    tokens = body["tokens"]
    assert tokens["token_type"] == "Bearer" and tokens["expires_in"] == 900
    assert tokens["access_token"].startswith("at_") and tokens["refresh_token"].startswith("rt_")
    me = client.get("/api/v1/me", headers=auth(tokens["access_token"]))
    assert me.status_code == 200 and me.json()["user"]["id"] == body["user"]["id"]
    row = pg.execute("SELECT password_hash, terms_version FROM users").fetchone()
    assert row["password_hash"].startswith("$argon2id$") and PASSWORD not in row["password_hash"]
    assert row["terms_version"] == CommerceSettings.terms_version


def test_only_token_digests_are_stored(register, pg):
    tokens = register(EMAIL, PASSWORD)["tokens"]
    access = pg.execute("SELECT token_digest FROM access_tokens").fetchone()["token_digest"]
    refresh = pg.execute("SELECT token_digest FROM refresh_tokens").fetchone()["token_digest"]
    assert bytes(access) == security.token_digest(tokens["access_token"])
    assert bytes(refresh) == security.token_digest(tokens["refresh_token"])


def test_register_conflicts_when_email_taken_meanwhile(client, mailer, pg):
    assert send_code(client).status_code == 202
    pg.execute(
        "INSERT INTO users (email, password_hash, terms_version, terms_accepted_at)"
        " VALUES (%s, 'x', 'v', now())",
        (EMAIL,),
    )
    response = register_with(client, mailer.last_code(EMAIL))
    assert response.status_code == 409 and response.json()["error"] == "email_taken"


def test_concurrent_registration_hits_unique_backstop(client, mailer, pg, database_dsn):
    """预检查时对方还没提交，插入时撞上唯一约束：要返回 409，验证码也不能被白白用掉。"""
    assert send_code(client).status_code == 202
    code = mailer.last_code(EMAIL)
    rival = psycopg.connect(database_dsn)  # 不开 autocommit：插入后先不提交，占住这个邮箱
    rival.execute(
        "INSERT INTO users (email, password_hash, terms_version, terms_accepted_at)"
        " VALUES (%s, 'x', 'v', now())",
        (EMAIL,),
    )
    blocked = []

    def commit_once_register_is_blocked():
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and not blocked:
            waiting = pg.execute(
                "SELECT count(*) AS n FROM pg_stat_activity"
                " WHERE datname = current_database() AND wait_event_type = 'Lock'"
            ).fetchone()["n"]
            if waiting:
                blocked.append(True)
            else:
                time.sleep(0.02)
        rival.commit()

    watcher = threading.Thread(target=commit_once_register_is_blocked)
    watcher.start()
    try:
        response = register_with(client, code)
    finally:
        watcher.join()
        rival.close()
    assert blocked, "注册请求没有在唯一约束上等待，竞争场景没有构造出来"
    assert response.status_code == 409 and response.json()["error"] == "email_taken"
    assert pg.execute("SELECT consumed_at FROM email_codes").fetchone()["consumed_at"] is None
    assert pg.execute("SELECT count(*) AS n FROM users").fetchone()["n"] == 1


@pytest.mark.parametrize(
    "override, field",
    [
        ({"accept_terms": False}, "accept_terms"),
        ({"accept_terms": "true"}, "accept_terms"),
        ({"password": "short"}, "password"),
        ({"email": "not-an-email"}, "email"),
        ({"invite_code": "x"}, "invite_code"),
    ],
)
def test_validation_errors_do_not_echo_input(client, override, field):
    payload = {"email": EMAIL, "code": "123456", "password": "secret-pass-9", "accept_terms": True, **override}
    response = client.post("/api/v1/auth/register", json=payload)
    assert response.status_code == 422
    body = response.json()
    assert body["error"] == "invalid_request"
    assert any(item["field"] == field for item in body["fields"])
    assert payload["password"] not in response.text


def test_chinese_messages_for_terms_and_malformed_json(client):
    payload = {"email": EMAIL, "code": "123456", "password": "secret-pass-9", "accept_terms": False}
    response = client.post("/api/v1/auth/register", json=payload)
    assert response.json()["fields"] == [{"field": "accept_terms", "message": "请先阅读并同意用户协议和隐私政策"}]
    broken = client.post(
        "/api/v1/auth/login", content=b"{not json", headers={"Content-Type": "application/json"}
    )
    assert broken.status_code == 422
    assert broken.json()["fields"] == [{"field": "body", "message": "请求体不是合法的 JSON"}]
