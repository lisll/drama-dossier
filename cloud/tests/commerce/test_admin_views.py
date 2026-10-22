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

from .conftest import FAST_HASHER, activate, auth, new_cards

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


READ_VIEWS = ["/users", "/card-batches", "/orders", "/ledger", "/devices", "/announcements", "/flags", "/version-policy", "/prices", "/recharge-tiers", "/invite-settings"]


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


# ---------- 价目与充值档位（可写） ----------

def test_prices_can_be_read_and_changed(client, admin_token):
    """总方案要求「价格做成后台可配置」。之前只能改种子 SQL 再重新部署。"""
    listing = client.get("/api/v1/admin/prices", headers=admin_auth(admin_token)).json()
    subjects = {row["subject"] for row in listing["items"]}
    assert {"version", "package", "server_version"} <= subjects

    response = client.put(
        "/api/v1/admin/prices/version",
        headers=admin_auth(admin_token),
        json={"credits_per_unit": 8, "min_units": 1},
    )
    assert response.status_code == 200, response.text
    after = client.get("/api/v1/admin/prices", headers=admin_auth(admin_token)).json()
    row = next(item for item in after["items"] if item["subject"] == "version")
    assert row["creditsPerUnit"] == 8


def test_a_price_change_does_not_affect_already_issued_recipes(client, admin_token, register, pg):
    """签发配方 → 走**后台接口**改价 → 消费，扣的是签发时定死的价。

    与 `test_recipe_billing.test_price_is_fixed_at_issue_time…` 的分工：那边直接改库
    （`price_rules`），这条走的是管理后台的改价接口——后台改价与直接改库必须同样守承诺。
    """
    from drama_cloud.commerce import credits
    from .test_recipes import PASSWORD, PC, TASK

    body = register("priced@example.com", PASSWORD)
    token = body["tokens"]["access_token"]
    assert activate(client, token, PC).status_code == 200
    [code] = new_cards(pg, days=30)
    assert client.post("/api/v1/cards/redeem", json={"code": code}, headers=auth(token)).status_code == 200
    credits.grant(pg, body["user"]["id"], 100, idempotency_key="seed:price")

    issued = client.post(
        "/api/v1/version-recipes",
        headers=auth(token),
        json={"device_id": PC, "task": TASK, "platform": "universal",
              "strength": "medium", "episode_count": 3},
    )
    assert issued.status_code == 200, issued.text
    assert issued.json()["credits"] == 15, "签发时每集 5 积分，3 集定死 15"

    # 后台把单价改成 50：已签发的配方不重算。
    client.put(
        "/api/v1/admin/prices/version",
        headers=admin_auth(admin_token),
        json={"credits_per_unit": 50, "min_units": 1},
    )

    consumed = client.post(
        f"/api/v1/version-recipes/{issued.json()['recipe_id']}/consume",
        headers=auth(token),
        json={"device_id": PC},
    )
    assert consumed.status_code == 200, consumed.text
    assert consumed.json()["credits"] == 15, "消费仍按签发时定死的 15 扣"
    assert credits.balance(pg, body["user"]["id"]) == 85, "100 - 15，改价没让已买的变价"


def test_a_tier_change_does_not_affect_already_placed_orders(client, admin_token, register, pg):
    """下单 → 后台改档位 → 模拟支付回调入账，到账的是**下单时**的积分数。

    订单在创建时把积分数写进了 `payment_orders.credits`，回调入账读的是它，
    不再回头看档位表——改档位不能让用户已下的单变多或变少。
    """
    from drama_cloud.commerce import credits
    from drama_cloud.commerce.payments import mock as mock_provider
    from .conftest import MOCK_PAY_SECRET

    body = register("tiered@example.com", "correct-horse-1")
    token = body["tokens"]["access_token"]
    order = client.post(
        "/api/v1/payments/orders",
        headers=auth(token),
        json={"tier_id": "tier-100", "channel": "mock"},
    ).json()
    assert order["credits"] == 100, "下单时 tier-100 是 100 积分"

    # 后台把这档改成 999 积分、金额不动。
    changed = client.put(
        "/api/v1/admin/recharge-tiers/tier-100",
        headers=admin_auth(admin_token),
        json={"amount_fen": 1000, "credits": 999, "sort_order": 10, "active": True},
    )
    assert changed.status_code == 200, changed.text

    # 按订单金额回调（金额必须一致，验签用真报文）。
    provider = mock_provider.MockProvider(MOCK_PAY_SECRET, None)
    payload = {
        "out_trade_no": order["outTradeNo"], "channel_trade_no": "mock-tier-1",
        "amount_fen": 1000, "status": "paid", "paid_at": "2026-10-17T12:00:00+00:00",
    }
    signed, signature = provider.sign(payload)
    notify = client.post(
        "/api/v1/payments/mock/notify",
        content=signed,
        headers={"content-type": "application/json", mock_provider.SIGNATURE_HEADER: signature},
    )
    assert notify.status_code == 200, notify.text
    assert credits.balance(pg, body["user"]["id"]) == 100, "到账的是下单时的 100，不是改后的 999"


