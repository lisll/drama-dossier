"""积分充值与支付（M5a）。

用 Mock 渠道把「下单 → 回调 → 入账」整条链路跑通，并逐条钉住钱的边界：
伪造签名、金额不符、重复回调、订单不存在。
"""

from __future__ import annotations

import json

import pytest

from drama_cloud.commerce import credits
from drama_cloud.commerce.payments import mock as mock_provider

from .conftest import MOCK_PAY_SECRET, auth

EMAIL = "pay@example.com"
PASSWORD = "correct-horse-1"


@pytest.fixture()
def account(client, register, pg):
    body = register(EMAIL, PASSWORD)
    return {"id": body["user"]["id"], "token": body["tokens"]["access_token"]}


def _provider() -> mock_provider.MockProvider:
    return mock_provider.MockProvider(MOCK_PAY_SECRET, None)


def _notify(client, payload: dict, *, sign: bool = True, signature: str | None = None):
    body, sig = _provider().sign(payload)
    headers = {"content-type": "application/json"}
    if sign:
        headers[mock_provider.SIGNATURE_HEADER] = signature if signature is not None else sig
    return client.post("/api/v1/payments/mock/notify", content=body, headers=headers)


def _paid_payload(out_trade_no: str, amount_fen: int, trade_no: str = "mock-trade-1") -> dict:
    return {
        "out_trade_no": out_trade_no,
        "channel_trade_no": trade_no,
        "amount_fen": amount_fen,
        "status": "paid",
        "paid_at": "2026-10-17T12:00:00+00:00",
    }


def test_tiers_are_listed(client, account):
    response = client.get("/api/v1/payments/tiers", headers=auth(account["token"]))
    assert response.status_code == 200, response.text
    tiers = response.json()["items"]
    assert [tier["id"] for tier in tiers] == ["tier-100", "tier-500", "tier-2000"]
    assert tiers[0]["amountFen"] == 1000 and tiers[0]["credits"] == 100


def test_mock_order_creates_a_pending_order_and_checkout_link(client, pg, account):
    response = client.post(
        "/api/v1/payments/orders",
        headers=auth(account["token"]),
        json={"tier_id": "tier-100", "channel": "mock"},
    )
    assert response.status_code == 200, response.text
    body = response.json()
    assert body["amountFen"] == 1000 and body["credits"] == 100
    assert body["checkout"]["kind"] == "redirect"
    assert body["outTradeNo"] in body["checkout"]["payload"]
    # 没配 public_base_url 时，地址要按请求来源补全成可直接打开的完整地址：
    # 桌面端要把这个地址给用户点，只给相对路径等于点不开。
    assert body["checkout"]["payload"].startswith("http://testserver"), body["checkout"]["payload"]
    row = pg.execute(
        "SELECT status, credits, amount_fen FROM payment_orders WHERE out_trade_no = %s",
        (body["outTradeNo"],),
    ).fetchone()
    assert row["status"] == "created" and row["credits"] == 100
    # 下单不扣钱也不加钱。
    assert credits.balance(pg, account["id"]) == 0


def test_notify_credits_the_account_once(client, pg, account):
    order = client.post(
        "/api/v1/payments/orders",
        headers=auth(account["token"]),
        json={"tier_id": "tier-100", "channel": "mock"},
    ).json()

    first = _notify(client, _paid_payload(order["outTradeNo"], order["amountFen"]))
    assert first.status_code == 200, first.text
    assert first.headers["X-Credited"] == "100"
    assert credits.balance(pg, account["id"]) == 100

    # 同一笔重复投递：仍然返回成功（渠道会重投），但不再入账。
    second = _notify(client, _paid_payload(order["outTradeNo"], order["amountFen"]))
    assert second.status_code == 200, second.text
    assert second.headers["X-Credited"] == "0"
    assert credits.balance(pg, account["id"]) == 100, "重复回调不能重复入账"

    # 订单变成已支付，且记下了渠道单号。
    row = pg.execute(
        "SELECT status, channel_trade_no, paid_at FROM payment_orders WHERE out_trade_no = %s",
        (order["outTradeNo"],),
    ).fetchone()
    assert row["status"] == "paid"
    assert row["channel_trade_no"] == "mock-trade-1"
    assert row["paid_at"] is not None


def test_notify_without_signature_is_rejected(client, pg, account):
    order = client.post(
        "/api/v1/payments/orders",
        headers=auth(account["token"]),
        json={"tier_id": "tier-100", "channel": "mock"},
    ).json()
    response = _notify(client, _paid_payload(order["outTradeNo"], order["amountFen"]), sign=False)
    assert response.status_code == 400
    assert credits.balance(pg, account["id"]) == 0, "没验签就入账是最严重的问题"


def test_notify_with_wrong_signature_is_rejected(client, pg, account):
    order = client.post(
        "/api/v1/payments/orders",
        headers=auth(account["token"]),
        json={"tier_id": "tier-100", "channel": "mock"},
    ).json()
    response = _notify(
        client, _paid_payload(order["outTradeNo"], order["amountFen"]), signature="deadbeef"
    )
    assert response.status_code == 400
    assert credits.balance(pg, account["id"]) == 0


