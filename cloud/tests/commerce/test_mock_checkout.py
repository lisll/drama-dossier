"""本地模拟收银台（M5a）。

这个页面是 Mock 支付闭环的最后一环，所以测的重点不是「页面好看」，而是：
1. 页面上的按钮发出去的回调**能真的入账**（说明它没有自己造第二条入账路径）；
2. 页面里的签名是真的（改动报文就对不上）；
3. 订单不存在 / 缺订单号时不假装能付。
"""

from __future__ import annotations

import base64
import re

import pytest

from drama_cloud.commerce import credits
from drama_cloud.commerce.payments import mock as mock_provider

from .conftest import MOCK_PAY_SECRET, auth

EMAIL = "checkout@example.com"
PASSWORD = "correct-horse-1"


@pytest.fixture()
def account(client, register, pg):
    body = register(EMAIL, PASSWORD)
    return {"id": body["user"]["id"], "token": body["tokens"]["access_token"]}


def _order(client, account, tier_id: str = "tier-100") -> dict:
    response = client.post(
        "/api/v1/payments/orders",
        headers=auth(account["token"]),
        json={"tier_id": tier_id, "channel": "mock"},
    )
    assert response.status_code == 200, response.text
    return response.json()


def _buttons(page: str) -> dict:
    """把页面里每个按钮的 (报文, 签名) 抠出来。报文在页面上是 base64。"""
    found = {}
    pattern = re.compile(
        r"data-action='(?P<action>[^']+)'\s+data-body='(?P<body>[A-Za-z0-9+/=]*)'\s+"
        r"data-signature='(?P<signature>[0-9a-f]+)'"
    )
    for match in pattern.finditer(page):
        body = base64.b64decode(match.group("body")).decode("utf-8")
        found[match.group("action")] = (body, match.group("signature"))
    return found


def test_checkout_renders_all_four_buttons(client, account):
    order = _order(client, account)
    response = client.get(f"/pay/mock/checkout?out_trade_no={order['outTradeNo']}")
    assert response.status_code == 200, response.text
    buttons = _buttons(response.text)
    assert set(buttons) == {"paid", "paid_again", "closed", "tampered"}
    # 订单信息在页面上：金额与积分来自订单，不是页面自己编的。
    assert str(order["amountFen"]) in response.text
    assert "待支付" in response.text


def test_checkout_button_actually_credits_the_account(client, account, pg):
    """页面上的「支付成功」按钮：用它生成的那份报文+签名去打回调，必须真的入账。"""
    order = _order(client, account)
    page = client.get(f"/pay/mock/checkout?out_trade_no={order['outTradeNo']}").text
    body, signature = _buttons(page)["paid"]

    response = client.post(
        "/api/v1/payments/mock/notify",
        content=body.encode("utf-8"),
        headers={"content-type": "application/json", mock_provider.SIGNATURE_HEADER: signature},
    )
    assert response.status_code == 200, response.text
    assert response.headers["X-Credited"] == str(order["credits"])
    assert credits.balance(pg, account["id"]) == order["credits"]


def test_paid_and_paid_again_are_byte_identical(client, account):
    """「重复回调」必须发逐字节相同的报文，否则测不到幂等。"""
    order = _order(client, account)
    buttons = _buttons(client.get(f"/pay/mock/checkout?out_trade_no={order['outTradeNo']}").text)
    assert buttons["paid"] == buttons["paid_again"]


def test_repeated_callback_from_page_does_not_double_credit(client, account, pg):
    order = _order(client, account)
    buttons = _buttons(client.get(f"/pay/mock/checkout?out_trade_no={order['outTradeNo']}").text)
    body, signature = buttons["paid_again"]
    headers = {"content-type": "application/json", mock_provider.SIGNATURE_HEADER: signature}

    first = client.post("/api/v1/payments/mock/notify", content=body.encode("utf-8"), headers=headers)
    second = client.post("/api/v1/payments/mock/notify", content=body.encode("utf-8"), headers=headers)
    assert first.status_code == 200 and second.status_code == 200
    assert second.headers["X-Credited"] == "0"
    assert credits.balance(pg, account["id"]) == order["credits"]


def test_tampered_button_is_rejected(client, account, pg):
    """「伪造金额」按钮：签名是真的，但金额与订单不符，必须拒绝且不入账。"""
    order = _order(client, account)
    buttons = _buttons(client.get(f"/pay/mock/checkout?out_trade_no={order['outTradeNo']}").text)
    body, signature = buttons["tampered"]
    response = client.post(
        "/api/v1/payments/mock/notify",
        content=body.encode("utf-8"),
        headers={"content-type": "application/json", mock_provider.SIGNATURE_HEADER: signature},
    )
    assert response.status_code == 400, response.text
    assert credits.balance(pg, account["id"]) == 0


def test_closed_button_closes_the_order_without_crediting(client, account, pg):
    order = _order(client, account)
    buttons = _buttons(client.get(f"/pay/mock/checkout?out_trade_no={order['outTradeNo']}").text)
    body, signature = buttons["closed"]
    response = client.post(
        "/api/v1/payments/mock/notify",
        content=body.encode("utf-8"),
        headers={"content-type": "application/json", mock_provider.SIGNATURE_HEADER: signature},
    )
    assert response.status_code == 200, response.text
    row = pg.execute(
        "SELECT status FROM payment_orders WHERE out_trade_no = %s", (order["outTradeNo"],)
    ).fetchone()
    assert row["status"] == "closed"
    assert credits.balance(pg, account["id"]) == 0


def test_unknown_order_shows_a_plain_message(client):
    response = client.get("/pay/mock/checkout?out_trade_no=NOPE")
    assert response.status_code == 200
    assert "找不到这笔订单" in response.text
    # 查不到订单时不能给按钮，否则点下去只会拿到 order_not_found。
    assert _buttons(response.text) == {}


def test_missing_order_no_asks_for_it(client):
    response = client.get("/pay/mock/checkout")
    assert response.status_code == 200
    assert "缺少订单号" in response.text
    assert _buttons(response.text) == {}


def test_page_does_not_leak_anything_but_the_order(client, account):
    """页面上不该出现签名密钥本身。"""
    order = _order(client, account)
    page = client.get(f"/pay/mock/checkout?out_trade_no={order['outTradeNo']}").text
    assert MOCK_PAY_SECRET not in page
