import ipaddress

import pytest
from fastapi.testclient import TestClient

from dd_license.admin_auth import AdminAuth, check_password_strength
from dd_license.api import create_app
from dd_license.errors import LicenseError

from .conftest import ADMIN_PW, ORIGIN, make_settings, mid, write_headers


def test_unauthenticated(client):
    assert client.get("/admin/api/stats").status_code == 401
    assert client.get("/admin/api/me").status_code == 401


def test_admin_page_served_with_csp(client):
    r = client.get("/admin")
    assert r.status_code == 200 and "剧备" in r.text
    csp = r.headers["content-security-policy"]
    assert "script-src 'self'" in csp and "frame-ancestors 'none'" in csp
    assert client.get("/admin/static/admin.js").status_code == 200
    assert client.get("/admin/static/../api.py").status_code == 404
    assert client.get("/admin/static/evil.js").status_code == 404


def test_login_and_cookie_flags(client):
    r = client.post("/admin/api/login", json={"username": "boss", "password": ADMIN_PW}, headers=ORIGIN)
    assert r.status_code == 200
    sc = r.headers["set-cookie"].lower()
    assert "httponly" in sc and "samesite=strict" in sc and "path=/admin" in sc
    assert client.get("/admin/api/me").json()["username"] == "boss"


def test_login_requires_same_origin(client):
    r = client.post("/admin/api/login", json={"username": "boss", "password": ADMIN_PW}, headers={"Origin": "https://evil.example"})
    assert r.status_code == 403
    r = client.post("/admin/api/login", json={"username": "boss", "password": ADMIN_PW})
    assert r.status_code == 403


def test_wrong_password_generic_message_and_lockout(client, service):
    for i in range(5):
        r = client.post("/admin/api/login", json={"username": "boss", "password": "wrong-password-xx"}, headers=ORIGIN)
        assert r.status_code == 401 and r.json()["message"] == "用户名或密码错误"
    r = client.post("/admin/api/login", json={"username": "boss", "password": ADMIN_PW}, headers=ORIGIN)
    assert r.status_code == 429  # 锁定期间正确密码也不行
    nobody = client.post("/admin/api/login", json={"username": "nobody", "password": "x"}, headers=ORIGIN)
    assert nobody.json()["message"] == "用户名或密码错误"
    assert service.list_audit(action="admin.login.fail")["total"] >= 6


def test_write_requires_csrf(admin):
    client, csrf = admin
    body = {"plan": "month", "count": 1, "note": "", "password": ADMIN_PW}
    assert client.post("/admin/api/batches", json=body, headers=ORIGIN).status_code == 403
    assert client.post("/admin/api/batches", json=body, headers={**ORIGIN, "X-CSRF-Token": "bad"}).status_code == 403
    assert client.post("/admin/api/batches", json=body, headers={"X-CSRF-Token": csrf}).status_code == 403
    assert client.post("/admin/api/batches", json=body, headers=write_headers(csrf)).status_code == 200


def test_generate_requires_reauth(admin, db):
    client, csrf = admin
    r = client.post("/admin/api/batches", json={"plan": "year", "count": 3, "note": "x", "password": "nope"}, headers=write_headers(csrf))
    assert r.status_code == 403 and r.json()["error"] == "reauth_failed"
    r = client.post("/admin/api/batches", json={"plan": "year", "count": 3, "note": "x", "password": ADMIN_PW}, headers=write_headers(csrf))
    assert r.status_code == 200
    codes = r.json()["codes"]
    assert len(codes) == len(set(codes)) == 3
    dump = "\n".join(db.read().iterdump())
    for c in codes:
        assert c.replace("DDK-", "").replace("-", "") not in dump


def test_full_admin_flow(admin, client, service, db):
    client, csrf = admin
    h = write_headers(csrf)
    codes = client.post("/admin/api/batches", json={"plan": "month", "count": 2, "note": "", "password": ADMIN_PW}, headers=h).json()["codes"]
    tok = client.post("/v1/activate", json={"card": codes[0], "machine_id": mid(1)}).json()["token"]

    look = client.post("/admin/api/cards/lookup", json={"code": codes[0]}, headers=h).json()
    assert look["status"] == "redeemed" and look["redeemed_mid"] == mid(1)
    assert client.post("/admin/api/cards/revoke", json={"code": codes[1]}, headers=h).status_code == 200
    assert client.post("/admin/api/cards/lookup", json={"code": codes[1]}, headers=h).json()["status"] == "revoked"

    ms = client.get(f"/admin/api/machines?q={mid(1)[:8]}").json()
    assert ms["total"] == 1
    m_id = ms["items"][0]["id"]
    assert client.get(f"/admin/api/machines/{m_id}").json()["cards"][0]["plan"] == "month"

    assert client.post(f"/admin/api/machines/{m_id}/revoke", json={"reason": "退款"}, headers=h).status_code == 200
    assert client.post("/v1/refresh", json={"token": tok, "machine_id": mid(1)}).json()["error"] == "machine_revoked"
    assert client.post(f"/admin/api/machines/{m_id}/unrevoke", headers=h).status_code == 200
    assert client.post("/v1/refresh", json={"token": tok, "machine_id": mid(1)}).status_code == 200

    # 固定绑定：后台没有迁移接口
    assert client.post(f"/admin/api/machines/{m_id}/transfer", json={}, headers=h).status_code == 404

    stats = client.get("/admin/api/stats").json()
    assert stats["plans"]["month"]["total"] == 2
    actions = {i["action"] for i in client.get("/admin/api/audit").json()["items"]}
    assert {"batch.create", "activate", "card.revoke", "machine.revoke", "machine.unrevoke", "admin.login"} <= actions


