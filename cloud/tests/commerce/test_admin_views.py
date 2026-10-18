"""管理后台的七个视图（M7-2）。

要点：

* 每个视图都要管理员令牌，用户令牌一律进不来；
* 只读视图**不能**改数据（没有写接口，除了公告/开关/版本策略）；
* 分页有上限，`limit=1000000` 会被夹到 200，不能把库拖死；
* 涉及隐私的字段要脱敏：机器码只给前 12 位、卡密明文一律不返回；
* 公告/开关/版本策略的写接口有校验（不认识的开关拒绝、版本号形状检查）。
"""

from __future__ import annotations

import pytest
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
    assert response.status_code == 200, response.text
    return response.json()["token"]


def admin_auth(token: str) -> dict:
    return {"Authorization": f"Bearer {token}"}


READ_VIEWS = ["/users", "/card-batches", "/orders", "/ledger", "/devices", "/announcements", "/flags", "/version-policy"]


@pytest.mark.parametrize("path", READ_VIEWS)
def test_every_view_needs_an_admin_token(client, path):
    assert client.get(f"/api/v1/admin{path}").status_code == 401


@pytest.mark.parametrize("path", READ_VIEWS)
def test_a_user_token_cannot_read_admin_views(client, register, path):
    body = register("plain@example.com", "correct-horse-1")
    token = body["tokens"]["access_token"]
    assert client.get(f"/api/v1/admin{path}", headers=admin_auth(token)).status_code == 401


@pytest.mark.parametrize("path", READ_VIEWS)
def test_every_view_answers_a_signed_in_admin(client, admin_token, path):
    response = client.get(f"/api/v1/admin{path}", headers=admin_auth(admin_token))
    assert response.status_code == 200, response.text


def test_users_view_shows_balance_and_entitlement(client, admin_token, register, pg):
    from drama_cloud.commerce import credits

    body = register("payer@example.com", "correct-horse-1")
    user_id = body["user"]["id"]
    credits.grant(pg, user_id, 42, idempotency_key="t:1", memo="测试")

    listing = client.get("/api/v1/admin/users", headers=admin_auth(admin_token)).json()
    assert listing["total"] == 1
    row = listing["items"][0]
    assert row["email"] == "payer@example.com"
    assert row["balance"] == 42
    assert row["entitlement"] is None
    assert row["deviceCount"] == 0


def test_the_users_view_filters_by_keyword(client, admin_token, register):
    register("alice@example.com", "correct-horse-1")
    register("bob@other.com", "correct-horse-1")
    listing = client.get(
        "/api/v1/admin/users?keyword=alice", headers=admin_auth(admin_token)
    ).json()
    assert [row["email"] for row in listing["items"]] == ["alice@example.com"]


def test_pagination_is_capped(client, admin_token, register, pg):
    """`limit` 超过上限要在入口就被拒，不能把整个库一次拉出来。

    用 `Query(le=MAX_PAGE)` 而不是「静默夹到 200」：静默夹住会让调用方
    以为自己拿到了 100 万行，实际只有 200 行——那种「看起来成功其实被截断」
    比明确的 422 危险得多。
    """
    for index in range(5):
        register(f"u{index}@example.com", "correct-horse-1")
    too_big = client.get(
        "/api/v1/admin/users?limit=1000000", headers=admin_auth(admin_token)
    )
    assert too_big.status_code == 422, too_big.text
    # 正常范围内的分页可用。
    ok = client.get("/api/v1/admin/users?limit=2", headers=admin_auth(admin_token)).json()
    assert len(ok["items"]) == 2


def test_the_device_view_only_shows_a_prefix(client, admin_token, register, pg):
    """机器码是设备标识，后台只给前 12 位。"""
    body = register("dev@example.com", "correct-horse-1")
    user_id = body["user"]["id"]
    full_id = "a" * 64
    pg.execute(
        "INSERT INTO devices (user_id, device_id, name, platform, app_version)"
        " VALUES (%s::uuid, %s, '办公室电脑', 'windows', '0.2.1')",
        (user_id, full_id),
    )
    listing = client.get("/api/v1/admin/devices", headers=admin_auth(admin_token)).json()
    row = listing["items"][0]
    assert row["deviceIdPrefix"] == full_id[:12]
    assert full_id not in str(listing), "完整机器码不该出现在响应里"


def test_the_card_batch_view_never_returns_a_plain_code(client, admin_token, pg):
    """数据库里只有卡密摘要，后台也不该冒出明文。"""
    batch = pg.execute(
        "INSERT INTO card_batches (name, kind, days, created_by)"
        " VALUES ('测试批次', 'timed', 30, 'cli') RETURNING id"
    ).fetchone()
    pg.execute(
        "INSERT INTO cards (code_digest, batch_id) VALUES (%s, %s)",
        (b"\x00" * 32, batch["id"]),
    )
    listing = client.get("/api/v1/admin/card-batches", headers=admin_auth(admin_token)).json()
    row = listing["items"][0]
    assert row["total"] == 1 and row["redeemed"] == 0 and row["remaining"] == 1
    assert "code_digest" not in str(row)