def test_an_unknown_subject_is_refused(client, admin_token):
    response = client.put(
        "/api/v1/admin/prices/not-a-subject",
        headers=admin_auth(admin_token),
        json={"credits_per_unit": 1, "min_units": 1},
    )
    assert response.status_code == 404, response.text


@pytest.mark.parametrize("value", [0, -1])
def test_a_non_positive_price_is_refused(client, admin_token, value):
    response = client.put(
        "/api/v1/admin/prices/version",
        headers=admin_auth(admin_token),
        json={"credits_per_unit": value, "min_units": 1},
    )
    assert response.status_code == 422, response.text


def test_tiers_can_be_listed_including_inactive(client, admin_token):
    listing = client.get(
        "/api/v1/admin/recharge-tiers", headers=admin_auth(admin_token)
    ).json()
    ids = [row["id"] for row in listing["items"]]
    assert "tier-100" in ids


def test_a_tier_can_be_added_and_updated(client, admin_token):
    created = client.put(
        "/api/v1/admin/recharge-tiers/tier-999",
        headers=admin_auth(admin_token),
        json={"amount_fen": 9900, "credits": 1000, "sort_order": 40, "active": True},
    )
    assert created.status_code == 200, created.text

    updated = client.put(
        "/api/v1/admin/recharge-tiers/tier-999",
        headers=admin_auth(admin_token),
        json={"amount_fen": 9900, "credits": 1200, "sort_order": 40, "active": False},
    )
    assert updated.status_code == 200, updated.text

    listing = client.get(
        "/api/v1/admin/recharge-tiers", headers=admin_auth(admin_token)
    ).json()
    row = next(item for item in listing["items"] if item["id"] == "tier-999")
    assert row["credits"] == 1200
    assert row["active"] is False, "下架的档位仍要能看到，否则没法重新上架"


def test_a_tier_change_shows_up_in_the_public_tier_list(client, admin_token):
    """改档位后，用户端看到的就是新价格。"""
    client.put(
        "/api/v1/admin/recharge-tiers/tier-100",
        headers=admin_auth(admin_token),
        json={"amount_fen": 1000, "credits": 150, "sort_order": 10, "active": True},
    )
    from drama_cloud.commerce.payments import service as pay_service

    # 用户端接口用的是同一个表；这里直接查 service 避免依赖登录。
    with client.app.state.pool.connection() as conn:
        tiers = pay_service.list_tiers(conn)
    row = next(item for item in tiers if item["id"] == "tier-100")
    assert row["credits"] == 150


def test_price_changes_are_audited_with_before_and_after(client, admin_token, pg):
    client.put(
        "/api/v1/admin/prices/package",
        headers=admin_auth(admin_token),
        json={"credits_per_unit": 6, "min_units": 1},
    )
    rows = pg.execute(
        "SELECT detail FROM audit_log WHERE action = 'admin.price.set'"
    ).fetchall()
    assert len(rows) == 1
    detail = rows[0]["detail"]
    # 种子价目是 5/集：改前值要记下来，否则事后查不出「从多少改成多少」。
    assert detail["before"] == {"credits_per_unit": 5, "min_units": 1}
    assert detail["after"] == {"credits_per_unit": 6, "min_units": 1}