def test_session_bound_to_ip(app, admin):
    client, _ = admin
    cookie = client.cookies.get("dd_admin")
    other = TestClient(app, client=("127.0.0.2", 1))
    other.cookies.set("dd_admin", cookie)
    assert other.get("/admin/api/me").status_code == 401
    # IP 变化后会话已被删除，原 IP 也不能再用
    assert client.get("/admin/api/me").status_code == 401


def test_password_change_invalidates_sessions(admin):
    client, csrf = admin
    r = client.post("/admin/api/password", json={"old": ADMIN_PW, "new": "Another-Very-Long-Secret-77"}, headers=write_headers(csrf))
    assert r.status_code == 200
    assert client.get("/admin/api/me").status_code == 401


def test_logout(admin):
    client, csrf = admin
    assert client.post("/admin/api/logout", headers=write_headers(csrf)).status_code == 200
    assert client.get("/admin/api/me").status_code == 401


# ------------------------------------------------------------ IP 白名单


def test_ip_not_in_allowlist_gets_404(app):
    c = TestClient(app, client=("8.8.8.8", 1))
    for p in ("/admin", "/admin/api/me", "/admin/static/admin.js"):
        assert c.get(p).status_code == 404
    assert c.post("/admin/api/login", json={"username": "boss", "password": ADMIN_PW}, headers=ORIGIN).status_code == 404
    assert c.get("/v1/health").status_code == 200  # 客户端接口不受影响


def test_forwarded_for_trusted_only_from_loopback(app):
    via_proxy = TestClient(app, client=("127.0.0.1", 1))
    assert via_proxy.get("/admin", headers={"X-Forwarded-For": "8.8.8.8"}).status_code == 404
    assert via_proxy.get("/admin", headers={"X-Forwarded-For": "8.8.8.8, 10.9.9.9"}).status_code == 200
    # 伪造：最左边写白名单 IP，但 Caddy 追加的最右边是真实地址
    assert via_proxy.get("/admin", headers={"X-Forwarded-For": "10.9.9.9, 8.8.8.8"}).status_code == 404
    direct = TestClient(app, client=("8.8.8.8", 1))
    assert direct.get("/admin", headers={"X-Forwarded-For": "10.9.9.9"}).status_code == 404


def test_admin_closed_without_allowlist(tmp_path, service, auth):
    s = make_settings(tmp_path, admin_allow_ips=[])
    c = TestClient(create_app(s, service, auth), client=("127.0.0.1", 1))
    assert c.get("/admin").status_code == 404
    s2 = make_settings(tmp_path, admin_allow_ips=[], admin_allow_any_ip=True)
    c2 = TestClient(create_app(s2, service, auth), client=("8.8.8.8", 1))
    assert c2.get("/admin").status_code == 200
    c2.post("/admin/api/login", json={"username": "boss", "password": ADMIN_PW}, headers=ORIGIN)
    assert c2.get("/admin/api/me").json()["allow_any_ip"] is True


# ------------------------------------------------------------ 密码规则


@pytest.mark.parametrize(
    "pw",
    ["short", "a" * 20, "password12345678", "Boss-is-my-name-12345", "Xqwertyuiop-123456", "12345678abcdefgh"],
)
def test_weak_passwords_rejected(pw):
    with pytest.raises(LicenseError):
        check_password_strength(pw, "boss")


def test_strong_password_ok():
    check_password_strength("Correct-Horse-Battery-Staple-42", "boss")


def test_disabled_admin_cannot_login(client, auth):
    auth.set_disabled("boss", True)
    r = client.post("/admin/api/login", json={"username": "boss", "password": ADMIN_PW}, headers=ORIGIN)
    assert r.status_code == 401
