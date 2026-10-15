"""个人数据：注销（立即匿名化）与导出。

注销：登录、令牌、设备、使用权、验证码全部失效；同一邮箱能重新注册但拿不到旧使用权；兑换过的卡不能再兑换；
审计日志里没有邮箱；密码错、没登录、限流、两个注销请求同时到达。
导出：字段齐全，不含卡密、令牌、哈希，看不到别人的数据。
"""

from __future__ import annotations

import threading

import psycopg
import pytest

from drama_cloud.commerce import cards, entitlements
from drama_cloud.commerce.auth_routes import DELETE_PER_USER, EXPORT_PER_USER

from .conftest import activate, auth, device_id, new_cards, rival_lock

EMAIL = "user@example.com"
PASSWORD = "correct-horse-1"


def delete(client, token: str, password: str = PASSWORD):
    return client.post("/api/v1/me/delete", json={"password": password}, headers=auth(token))


def export(client, token: str):
    return client.get("/api/v1/me/export", headers=auth(token))


def login(client, email: str = EMAIL, password: str = PASSWORD):
    return client.post("/api/v1/auth/login", json={"email": email, "password": password})


@pytest.fixture()
def account(client, register, pg):
    """注册、绑定一台电脑、兑换一张月卡、留一条重设密码的验证码。"""
    body = register(EMAIL, PASSWORD)
    token = body["tokens"]["access_token"]
    assert activate(client, token, device_id("pc-1")).status_code == 200
    [code] = new_cards(pg, days=30)
    redeemed = client.post("/api/v1/cards/redeem", json={"code": code}, headers=auth(token))
    assert redeemed.status_code == 200, redeemed.text
    sent = client.post("/api/v1/auth/codes", json={"email": EMAIL, "purpose": "reset_password"})
    assert sent.status_code == 202, sent.text
    return {"id": body["user"]["id"], "tokens": body["tokens"], "card": code}


def count(pg, table: str, where: str, value) -> int:
    return pg.execute(f"SELECT count(*) AS n FROM {table} WHERE {where}", (value,)).fetchone()["n"]


# ---------- 注销 ----------


def test_delete_anonymizes_and_revokes_everything(client, pg, account):
    user_id, tokens = account["id"], account["tokens"]
    response = delete(client, tokens["access_token"])
    assert response.status_code == 204, response.text

    row = pg.execute("SELECT email, password_hash, deleted_at FROM users WHERE id = %s::uuid", (user_id,)).fetchone()
    assert row["email"] is None and row["password_hash"] is None and row["deleted_at"] is not None
    for table in ("sessions", "devices", "entitlements"):
        assert count(pg, table, "user_id = %s::uuid", user_id) == 0, table
    assert count(pg, "email_codes", "email = %s", EMAIL) == 0

    # 原来的访问令牌、刷新令牌全部失效。
    assert client.get("/api/v1/me", headers=auth(tokens["access_token"])).status_code == 401
    refreshed = client.post("/api/v1/auth/refresh", json={"refresh_token": tokens["refresh_token"]})
    assert refreshed.status_code == 401
    # 登录、重设密码：和邮箱从没注册过一样。
    failed = login(client)
    assert failed.status_code == 401 and failed.json()["error"] == "invalid_credentials"
    reset = client.post("/api/v1/auth/codes", json={"email": EMAIL, "purpose": "reset_password"})
    assert reset.status_code == 404 and reset.json()["error"] == "email_not_registered"


def test_same_email_registers_fresh_account(client, pg, register, account):
    assert delete(client, account["tokens"]["access_token"]).status_code == 204
    body = register(EMAIL, "another-horse-2")
    assert body["user"]["id"] != account["id"]
    # 新账号拿不到旧账号的使用权。
    assert entitlements.current(pg, body["user"]["id"]).view()["kind"] == "none"
    # 兑换过的卡仍然指向旧账号，不能被新账号再兑换一次。
    again = client.post(
        "/api/v1/cards/redeem", json={"code": account["card"]}, headers=auth(body["tokens"]["access_token"])
    )
    assert again.status_code == 409 and again.json()["error"] == "card_used"
    redeemed_by = pg.execute("SELECT redeemed_by FROM cards WHERE code_digest = %s", (cards.digest(account["card"]),))
    assert str(redeemed_by.fetchone()["redeemed_by"]) == account["id"]


