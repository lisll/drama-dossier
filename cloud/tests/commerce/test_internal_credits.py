"""内部积分接口（M5b-4）。

媒体控制面靠这四个接口扣费。它们是「服务到服务」的调用，所以：
密钥留空 → 404（不是不鉴权）；密钥错 → 401；
预扣幂等、结算/退还幂等、余额不足 402 带 required/balance。
"""

from __future__ import annotations

import dataclasses

import pytest
from fastapi.testclient import TestClient

from drama_cloud.commerce.api import create_app

from .conftest import FAST_HASHER

PASSWORD = "correct-horse-1"
SECRET = "internal-secret-for-credits-0123456789"
SECRET_HEADER = "X-Internal-Secret"


@pytest.fixture()
def with_secret(commerce_settings, pool, pg, mailer):
    settings = dataclasses.replace(commerce_settings, internal_secret=SECRET)
    app = create_app(settings, pool=pool, mailer=mailer, hasher=FAST_HASHER)
    with TestClient(app) as test_client:
        yield test_client


def _call(app, path, payload, secret=SECRET):
    return app.post(path, json=payload, headers={SECRET_HEADER: secret})


def _granted(with_secret, register, pg, email="payer@example.com", amount=20):
    """注册一个账号并直接给它充值（绕过支付，只测积分接口）。"""
    from drama_cloud.commerce import credits

    body = register(email, PASSWORD)
    user_id = body["user"]["id"]
    credits.grant(pg, user_id, amount, idempotency_key=f"test:{email}", memo="测试充值")
    return user_id, body["tokens"]["access_token"]


def test_balance_is_returned(with_secret, register, pg):
    user_id, _ = _granted(with_secret, register, pg, amount=42)
    response = _call(with_secret, "/internal/credits/balance", {"user_id": user_id})
    assert response.status_code == 200, response.text
    assert response.json()["balance"] == 42


def test_a_hold_deducts_and_returns_the_key(with_secret, register, pg):
    user_id, _ = _granted(with_secret, register, pg, amount=20)
    response = _call(
        with_secret, "/internal/credits/hold",
        {"user_id": user_id, "amount": 5, "ref": "server_task:t1"},
    )
    assert response.status_code == 200, response.text
    body = response.json()
    assert body["hold_key"] == "hold:server_task:t1"
    assert body["balance"] == 15


def test_the_same_ref_does_not_double_hold(with_secret, register, pg):
    """媒体控制面会重试；同一个 ref 重放不能扣两次。"""
    user_id, _ = _granted(with_secret, register, pg, amount=20)
    payload = {"user_id": user_id, "amount": 5, "ref": "server_task:t2"}
    first = _call(with_secret, "/internal/credits/hold", payload)
    second = _call(with_secret, "/internal/credits/hold", payload)
    assert first.status_code == 200 and second.status_code == 200
    assert second.json()["balance"] == 15


def test_insufficient_credits_is_402_with_numbers(with_secret, register, pg):
    user_id, _ = _granted(with_secret, register, pg, amount=3)
    response = _call(
        with_secret, "/internal/credits/hold",
        {"user_id": user_id, "amount": 5, "ref": "server_task:t3"},
    )
    assert response.status_code == 402, response.text
    body = response.json()
    assert body["required"] == 5 and body["balance"] == 3


def test_settle_keeps_the_deduction(with_secret, register, pg):
    user_id, _ = _granted(with_secret, register, pg, amount=20)
    _call(with_secret, "/internal/credits/hold",
          {"user_id": user_id, "amount": 5, "ref": "server_task:t4"})
    response = _call(with_secret, "/internal/credits/settle", {"ref": "server_task:t4"})
    assert response.status_code == 200, response.text
    balance = _call(with_secret, "/internal/credits/balance", {"user_id": user_id}).json()
    assert balance["balance"] == 15  # 结算后仍然是扣掉的


def test_release_gives_the_credits_back(with_secret, register, pg):
    user_id, _ = _granted(with_secret, register, pg, amount=20)
    _call(with_secret, "/internal/credits/hold",
          {"user_id": user_id, "amount": 5, "ref": "server_task:t5"})
    response = _call(with_secret, "/internal/credits/release", {"ref": "server_task:t5"})
    assert response.status_code == 200, response.text
    balance = _call(with_secret, "/internal/credits/balance", {"user_id": user_id}).json()
    assert balance["balance"] == 20  # 退回原额


def test_release_twice_does_not_refund_twice(with_secret, register, pg):
    user_id, _ = _granted(with_secret, register, pg, amount=20)
    _call(with_secret, "/internal/credits/hold",
          {"user_id": user_id, "amount": 5, "ref": "server_task:t6"})
    _call(with_secret, "/internal/credits/release", {"ref": "server_task:t6"})
    _call(with_secret, "/internal/credits/release", {"ref": "server_task:t6"})
    balance = _call(with_secret, "/internal/credits/balance", {"user_id": user_id}).json()
    assert balance["balance"] == 20


def test_settle_without_a_hold_is_404(with_secret, register, pg):
    """没有预扣却要求结算：如实报找不到，不假装成功。"""
    response = _call(with_secret, "/internal/credits/settle", {"ref": "never-held"})
    assert response.status_code == 404, response.text


def test_costing_uses_the_price_table(with_secret, register, pg):
    response = _call(with_secret, "/internal/credits/cost",
                     {"subject": "server_version", "units": 3})
    assert response.status_code == 200, response.text
    assert response.json()["required"] == 15  # 3 集 × 5


def test_costing_rejects_an_unknown_subject(with_secret):
    response = _call(with_secret, "/internal/credits/cost",
                     {"subject": "not-a-subject", "units": 1})
    assert response.status_code == 422, response.text


def test_a_wrong_secret_is_rejected_everywhere(with_secret, register, pg):
    user_id, _ = _granted(with_secret, register, pg)
    for path, payload in (
        ("/internal/credits/balance", {"user_id": user_id}),
        ("/internal/credits/hold", {"user_id": user_id, "amount": 1, "ref": "x"}),
        ("/internal/credits/settle", {"ref": "x"}),
        ("/internal/credits/release", {"ref": "x"}),
        ("/internal/credits/cost", {"subject": "server_version", "units": 1}),
    ):
        assert _call(with_secret, path, payload, secret="wrong").status_code == 401, path


def test_without_a_configured_secret_the_routes_do_not_exist(client, register, pg):
    """共享 client 没配内部密钥：这些路由必须等于不存在。"""
    user_id, _ = _granted(client, register, pg)
    assert _call(client, "/internal/credits/balance", {"user_id": user_id}).status_code == 404