def test_the_orders_view_rejects_an_unknown_status(client, admin_token):
    response = client.get(
        "/api/v1/admin/orders?status=what", headers=admin_auth(admin_token)
    )
    assert response.status_code == 422, response.text


# ---------- 公告 ----------

def test_an_announcement_can_be_created_updated_and_deleted(client, admin_token):
    created = client.post(
        "/api/v1/admin/announcements",
        headers=admin_auth(admin_token),
        json={"title": "维护通知", "body": "今晚 23 点维护", "level": "warn"},
    )
    assert created.status_code == 201, created.text
    announcement_id = created.json()["id"]

    listing = client.get("/api/v1/admin/announcements", headers=admin_auth(admin_token)).json()
    assert [row["title"] for row in listing["items"]] == ["维护通知"]

    updated = client.patch(
        f"/api/v1/admin/announcements/{announcement_id}",
        headers=admin_auth(admin_token),
        json={"title": "维护通知", "body": "改到明晚", "level": "warn", "active": False},
    )
    assert updated.status_code == 200, updated.text
    listing = client.get("/api/v1/admin/announcements", headers=admin_auth(admin_token)).json()
    assert listing["items"][0]["active"] is False

    assert client.delete(
        f"/api/v1/admin/announcements/{announcement_id}", headers=admin_auth(admin_token)
    ).status_code == 204
    listing = client.get("/api/v1/admin/announcements", headers=admin_auth(admin_token)).json()
    assert listing["items"] == []


def test_updating_a_missing_announcement_is_404(client, admin_token):
    response = client.patch(
        "/api/v1/admin/announcements/00000000-0000-4000-8000-000000000000",
        headers=admin_auth(admin_token),
        json={"title": "x", "body": "y"},
    )
    assert response.status_code == 404


# ---------- 开关 ----------

def test_the_four_flags_exist_by_default(client, admin_token):
    listing = client.get("/api/v1/admin/flags", headers=admin_auth(admin_token)).json()
    keys = [row["key"] for row in listing["items"]]
    assert keys == ["recharge", "server_processing", "signup", "version_task"]
    assert all(row["enabled"] for row in listing["items"]), "默认应当全开"


def test_a_flag_can_be_turned_off_and_on(client, admin_token):
    off = client.put(
        "/api/v1/admin/flags/recharge",
        headers=admin_auth(admin_token),
        json={"enabled": False, "note": "暂时关闭充值"},
    )
    assert off.status_code == 200, off.text
    listing = client.get("/api/v1/admin/flags", headers=admin_auth(admin_token)).json()
    row = next(item for item in listing["items"] if item["key"] == "recharge")
    assert row["enabled"] is False


def test_an_unknown_flag_is_refused(client, admin_token):
    """客户端只认白名单里的开关，后台造一个新的两边会不一致。"""
    response = client.put(
        "/api/v1/admin/flags/made_up_flag",
        headers=admin_auth(admin_token),
        json={"enabled": True, "note": ""},
    )
    assert response.status_code == 404, response.text


# ---------- 版本策略 ----------

def test_the_default_policy_does_not_block_anyone(client, admin_token):
    policy = client.get("/api/v1/admin/version-policy", headers=admin_auth(admin_token)).json()
    assert policy["minSupported"] == "0.0.0", "默认不该强制任何人更新"
    assert policy["downloadUrl"] == ""


def test_the_policy_can_be_updated(client, admin_token):
    response = client.put(
        "/api/v1/admin/version-policy",
        headers=admin_auth(admin_token),
        json={
            "min_supported": "1.1.0",
            "latest": "1.2.0",
            "download_url": "https://example.com/app.exe",
            "download_sha256": "a" * 64,
            "download_sig": "sig",
            "notes": "修复若干问题",
        },
    )
    assert response.status_code == 200, response.text
    policy = client.get("/api/v1/admin/version-policy", headers=admin_auth(admin_token)).json()
    assert policy["minSupported"] == "1.1.0"
    assert policy["latest"] == "1.2.0"


@pytest.mark.parametrize("value", ["abc", "1", "1.x.0", ""])
def test_a_malformed_version_is_refused(client, admin_token, value):
    response = client.put(
        "/api/v1/admin/version-policy",
        headers=admin_auth(admin_token),
        json={"min_supported": value, "latest": "1.0.0"},
    )
    assert response.status_code in (422, 400), response.text


def test_a_malformed_sha256_is_refused(client, admin_token):
    response = client.put(
        "/api/v1/admin/version-policy",
        headers=admin_auth(admin_token),
        json={"min_supported": "1.0.0", "latest": "1.0.0", "download_sha256": "not-a-hash"},
    )
    assert response.status_code == 422, response.text


def test_admin_writes_are_audited(client, admin_token, pg):
    client.put(
        "/api/v1/admin/flags/recharge",
        headers=admin_auth(admin_token),
        json={"enabled": False, "note": ""},
    )
    rows = pg.execute(
        "SELECT action FROM audit_log WHERE action LIKE 'admin.%%' ORDER BY id"
    ).fetchall()
    actions = [row["action"] for row in rows]
    assert "admin.flag.set" in actions


def test_the_admin_page_is_served(client):
    response = client.get("/admin")
    assert response.status_code == 200, response.text
    assert "管理后台" in response.text
