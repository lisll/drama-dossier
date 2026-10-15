"""设备绑定与授权凭证：绑定、刷新、换电脑、名额、解绑次数、同时抢名额、伪造凭证。"""

from __future__ import annotations

import json
import threading
from datetime import datetime

import pytest

from drama_cloud.commerce import license_token as lt
from drama_cloud.commerce.accounts import purge_expired

from .conftest import activate, auth, device_id, new_cards, rival_lock

PASSWORD = "correct-horse-1"
DESKTOP = device_id("desktop")
LAPTOP = device_id("laptop")


def refresh(client, token: str, device: str, **extra):
    return client.post("/api/v1/license/refresh", json={"device_id": device, **extra}, headers=auth(token))


def devices(client, token: str, current=None):
    params = {"current_device": current} if current else None
    return client.get("/api/v1/devices", params=params, headers=auth(token))


def unbind(client, token: str, binding_id: str):
    return client.delete(f"/api/v1/devices/{binding_id}", headers=auth(token))


def login(client, email: str = "user@example.com") -> str:
    response = client.post("/api/v1/auth/login", json={"email": email, "password": PASSWORD})
    assert response.status_code == 200, response.text
    return response.json()["tokens"]["access_token"]


def payload_of(token: str) -> dict:
    return json.loads(lt.b64url_decode(token.split(".")[0]))


@pytest.fixture()
def account(register):
    body = register("user@example.com", PASSWORD)
    return body["user"]["id"], body["tokens"]["access_token"]


def test_activate_issues_verifiable_license(client, pg, signer, account):
    user_id, token = account
    response = activate(client, token, DESKTOP)
    assert response.status_code == 200, response.text
    body = response.json()
    issued = lt.verify(body["license"]["token"], [signer.public_key], DESKTOP, payload_of(body["license"]["token"])["iat"])
    assert (issued.user_id, issued.device_id, issued.entitlement, issued.entitlement_until) == (
        user_id, DESKTOP, "none", None)
    assert issued.expires_at - issued.issued_at == 72 * 3600 and issued.kid == signer.kid
    assert body["license"]["kid"] == signer.kid and body["entitlement"]["kind"] == "none"
    refresh_after = datetime.fromisoformat(body["license"]["refresh_after"]).timestamp()
    assert refresh_after == issued.issued_at + 3600
    device = body["device"]
    assert device["code"] == "-".join(DESKTOP[i:i + 4].upper() for i in range(0, 16, 4))
    assert device["current"] is True and device["platform"] == "windows"
    listed = devices(client, token, DESKTOP).json()
    assert listed["device_limit"] == 1 and [item["id"] for item in listed["devices"]] == [device["id"]]
    assert listed["devices"][0]["current"] is True
    assert listed["unbind_quota"] == {"limit": 3, "window_days": 30, "remaining": 3}
    # 凭证里没有邮箱，审计日志里没有凭证。
    assert "user@example.com" not in json.dumps(payload_of(body["license"]["token"]))
    assert body["license"]["token"] not in str(pg.execute("SELECT detail FROM audit_log").fetchall())


def test_reactivating_same_computer_takes_no_new_slot(client, pg, account):
    _, token = account
    first = activate(client, token, DESKTOP).json()["device"]
    again = activate(client, token, DESKTOP, name="改了名字", app_version="0.3.1")
    assert again.status_code == 200
    assert again.json()["device"]["id"] == first["id"]
    assert again.json()["device"]["name"] == "改了名字" and again.json()["device"]["app_version"] == "0.3.1"
    assert pg.execute("SELECT count(*) AS n FROM devices").fetchone()["n"] == 1


def test_refresh_picks_up_new_entitlement(client, pg, account):
    user_id, token = account
    activate(client, token, DESKTOP)
    code = new_cards(pg)[0]
    assert client.post("/api/v1/cards/redeem", json={"code": code}, headers=auth(token)).status_code == 200
    response = refresh(client, token, DESKTOP)
    assert response.status_code == 200, response.text
    payload = payload_of(response.json()["license"]["token"])
    expires = pg.execute(
        "SELECT floor(extract(epoch FROM expires_at))::bigint AS e FROM entitlements"
    ).fetchone()["e"]
    assert payload["ent"] == "timed" and payload["ent_until"] == expires
    assert response.json()["entitlement"]["active"] is True
    assert response.json()["device"]["app_version"] == "0.3.0"  # 刷新时没带版本号，保持原值
    assert refresh(client, token, DESKTOP, app_version="0.4.0").json()["device"]["app_version"] == "0.4.0"


