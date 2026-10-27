import pytest
from fastapi.testclient import TestClient

from dd_license.errors import LicenseError

from .conftest import mid


def _card(service, plan="month"):
    return service.create_batch(plan, 1, "", "boss")[1][0]


def test_activate_and_refresh(client, service):
    code = _card(service)
    r = client.post("/v1/activate", json={"card": code, "machine_id": mid(1), "app_version": "0.2.18"})
    assert r.status_code == 200, r.text
    body = r.json()
    assert body["plan"] == "month" and body["token"].count(".") == 1
    assert r.headers["cache-control"] == "no-store"
    r2 = client.post("/v1/refresh", json={"token": body["token"], "machine_id": mid(1)})
    assert r2.status_code == 200 and "already_redeemed" not in r2.json()


def test_error_shape(client):
    r = client.post("/v1/activate", json={"card": "DDK-2222-2222-2222-2222", "machine_id": mid(1)})
    assert r.status_code == 404
    assert r.json() == {"error": "card_invalid", "message": "卡密不存在，请检查后重新输入"}


def test_failed_activation_is_audited(client, service):
    client.post("/v1/activate", json={"card": "DDK-2222-2222-2222-2222", "machine_id": mid(1)})
    assert service.list_audit(action="activate.fail")["total"] == 1


def test_strict_body(client):
    r = client.post("/v1/activate", json={"card": "x", "machine_id": mid(1), "evil": 1})
    assert r.status_code == 422 and r.json()["error"] == "bad_request"
    r = client.post("/v1/activate", json={"card": "x", "machine_id": "short"})
    assert r.status_code == 422


def test_body_size_limit(client):
    r = client.post("/v1/activate", content=b"{" + b" " * 5000 + b"}", headers={"content-type": "application/json"})
    assert r.status_code == 413


def test_rate_limit_per_machine(client):
    for _ in range(10):
        client.post("/v1/activate", json={"card": "DDK-2222-2222-2222-2222", "machine_id": mid(9)})
    r = client.post("/v1/activate", json={"card": "DDK-2222-2222-2222-2222", "machine_id": mid(9)})
    assert r.status_code == 429 and int(r.headers["retry-after"]) > 0


def test_docs_disabled_and_unknown_paths(client):
    for p in ("/docs", "/redoc", "/openapi.json", "/", "/v1", "/etc/passwd"):
        assert client.get(p).status_code == 404, p


def test_security_headers(client):
    r = client.get("/v1/health")
    assert r.status_code == 200 and r.json()["ok"] is True
    h = r.headers
    assert h["x-content-type-options"] == "nosniff"
    assert "max-age" in h["strict-transport-security"]
    assert h["x-frame-options"] == "DENY"
    assert "server" not in h


def test_unexpected_error_hides_details(app, service, monkeypatch):
    def boom(*a, **k):
        raise RuntimeError("secret path /var/lib/x")

    monkeypatch.setattr(service, "refresh", boom)
    c = TestClient(app, client=("127.0.0.1", 1), raise_server_exceptions=False)
    r = c.post("/v1/refresh", json={"token": "a.b", "machine_id": mid(1)})
    assert r.status_code == 500 and "secret" not in r.text


def test_bad_machine_id_rejected_before_rate_limit_key(client, limiter):
    before = limiter.size()
    for i in range(20):
        r = client.post("/v1/refresh", json={"token": "x", "machine_id": f"{i:064d}".replace("0", "g")})
        assert r.status_code == 422
    # 只有按 IP 计数的那一条，没有为 20 个乱编的机器码各建一条记录
    assert limiter.size() - before == 1


def test_rate_limiter_gc_respects_each_window():
    from dd_license.ratelimit import GC_EVERY, RateLimiter

    t = [1000.0]
    rl = RateLimiter(clock=lambda: t[0])
    rl.hit("long", 5, 3600)
    rl.hit("short", 5, 10)
    t[0] += GC_EVERY + 1
    rl.hit("trigger", 5, 10)  # 触发一次清理
    # short 的窗口已过期，被清掉；long 还在自己的 1 小时窗口内，必须保留
    assert rl.size() == 2
    for _ in range(4):
        rl.hit("long", 5, 3600)
    with pytest.raises(LicenseError):
        rl.hit("long", 5, 3600)
