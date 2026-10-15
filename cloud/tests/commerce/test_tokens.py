"""令牌与会话：刷新轮换、重放检测、过期、只认请求头、退出、重设与修改密码后的吊销、审计日志、过期清理。"""

from __future__ import annotations

from drama_cloud.commerce.accounts import purge_expired

from .conftest import auth

EMAIL = "user@example.com"
PASSWORD = "correct-horse-1"


def me(client, token):
    return client.get("/api/v1/me", headers=auth(token))


def refresh(client, token):
    return client.post("/api/v1/auth/refresh", json={"refresh_token": token})


def login(client, password=PASSWORD):
    response = client.post("/api/v1/auth/login", json={"email": EMAIL, "password": password})
    assert response.status_code == 200, response.text
    return response.json()["tokens"]


def test_refresh_rotates_tokens_and_extends_session(client, register, pg):
    old = register(EMAIL, PASSWORD)["tokens"]
    pg.execute("UPDATE sessions SET expires_at = now() + interval '1 day'")
    response = refresh(client, old["refresh_token"])
    assert response.status_code == 200
    new = response.json()["tokens"]
    assert new["access_token"] != old["access_token"] and new["refresh_token"] != old["refresh_token"]
    assert me(client, new["access_token"]).status_code == 200
    # 旧访问令牌在自然过期前仍可用（最多 15 分钟）：这是短期访问令牌的设计取舍。
    assert me(client, old["access_token"]).status_code == 200
    left = pg.execute("SELECT extract(epoch FROM expires_at - now()) AS s FROM sessions").fetchone()["s"]
    assert left > 29 * 24 * 3600


def test_refresh_reuse_revokes_the_whole_session(client, register, pg):
    old = register(EMAIL, PASSWORD)["tokens"]
    new = refresh(client, old["refresh_token"]).json()["tokens"]
    replay = refresh(client, old["refresh_token"])
    assert replay.status_code == 401 and "重复使用" in replay.json()["message"]
    # 分不清谁是正主，新签发的令牌也一起作废。
    assert me(client, new["access_token"]).status_code == 401
    assert refresh(client, new["refresh_token"]).status_code == 401
    assert pg.execute("SELECT revoke_reason FROM sessions").fetchone()["revoke_reason"] == "refresh_reuse"
    actions = [row["action"] for row in pg.execute("SELECT action FROM audit_log ORDER BY id")]
    assert "session.refresh_reuse" in actions


def test_unknown_refresh_token_rejected(client):
    response = refresh(client, "rt_does-not-exist")
    assert response.status_code == 401 and response.json()["error"] == "unauthorized"


def test_expired_access_token_rejected_but_refresh_works(client, register, pg):
    tokens = register(EMAIL, PASSWORD)["tokens"]
    pg.execute("UPDATE access_tokens SET expires_at = now() - interval '1 second'")
    assert me(client, tokens["access_token"]).status_code == 401
    renewed = refresh(client, tokens["refresh_token"])
    assert renewed.status_code == 200
    assert me(client, renewed.json()["tokens"]["access_token"]).status_code == 200


def test_expired_session_cannot_refresh(client, register, pg):
    tokens = register(EMAIL, PASSWORD)["tokens"]
    pg.execute("UPDATE sessions SET expires_at = now() - interval '1 second'")
    assert refresh(client, tokens["refresh_token"]).status_code == 401
    assert me(client, tokens["access_token"]).status_code == 401


def test_access_token_only_accepted_in_authorization_header(client, register):
    token = register(EMAIL, PASSWORD)["tokens"]["access_token"]
    assert client.get("/api/v1/me", params={"token": token}).status_code == 401
    assert client.get("/api/v1/me", params={"access_token": token}).status_code == 401
    assert client.get("/api/v1/me", headers={"Authorization": token}).status_code == 401
    assert client.get("/api/v1/me", headers={"Authorization": "Bearer"}).status_code == 401
    assert client.get("/api/v1/me", headers={"Authorization": f"bearer {token}"}).status_code == 200


def test_logout_revokes_session(client, register):
    tokens = register(EMAIL, PASSWORD)["tokens"]
    assert client.post("/api/v1/auth/logout", headers=auth(tokens["access_token"])).status_code == 204
    assert me(client, tokens["access_token"]).status_code == 401
    assert refresh(client, tokens["refresh_token"]).status_code == 401
    assert client.post("/api/v1/auth/logout").status_code == 401


def test_password_reset_revokes_every_session(client, register, mailer):
    first = register(EMAIL, PASSWORD)["tokens"]
    second = login(client)
    sent = client.post("/api/v1/auth/codes", json={"email": EMAIL, "purpose": "reset_password"})
    assert sent.status_code == 202
    reset = client.post(
        "/api/v1/auth/password/reset",
        json={"email": EMAIL, "code": mailer.last_code(EMAIL), "new_password": "brand-new-pass-1"},
    )
    assert reset.status_code == 204
    for tokens in (first, second):
        assert me(client, tokens["access_token"]).status_code == 401
        assert refresh(client, tokens["refresh_token"]).status_code == 401
    old = client.post("/api/v1/auth/login", json={"email": EMAIL, "password": PASSWORD})
    assert old.status_code == 401
    assert login(client, "brand-new-pass-1")["access_token"]


def test_password_change_keeps_only_current_session(client, register):
    current = register(EMAIL, PASSWORD)["tokens"]
    other = login(client)
    headers = auth(current["access_token"])
    wrong = client.post(
        "/api/v1/auth/password/change", headers=headers,
        json={"current_password": "not-my-password", "new_password": "brand-new-pass-1"},
    )
    assert wrong.status_code == 400 and wrong.json()["error"] == "wrong_password"
    changed = client.post(
        "/api/v1/auth/password/change", headers=headers,
        json={"current_password": PASSWORD, "new_password": "brand-new-pass-1"},
    )
    assert changed.status_code == 204
    assert me(client, current["access_token"]).status_code == 200
    assert me(client, other["access_token"]).status_code == 401
    assert login(client, "brand-new-pass-1")["access_token"]


def test_audit_log_records_events_without_secrets(client, register, pg):
    tokens = register(EMAIL, PASSWORD)["tokens"]
    login(client)
    client.post("/api/v1/auth/logout", headers=auth(tokens["access_token"]))
    rows = pg.execute("SELECT actor, action, subject, detail::text AS detail FROM audit_log ORDER BY id").fetchall()
    assert [row["action"] for row in rows] == ["user.register", "user.login", "user.logout"]
    dump = repr(rows)
    for secret in (EMAIL, PASSWORD, tokens["access_token"], tokens["refresh_token"]):
        assert secret not in dump


def test_purge_removes_only_expired_rows(client, register, pg):
    tokens = register(EMAIL, PASSWORD)["tokens"]
    pg.execute("UPDATE email_codes SET expires_at = now() - interval '2 days'")
    pg.execute("INSERT INTO rate_limits (bucket, window_start, hits) VALUES ('old', now() - interval '2 days', 1)")
    counts = purge_expired(pg)
    assert counts["email_codes"] == 1 and counts["rate_limits"] == 1
    assert counts["access_tokens"] == 0 and counts["sessions"] == 0
    assert me(client, tokens["access_token"]).status_code == 200