def test_license_carries_expired_and_lifetime_entitlements(client, pg, signer, account):
    _, token = account
    activate(client, token, DESKTOP)
    client.post("/api/v1/cards/redeem", json={"code": new_cards(pg)[0]}, headers=auth(token))
    pg.execute("UPDATE entitlements SET expires_at = now() - interval '1 day'")
    response = refresh(client, token, DESKTOP).json()
    payload = payload_of(response["license"]["token"])
    # 月卡过期了，凭证照样签发（本地免费功能还能用），ent_until 早于签发时间，桌面端据此判定已过期。
    assert payload["ent"] == "timed" and payload["ent_until"] < payload["iat"]
    assert response["entitlement"]["active"] is False
    client.post("/api/v1/cards/redeem", json={"code": new_cards(pg, days=None)[0]}, headers=auth(token))
    token_text = refresh(client, token, DESKTOP).json()["license"]["token"]
    issued = lt.verify(token_text, [signer.public_key], DESKTOP, payload_of(token_text)["iat"])
    assert (issued.entitlement, issued.entitlement_until) == ("lifetime", None)


def test_refresh_requires_bound_computer(client, account):
    _, token = account
    response = refresh(client, token, LAPTOP)
    assert response.status_code == 403 and response.json()["error"] == "device_not_bound"


def test_switching_computers(client, pg, account):
    """旧电脑 A 占着唯一的名额：在新电脑 B 上登录后绑定被拒，踢掉 A 再绑 B 成功，A 立即失效。"""
    _, token_a = account
    binding_a = activate(client, token_a, DESKTOP).json()["device"]["id"]
    token_b = login(client)  # 在新电脑上登录，是另一个会话
    rejected = activate(client, token_b, LAPTOP, name="新电脑")
    assert rejected.status_code == 409
    body = rejected.json()
    assert body["error"] == "device_limit" and body["device_limit"] == 1
    assert [item["id"] for item in body["devices"]] == [binding_a] and body["devices"][0]["current"] is False
    assert body["unbind_quota"]["remaining"] == 3
    assert unbind(client, token_b, binding_a).status_code == 204
    assert activate(client, token_b, LAPTOP, name="新电脑").status_code == 200
    # A 的登录会话随绑定一起吊销，令牌和凭证刷新都不能再用。
    assert client.get("/api/v1/me", headers=auth(token_a)).status_code == 401
    assert refresh(client, token_a, DESKTOP).status_code == 401
    # 在 A 上重新登录也拿不回来：绑定已作废，名额被 B 占着。
    token_a2 = login(client)
    assert refresh(client, token_a2, DESKTOP).json()["error"] == "device_not_bound"
    assert activate(client, token_a2, DESKTOP).json()["error"] == "device_limit"
    assert refresh(client, token_b, LAPTOP).status_code == 200
    rows = pg.execute("SELECT device_id, unbind_reason FROM devices ORDER BY bound_at").fetchall()
    assert [(row["device_id"], row["unbind_reason"]) for row in rows] == [(DESKTOP, "user"), (LAPTOP, None)]
    actions = [row["action"] for row in pg.execute(
        "SELECT action FROM audit_log WHERE action LIKE 'device.%' ORDER BY id")]
    assert actions == ["device.bind", "device.unbind", "device.bind"]


def test_unbind_limit(client, pg, account):
    """30 天内最多解绑 3 次：第 4 次被挡住，并告诉客户端要等多久。"""
    _, token = account
    machines = [device_id(f"pc-{index}") for index in range(5)]
    assert activate(client, token, machines[0]).status_code == 200
    for machine in machines[1:4]:
        token = login(client)
        rejected = activate(client, token, machine).json()
        assert rejected["error"] == "device_limit"
        assert unbind(client, token, rejected["devices"][0]["id"]).status_code == 204
        assert activate(client, token, machine).status_code == 200
    token = login(client)
    rejected = activate(client, token, machines[4]).json()
    assert rejected["error"] == "device_limit" and rejected["unbind_quota"]["remaining"] == 0
    response = unbind(client, token, rejected["devices"][0]["id"])
    assert response.status_code == 429
    assert response.json()["error"] == "unbind_limit" and response.json()["unbind_quota"]["remaining"] == 0
    # 最早那次解绑刚刚发生：要等将近 30 天，次数才会回来一次。
    assert 29 * 86400 < int(response.headers["Retry-After"]) <= 30 * 86400
    assert pg.execute("SELECT count(*) AS n FROM devices WHERE unbound_at IS NULL").fetchone()["n"] == 1