def test_notify_with_wrong_amount_is_rejected_and_audited(client, pg, account):
    order = client.post(
        "/api/v1/payments/orders",
        headers=auth(account["token"]),
        json={"tier_id": "tier-100", "channel": "mock"},
    ).json()
    # 把金额改小：签名本身是对的（改了金额后重新签），所以会走到金额校验这一步。
    response = _notify(client, _paid_payload(order["outTradeNo"], 1))
    assert response.status_code == 400
    assert credits.balance(pg, account["id"]) == 0
    # 订单仍是未支付。
    row = pg.execute(
        "SELECT status FROM payment_orders WHERE out_trade_no = %s", (order["outTradeNo"],)
    ).fetchone()
    assert row["status"] == "created"
    # 审计里留下了「金额不符」。
    actions = [
        row["action"]
        for row in pg.execute("SELECT action FROM audit_log WHERE subject = %s", (order["outTradeNo"],)).fetchall()
    ]
    assert "payment.amount_mismatch" in actions, actions


def test_notify_for_unknown_order_is_rejected(client, pg, account):
    response = _notify(client, _paid_payload("20260101000000abcdef", 1000))
    assert response.status_code == 400
    actions = [
        row["action"]
        for row in pg.execute(
            "SELECT action FROM audit_log WHERE subject = %s", ("20260101000000abcdef",)
        ).fetchall()
    ]
    assert "payment.unknown_order" in actions, actions


def test_closed_event_updates_status_without_crediting(client, pg, account):
    order = client.post(
        "/api/v1/payments/orders",
        headers=auth(account["token"]),
        json={"tier_id": "tier-100", "channel": "mock"},
    ).json()
    payload = _paid_payload(order["outTradeNo"], order["amountFen"])
    payload["status"] = "closed"
    response = _notify(client, payload)
    assert response.status_code == 200
    assert credits.balance(pg, account["id"]) == 0
    row = pg.execute(
        "SELECT status FROM payment_orders WHERE out_trade_no = %s", (order["outTradeNo"],)
    ).fetchone()
    assert row["status"] == "closed"


def test_order_lookup_is_scoped_to_the_owner(client, pg, account, register):
    order = client.post(
        "/api/v1/payments/orders",
        headers=auth(account["token"]),
        json={"tier_id": "tier-100", "channel": "mock"},
    ).json()
    # 本人能查。
    mine = client.get(f"/api/v1/payments/orders/{order['outTradeNo']}", headers=auth(account["token"]))
    assert mine.status_code == 200
    assert mine.json()["status"] == "created"
    # 别人查同一单：404（不透露单号是否存在）。
    other = register("someone-else@example.com", PASSWORD)
    theirs = client.get(
        f"/api/v1/payments/orders/{order['outTradeNo']}",
        headers=auth(other["tokens"]["access_token"]),
    )
    assert theirs.status_code == 404


def test_reconcile_against_mock_reports_not_configured(client, account):
    """Mock 没有渠道侧可查：必须如实报「查不了」，不能假装「查过了但没付」。"""
    order = client.post(
        "/api/v1/payments/orders",
        headers=auth(account["token"]),
        json={"tier_id": "tier-100", "channel": "mock"},
    ).json()
    response = client.post(
        f"/api/v1/payments/orders/{order['outTradeNo']}/reconcile",
        headers=auth(account["token"]),
    )
    assert response.status_code == 503
    assert response.json()["error"] == "payment_not_configured"


def test_unknown_channel_or_tier_is_rejected(client, account):
    bad_tier = client.post(
        "/api/v1/payments/orders",
        headers=auth(account["token"]),
        json={"tier_id": "no-such-tier", "channel": "mock"},
    )
    assert bad_tier.status_code == 404
    assert bad_tier.json()["error"] == "tier_not_found"

    bad_channel = client.post(
        "/api/v1/payments/orders",
        headers=auth(account["token"]),
        json={"tier_id": "tier-100", "channel": "paypal"},
    )
    # 渠道取值由 schema 限定，非法值是 422。
    assert bad_channel.status_code == 422


def test_alipay_without_credentials_says_not_configured(client, account):
    """没配支付宝凭证时必须明确报「未配置」，而不是用空密钥签名。"""
    response = client.post(
        "/api/v1/payments/orders",
        headers=auth(account["token"]),
        json={"tier_id": "tier-100", "channel": "alipay"},
    )
    assert response.status_code == 503
    assert response.json()["error"] == "payment_not_configured"
    assert "支付宝未配置" in response.json()["message"]


def test_wechat_without_credentials_says_not_configured(client, account):
    response = client.post(
        "/api/v1/payments/orders",
        headers=auth(account["token"]),
        json={"tier_id": "tier-100", "channel": "wechat"},
    )
    assert response.status_code == 503
    assert response.json()["error"] == "payment_not_configured"
    assert "微信支付未配置" in response.json()["message"]


def test_balance_and_history_endpoint(client, pg, account):
    credits.grant(pg, account["id"], 42, idempotency_key="seed", memo="测试")
    response = client.get("/api/v1/credits", headers=auth(account["token"]))
    assert response.status_code == 200, response.text
    body = response.json()
    assert body["balance"] == 42
    assert body["history"][0]["delta"] == 42
    assert body["history"][0]["memo"] == "测试"