def test_tier_upserts_are_audited_with_before_and_after(client, admin_token, pg):
    headers = admin_auth(admin_token)
    # 新增：改前是 null（这一档还不存在）。
    client.put(
        "/api/v1/admin/recharge-tiers/tier-new",
        headers=headers,
        json={"amount_fen": 500, "credits": 60, "sort_order": 40, "active": True},
    )
    # 修改：改前是上一步写入的值（conftest 种子之外新造的，不受别的测试干扰）。
    client.put(
        "/api/v1/admin/recharge-tiers/tier-new",
        headers=headers,
        json={"amount_fen": 500, "credits": 66, "sort_order": 41, "active": False},
    )
    rows = pg.execute(
        "SELECT detail FROM audit_log WHERE action = 'admin.tier.upsert' ORDER BY id"
    ).fetchall()
    assert [row["detail"] for row in rows] == [
        {
            "before": None,
            "after": {"amount_fen": 500, "credits": 60, "sort_order": 40, "active": True},
        },
        {
            "before": {"amount_fen": 500, "credits": 60, "sort_order": 40, "active": True},
            "after": {"amount_fen": 500, "credits": 66, "sort_order": 41, "active": False},
        },
]


# ---------- 邀请奖励（审查批次 C） ----------


def test_invite_settings_can_be_read_and_changed(client, admin_token, pg):
    """读写走同一个后台接口；改完之后发奖路径读到的就是新值。"""
    from drama_cloud.commerce import invites

    headers = admin_auth(admin_token)
    before = client.get("/api/v1/admin/invite-settings", headers=headers).json()
    # 迁移 0009 的种子数据：改之前先确认起点，改坏了立刻看得出来。
    assert before == {"inviterCredits": 50, "inviteeCredits": 20, "maxRewards": 20}

    changed = client.put(
        "/api/v1/admin/invite-settings",
        headers=headers,
        json={"inviter_credits": 30, "invitee_credits": 10, "max_rewards": 5},
    )
    assert changed.status_code == 200, changed.text
    assert changed.json() == {"inviterCredits": 30, "inviteeCredits": 10, "maxRewards": 5}
    # 发奖时读的就是 invites.settings（invites.py），不能出现「后台改了、发奖还用旧值」。
    assert invites.settings(pg) == {
        "inviterCredits": 30,
        "inviteeCredits": 10,
        "maxRewards": 5,
    }


def test_invite_settings_changes_are_audited_with_before_and_after(client, admin_token, pg):
    """审计要同时记改前与改后值：事后才查得出「从多少改成多少」。"""
    client.put(
        "/api/v1/admin/invite-settings",
        headers=admin_auth(admin_token),
        json={"inviter_credits": 40, "invitee_credits": 15, "max_rewards": 8},
    )
    rows = pg.execute(
        "SELECT detail FROM audit_log WHERE action = 'admin.invite_settings.set'"
    ).fetchall()
    assert len(rows) == 1, "改邀请奖励必须写审计"
    detail = rows[0]["detail"]
    # 改前值是迁移种子（50/20/20）。
    assert detail["before"] == {"inviter_credits": 50, "invitee_credits": 20, "max_rewards": 20}
    assert detail["after"] == {"inviter_credits": 40, "invitee_credits": 15, "max_rewards": 8}


@pytest.mark.parametrize(
    "payload",
    [
        {"inviter_credits": -1, "invitee_credits": 10, "max_rewards": 5},
        {"inviter_credits": 10, "invitee_credits": -1, "max_rewards": 5},
        {"inviter_credits": 10, "invitee_credits": 10, "max_rewards": 0},
        {"inviter_credits": 10, "invitee_credits": 10, "max_rewards": 5, "extra": 1},
    ],
)
def test_invalid_invite_settings_are_refused(client, admin_token, payload):
    response = client.put(
        "/api/v1/admin/invite-settings",
        headers=admin_auth(admin_token),
        json=payload,
    )
    assert response.status_code == 422, response.text


# ---------- 卡密售价（审查批次 C） ----------


def test_card_batches_view_shows_the_price(client, admin_token, pg):
    """批次带售价时后台要显示；没标价的批次显示 NULL（不是 0——0 是赠品卡）。"""
    from drama_cloud.commerce import cards

    cards.create_batch(
        pg, name="标价批次", days=30, codes=[cards.generate_code()], operator="t",
        price_fen=1999,
    )
    cards.create_batch(
        pg, name="没标价", days=30, codes=[cards.generate_code()], operator="t",
    )
    items = client.get(
        "/api/v1/admin/card-batches", headers=admin_auth(admin_token)
    ).json()["items"]
    by_name = {row["name"]: row for row in items}
    assert by_name["标价批次"]["priceFen"] == 1999
    assert by_name["没标价"]["priceFen"] is None