def test_unbind_window_and_wait(client, pg, account):
    """只数 30 天内、用户自己发起的解绑。次数超过上限时（后台调低了上限），
    要等用掉的次数降到上限以下才能再解绑，不一定是等最早那次出周期。"""
    user_id, token = account
    binding = activate(client, token, DESKTOP).json()["device"]["id"]

    def past_unbind(age_days, reason="user"):
        pg.execute(
            "INSERT INTO devices (user_id, device_id, name, platform, app_version, bound_at, last_seen_at,"
            " unbound_at, unbind_reason) VALUES (%s::uuid, %s, 'old', 'linux', '0.1.0',"
            " now() - interval '40 days', now() - interval '40 days', now() - %s * interval '1 day', %s)",
            (user_id, device_id(f"old-{age_days}-{reason}"), age_days, reason),
        )

    past_unbind(31)  # 已经出了周期
    past_unbind(29)
    past_unbind(1, reason="admin")  # 后台强制解绑不算用户的次数
    assert devices(client, token).json()["unbind_quota"]["remaining"] == 2
    for age in (20, 10, 5):  # 周期内变成 29、20、10、5 天前共 4 次，超过上限 3 次
        past_unbind(age)
    response = unbind(client, token, binding)
    assert response.status_code == 429
    # 29 天前那次 1 天后出周期，周期内还剩 3 次，仍然用完；20 天前那次 10 天后出周期，才回来一次。
    assert 10 * 86400 - 60 < int(response.headers["Retry-After"]) <= 10 * 86400


def test_kicking_this_computer_logs_it_out(client, account):
    _, token = account
    binding = activate(client, token, DESKTOP).json()["device"]["id"]
    assert unbind(client, token, binding).status_code == 204
    assert client.get("/api/v1/me", headers=auth(token)).status_code == 401
    token = login(client)
    response = unbind(client, token, binding)
    assert response.status_code == 404 and response.json()["error"] == "device_not_found"
    assert devices(client, token).json()["unbind_quota"]["remaining"] == 2
    assert activate(client, token, DESKTOP).status_code == 200  # 名额空出来了，可以重新绑定


def test_devices_are_private_to_their_account(client, register, account):
    _, token = account
    binding = activate(client, token, DESKTOP).json()["device"]["id"]
    other = register("other@example.com", PASSWORD)["tokens"]["access_token"]
    assert devices(client, other).json()["devices"] == []
    response = unbind(client, other, binding)
    assert response.status_code == 404 and response.json()["error"] == "device_not_found"
    assert refresh(client, token, DESKTOP).status_code == 200
    # 同一台电脑可以分别绑到两个账号上，各占各的名额。
    assert activate(client, other, DESKTOP).status_code == 200


def test_two_computers_race_for_the_last_slot(client, pg, account, database_dsn):
    user_id, token = account
    second = login(client)
    results = {}

    def attempt(name, access_token, machine):
        results[name] = activate(client, access_token, machine)

    lock = "SELECT 1 FROM users WHERE id = %s::uuid FOR NO KEY UPDATE"
    with rival_lock(database_dsn, pg, lock, (user_id,), "FOR NO KEY UPDATE", 2) as blocked:
        threads = [
            threading.Thread(target=attempt, args=("desktop", token, DESKTOP)),
            threading.Thread(target=attempt, args=("laptop", second, LAPTOP)),
        ]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=30)
    assert blocked, "两个绑定请求没有同时卡在用户锁上，竞争场景没有构造出来"
    statuses = sorted(response.status_code for response in results.values())
    assert statuses == [200, 409], [response.text for response in results.values()]
    loser = next(response for response in results.values() if response.status_code == 409)
    assert loser.json()["error"] == "device_limit"
    assert pg.execute("SELECT count(*) AS n FROM devices WHERE unbound_at IS NULL").fetchone()["n"] == 1


def test_forged_license_is_rejected(client, signer, account):
    """改了凭证内容（升成永久、延长有效期、换机器码）或自己签一张，验签都过不去。"""
    _, token = account
    issued = activate(client, token, DESKTOP).json()["license"]["token"]
    signature_part = issued.split(".")[1]
    payload = payload_of(issued)
    now = payload["iat"]
    trusted = [signer.public_key]
    assert lt.verify(issued, trusted, DESKTOP, now).entitlement == "none"
    for forged in (dict(payload, ent="lifetime"), dict(payload, exp=payload["exp"] + 365 * 86400),
                   dict(payload, dev=LAPTOP)):
        tampered = lt.b64url_encode(lt.canonical_payload(forged)) + "." + signature_part
        with pytest.raises(lt.LicenseError) as caught:
            lt.verify(tampered, trusted, forged["dev"], now)
        assert caught.value.code == "bad_signature"
    attacker = lt.Signer.from_seed(bytes(32))
    with pytest.raises(lt.LicenseError) as caught:
        lt.verify(attacker.sign(dict(payload, ent="lifetime", kid=attacker.kid)), trusted, DESKTOP, now)
    assert caught.value.code == "bad_signature"
    with pytest.raises(lt.LicenseError) as caught:
        lt.verify(issued, trusted, LAPTOP, now)  # 原样拷到另一台电脑上
    assert caught.value.code == "wrong_device"


