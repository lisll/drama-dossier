"""桌面端的公开配置接口（M7-3）。

这个接口**不需要登录**，所以最要紧的是：里面绝不能有用户数据。
另外它决定了「要不要强制更新」——拉不到就把人锁在门外是最糟的降级，
所以默认值必须是「不拦人」。
"""

from __future__ import annotations

import pytest
from datetime import datetime, timedelta, timezone

from fastapi.testclient import TestClient

from drama_cloud.commerce.admins import AdminService
from drama_cloud.commerce.api import create_app

from .conftest import FAST_HASHER, auth

ADMIN_PASSWORD = "admin-password-1234"


@pytest.fixture()
def client(commerce_settings, pool, pg, mailer):
    app = create_app(commerce_settings, pool=pool, mailer=mailer, hasher=FAST_HASHER)
    with TestClient(app) as test_client:
        yield test_client


@pytest.fixture()
def admin_token(client, pg):
    AdminService(hasher=FAST_HASHER).create(pg, "boss@example.com", ADMIN_PASSWORD)
    response = client.post(
        "/api/v1/admin/login",
        json={"email": "boss@example.com", "password": ADMIN_PASSWORD},
    )
    return response.json()["token"]


def admin_auth(token: str) -> dict:
    return {"Authorization": f"Bearer {token}"}


def test_the_config_needs_no_login(client):
    """启动时就要知道要不要强制更新，那时可能还没登录。"""
    response = client.get("/api/v1/client/config")
    assert response.status_code == 200, response.text


def test_the_defaults_do_not_block_anyone(client):
    config = client.get("/api/v1/client/config").json()
    assert config["version"]["minSupported"] == "0.0.0", "默认不该拦住任何人"
    assert config["version"]["downloadUrl"] == ""
    assert all(config["flags"].values()), "四个开关默认全开"
    assert config["announcements"] == []


def test_the_config_never_contains_user_data(client, register, pg):
    """公开接口里绝不能出现用户数据——这里逐字段确认。"""
    body = register("someone@example.com", "correct-horse-1")
    config = client.get("/api/v1/client/config")
    text = config.text
    assert "someone@example.com" not in text
    assert body["user"]["id"] not in text
    assert "token" not in text.lower()
    # 只应当有三个顶层的键。
    assert set(config.json()) == {"announcements", "flags", "version"}


def test_only_active_announcements_are_returned(client, admin_token, pg):
    now = pg.execute("SELECT now()").fetchone()["now"]
    # 一条生效中。
    client.post(
        "/api/v1/admin/announcements",
        headers=admin_auth(admin_token),
        json={"title": "生效中", "body": "正文"},
    )
    # 一条被停用。
    created = client.post(
        "/api/v1/admin/announcements",
        headers=admin_auth(admin_token),
        json={"title": "已停用", "body": "正文", "active": False},
    )
    # 一条已过期（直接改库里的时间窗口）。
    expired = client.post(
        "/api/v1/admin/announcements",
        headers=admin_auth(admin_token),
        json={"title": "已过期", "body": "正文"},
    ).json()["id"]
    pg.execute(
        "UPDATE announcements SET starts_at = now() - interval '2 days',"
        " ends_at = now() - interval '1 day' WHERE id = %s::uuid",
        (expired,),
    )

    config = client.get("/api/v1/client/config").json()
    titles = [item["title"] for item in config["announcements"]]
    assert titles == ["生效中"], titles


def test_announcements_are_sorted_by_sort_order(client, admin_token):
    for title, order in (("后", 200), ("前", 10)):
        client.post(
            "/api/v1/admin/announcements",
            headers=admin_auth(admin_token),
            json={"title": title, "body": "正文", "sort_order": order},
        )
    config = client.get("/api/v1/client/config").json()
    assert [item["title"] for item in config["announcements"]] == ["前", "后"]


def test_a_future_announcement_waits_for_its_window(client, admin_token):
    """给公告定一个未来的生效时间：现在不该显示，到点才显示。

    表里有 starts_at/ends_at、客户端配置也按窗口过滤，接口必须能写进去，
    否则那条能力等于用不上。
    """
    future = (datetime.now(timezone.utc) + timedelta(days=1)).isoformat()
    created = client.post(
        "/api/v1/admin/announcements",
        headers=admin_auth(admin_token),
        json={"title": "明天开始", "body": "正文", "starts_at": future},
    )
    assert created.status_code == 201, created.text
    config = client.get("/api/v1/client/config").json()
    assert config["announcements"] == [], "还没到生效时间，不该显示"


def test_an_expired_window_hides_the_announcement(client, admin_token, pg):
    past_start = (datetime.now(timezone.utc) - timedelta(days=2)).isoformat()
    past_end = (datetime.now(timezone.utc) - timedelta(days=1)).isoformat()
    created = client.post(
        "/api/v1/admin/announcements",
        headers=admin_auth(admin_token),
        json={"title": "已结束", "body": "正文", "starts_at": past_start, "ends_at": past_end},
    )
    assert created.status_code == 201, created.text
    config = client.get("/api/v1/client/config").json()
    assert config["announcements"] == [], "过了结束时间，不该显示"


def test_a_window_without_a_timezone_is_refused(client, admin_token):
    """不带时区的时间会在跨时区部署时悄悄偏几小时，所以直接拒绝。"""
    response = client.post(
        "/api/v1/admin/announcements",
        headers=admin_auth(admin_token),
        json={"title": "x", "body": "y", "starts_at": "2026-10-18T00:00:00"},
    )
    assert response.status_code == 422, response.text
    assert "时区" in response.text


def test_a_window_that_ends_before_it_starts_is_refused(client, admin_token):
    start = (datetime.now(timezone.utc) + timedelta(days=2)).isoformat()
    end = (datetime.now(timezone.utc) + timedelta(days=1)).isoformat()
    response = client.post(
        "/api/v1/admin/announcements",
        headers=admin_auth(admin_token),
        json={"title": "x", "body": "y", "starts_at": start, "ends_at": end},
    )
    assert response.status_code == 422, response.text
    assert "早于" in response.text


def test_turning_a_flag_off_shows_up_in_the_config(client, admin_token):
    client.put(
        "/api/v1/admin/flags/server_processing",
        headers=admin_auth(admin_token),
        json={"enabled": False, "note": ""},
    )
    config = client.get("/api/v1/client/config").json()
    assert config["flags"]["server_processing"] is False
    # 其它开关不受影响。
    assert config["flags"]["recharge"] is True


def test_the_version_policy_is_reflected(client, admin_token):
    client.put(
        "/api/v1/admin/version-policy",
        headers=admin_auth(admin_token),
        json={
            "min_supported": "1.1.0",
            "latest": "1.2.0",
            "download_url": "https://example.com/a.exe",
            "download_sha256": "b" * 64,
            "download_sig": "sig-value",
            "notes": "修了些问题",
        },
    )
    config = client.get("/api/v1/client/config").json()["version"]
    assert config["minSupported"] == "1.1.0"
    assert config["latest"] == "1.2.0"
    assert config["downloadSha256"] == "b" * 64
    assert config["downloadSig"] == "sig-value"


def test_the_config_is_rate_limited(client):
    """公开接口也要限流，否则能被刷。"""
    from drama_cloud.commerce import ratelimit

    codes = [client.get("/api/v1/client/config").status_code for _ in range(650)]
    assert 429 in codes, "超过每小时上限应当被限流"