def test_audit_log_has_no_email_after_delete(client, pg, account):
    assert delete(client, account["tokens"]["access_token"]).status_code == 204
    rows = pg.execute("SELECT actor, action, subject, detail::text AS detail FROM audit_log ORDER BY id").fetchall()
    assert rows[-1]["action"] == "user.delete"
    assert '"devices": 1' in rows[-1]["detail"] and '"sessions": 1' in rows[-1]["detail"]
    dump = repr(rows)
    for secret in (EMAIL, PASSWORD, account["card"], account["tokens"]["refresh_token"]):
        assert secret not in dump


def test_wrong_password_changes_nothing(client, pg, account):
    response = delete(client, account["tokens"]["access_token"], password="wrong-password")
    assert response.status_code == 400 and response.json()["error"] == "wrong_password"
    assert pg.execute("SELECT deleted_at FROM users").fetchone()["deleted_at"] is None
    assert count(pg, "devices", "user_id = %s::uuid", account["id"]) == 1
    assert client.get("/api/v1/me", headers=auth(account["tokens"]["access_token"])).status_code == 200


def test_delete_requires_login_and_password_field(client, account):
    assert client.post("/api/v1/me/delete", json={"password": PASSWORD}).status_code == 401
    # 必须重新输入密码：只拿访问令牌不够。
    missing = client.post("/api/v1/me/delete", json={}, headers=auth(account["tokens"]["access_token"]))
    assert missing.status_code == 422
    extra = client.post(
        "/api/v1/me/delete",
        json={"password": PASSWORD, "force": True},
        headers=auth(account["tokens"]["access_token"]),
    )
    assert extra.status_code == 422
def test_delete_is_rate_limited(client, account, fresh_window):
    fresh_window(DELETE_PER_USER.window_seconds, needed_seconds=30)
    token = account["tokens"]["access_token"]
    for _ in range(DELETE_PER_USER.limit):
        assert delete(client, token, password="wrong-password").status_code == 400
    blocked = delete(client, token)
    assert blocked.status_code == 429 and blocked.json()["error"] == "rate_limited"


def test_concurrent_deletes_only_one_succeeds(client, pg, database_dsn, account):
    token = account["tokens"]["access_token"]
    results = {}

    def attempt(index: int) -> None:
        results[index] = delete(client, token)

    lock = "SELECT 1 FROM users WHERE id = %s::uuid FOR UPDATE"
    with rival_lock(database_dsn, pg, lock, (account["id"],), "deleted_at IS NULL FOR UPDATE", 2) as blocked:
        threads = [threading.Thread(target=attempt, args=(index,)) for index in range(2)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=30)
    assert blocked, "两个注销请求没有同时卡在用户行上，竞争场景没有构造出来"
    # 先拿到锁的那个注销成功；后一个等它提交后读到 deleted_at，按未登录处理。
    assert sorted(response.status_code for response in results.values()) == [204, 401]
    assert pg.execute("SELECT count(*) AS n FROM audit_log WHERE action = 'user.delete'").fetchone()["n"] == 1


# ---------- 导出 ----------