def test_password_reset_keeps_binding(client, mailer, account):
    _, token = account
    binding = activate(client, token, DESKTOP).json()["device"]["id"]
    email = "user@example.com"
    assert client.post("/api/v1/auth/codes", json={"email": email, "purpose": "reset_password"}).status_code == 202
    reset = {"email": email, "code": mailer.last_code(email), "new_password": "new-horse-2"}
    assert client.post("/api/v1/auth/password/reset", json=reset).status_code == 204
    assert refresh(client, token, DESKTOP).status_code == 401  # 旧登录全部失效
    response = client.post("/api/v1/auth/login", json={"email": email, "password": "new-horse-2"})
    new_token = response.json()["tokens"]["access_token"]
    assert refresh(client, new_token, DESKTOP).status_code == 200  # 绑定还在，不占新名额
    assert [item["id"] for item in devices(client, new_token).json()["devices"]] == [binding]


def test_logout_keeps_the_slot(client, account):
    """退出登录不归还名额：名额跟着电脑走，要换电脑得先解绑。"""
    _, token = account
    binding = activate(client, token, DESKTOP).json()["device"]["id"]
    assert client.post("/api/v1/auth/logout", headers=auth(token)).status_code == 204
    token = login(client)
    assert activate(client, token, LAPTOP).json()["error"] == "device_limit"
    assert [item["id"] for item in devices(client, token).json()["devices"]] == [binding]
    assert refresh(client, token, DESKTOP).status_code == 200  # 原来那台重新登录后照常刷新


def test_cleanup_keeps_bindings_and_unbind_history(client, pg, account):
    """每日清理会删掉过期会话：绑定和解绑历史都要留着，名额和解绑次数照样数得准。"""
    _, token = account
    kicked = activate(client, token, DESKTOP).json()["device"]["id"]
    token = login(client)
    assert unbind(client, token, kicked).status_code == 204
    kept = activate(client, token, LAPTOP).json()["device"]["id"]
    pg.execute("UPDATE sessions SET expires_at = now() - interval '31 days'")
    assert purge_expired(pg)["sessions"] == 2
    rows = pg.execute("SELECT id, session_id, unbind_reason FROM devices ORDER BY bound_at").fetchall()
    assert [(str(row["id"]), row["session_id"], row["unbind_reason"]) for row in rows] == [
        (kicked, None, "user"), (kept, None, None)]
    token = login(client)
    listed = devices(client, token).json()
    assert [item["id"] for item in listed["devices"]] == [kept] and listed["unbind_quota"]["remaining"] == 2
    assert activate(client, token, DESKTOP).json()["error"] == "device_limit"  # 被清理的会话不归还名额
    # 解绑一台会话已被清理的电脑：没有会话可吊销，照样解绑，也不影响发起请求的这个登录。
    assert unbind(client, token, kept).status_code == 204
    assert client.get("/api/v1/me", headers=auth(token)).status_code == 200
    assert devices(client, token).json()["unbind_quota"]["remaining"] == 1
    assert activate(client, token, DESKTOP).status_code == 200


@pytest.mark.parametrize("path, body, field", [
    ("/api/v1/license/activate",
     {"device_id": DESKTOP.upper(), "name": "x", "platform": "windows", "app_version": "1.0"}, "device_id"),
    ("/api/v1/license/activate",
     {"device_id": DESKTOP, "name": "x", "platform": "android", "app_version": "1.0"}, "platform"),
    ("/api/v1/license/activate", {"device_id": DESKTOP, "name": "x", "platform": "windows"}, "app_version"),
    ("/api/v1/license/refresh", {"device_id": DESKTOP, "extra": 1}, "extra"),
])
def test_bad_requests_are_422(client, account, path, body, field):
    response = client.post(path, json=body, headers=auth(account[1]))
    assert response.status_code == 422
    assert [item["field"] for item in response.json()["fields"]] == [field]


def test_bad_query_and_path_are_422(client, account):
    headers = auth(account[1])
    response = client.get("/api/v1/devices", params={"current_device": "XYZ"}, headers=headers)
    assert response.status_code == 422 and response.json()["fields"][0]["field"] == "query.current_device"
    response = client.delete("/api/v1/devices/not-a-uuid", headers=headers)
    assert response.status_code == 422 and response.json()["fields"][0]["field"] == "path.binding_id"


@pytest.mark.parametrize("method, path", [
    ("POST", "/api/v1/license/activate"),
    ("POST", "/api/v1/license/refresh"),
    ("GET", "/api/v1/devices"),
    ("DELETE", "/api/v1/devices/00000000-0000-4000-8000-000000000000"),
])
def test_license_endpoints_require_login(client, method, path):
    response = client.request(method, path, json={} if method == "POST" else None)
    assert response.status_code == 401 and response.json()["error"] == "unauthorized"