def test_export_has_every_section_without_secrets(client, pg, account):
    response = export(client, account["tokens"]["access_token"])
    assert response.status_code == 200, response.text
    body = response.json()
    assert set(body) == {"format", "exported_at", "account", "entitlement", "devices", "redemptions",
                         "sessions", "audit_log"}
    assert body["format"] == "drama-dossier/account-export/v1"
    assert set(body["account"]) == {"id", "email", "created_at", "terms_version", "terms_accepted_at"}
    assert body["account"]["email"] == EMAIL
    assert body["entitlement"]["kind"] == "timed"
    [device] = body["devices"]
    assert set(device) == {"code", "name", "platform", "app_version", "bound_at", "last_seen_at",
                           "unbound_at", "unbind_reason"}
    head = device_id("pc-1")[:16].upper()
    # 只给 4 位一组的展示形式，不给机器码摘要本身。
    assert device["code"] == "-".join(head[index:index + 4] for index in range(0, 16, 4))
    assert body["redemptions"] == [{"kind": "timed", "days": 30, "redeemed_at": body["redemptions"][0]["redeemed_at"]}]
    assert len(body["sessions"]) == 1
    assert {row["action"] for row in body["audit_log"]} >= {"user.register", "device.bind", "card.redeem"}

    text = response.text
    password_hash = pg.execute("SELECT password_hash FROM users").fetchone()["password_hash"]
    secrets = [PASSWORD, password_hash, account["card"], cards.display(account["card"]),
               account["tokens"]["access_token"], account["tokens"]["refresh_token"], device_id("pc-1")]
    for secret in secrets:
        assert secret not in text


def test_export_shows_only_own_data(client, register, account):
    other = register("other@example.com", "other-horse-3")
    other_token = other["tokens"]["access_token"]
    assert activate(client, other_token, device_id("pc-2")).status_code == 200
    body = export(client, account["tokens"]["access_token"]).json()
    assert [device["code"][:4] for device in body["devices"]] == [device_id("pc-1")[:4].upper()]
    assert "other@example.com" not in str(body)
    # 审计日志只有本账号发起的操作：别人的注册、绑定不在里面。
    assert {row["subject"] for row in body["audit_log"] if row["action"] == "user.register"} == {account["id"]}
    assert sum(row["action"] == "device.bind" for row in body["audit_log"]) == 1
    mine = export(client, other_token).json()
    assert mine["account"]["email"] == "other@example.com"
    assert mine["redemptions"] == [] and mine["entitlement"]["kind"] == "none"


def test_export_rate_limited_and_gone_after_delete(client, account, fresh_window):
    fresh_window(EXPORT_PER_USER.window_seconds, needed_seconds=30)
    token = account["tokens"]["access_token"]
    assert client.get("/api/v1/me/export").status_code == 401
    for _ in range(EXPORT_PER_USER.limit):
        assert export(client, token).status_code == 200
    blocked = export(client, token)
    assert blocked.status_code == 429 and blocked.json()["error"] == "rate_limited"


def test_export_after_delete_is_unauthorized(client, account):
    token = account["tokens"]["access_token"]
    assert delete(client, token).status_code == 204
    assert export(client, token).status_code == 401


# ---------- 迁移 0003 的约束 ----------


def test_deleted_state_must_be_complete(pg):
    insert = ("INSERT INTO users (email, password_hash, terms_version, terms_accepted_at)"
              " VALUES (%s, 'x', 'v', now()) RETURNING id")
    user = pg.execute(insert, ("a@example.com",)).fetchone()["id"]
    incomplete = [
        "UPDATE users SET email = NULL WHERE id = %s",  # 只清了邮箱
        "UPDATE users SET password_hash = NULL WHERE id = %s",
        "UPDATE users SET deleted_at = now() WHERE id = %s",  # 记了注销时间却没清空
        "UPDATE users SET email = NULL, password_hash = NULL WHERE id = %s",  # 清空了却没记时间
    ]
    for statement in incomplete:
        with pytest.raises(psycopg.errors.CheckViolation):
            pg.execute(statement, (user,))
    done = "UPDATE users SET email = NULL, password_hash = NULL, deleted_at = now() WHERE id = %s"
    pg.execute(done, (user,))
    # 多个注销过的账号（邮箱都是 NULL）互不冲突；同一个邮箱能重新注册。
    second = pg.execute(insert, ("b@example.com",)).fetchone()["id"]
    pg.execute(done, (second,))
    pg.execute(insert, ("a@example.com",))
    assert pg.execute("SELECT count(*) AS n FROM users WHERE deleted_at IS NOT NULL").fetchone()["n"] == 2
