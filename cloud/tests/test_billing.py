from __future__ import annotations

import pytest

from drama_cloud import billing
from drama_cloud.auth import create_user
from drama_cloud.errors import WorkError


def test_quote_reserve_settle_is_idempotent(settings, conn):
    token = create_user(conn, "billing-user", role="operator", actor="test")
    user_id = conn.execute("SELECT id FROM users WHERE name = %s", ("billing-user",)).fetchone()["id"]
    billing.grant_credits(conn, user_id, 20, idempotency_key="pay:1", actor="test")
    quote = billing.create_quote(conn, user_id, units=3, unit_cost=2, idempotency_key="quote:1")
    assert quote["amount"] == 6
    assert billing.reserve_quote(conn, quote["id"], user_id, "task-1")["status"] == "reserved"
    assert billing.get_balance(conn, user_id)["balance"] == 14
    # 相同 reserve/settle 请求不会重复扣款或退款。
    assert billing.reserve_quote(conn, quote["id"], user_id, "task-1")["status"] == "reserved"
    assert billing.settle_quote(conn, quote["id"], user_id, "task-1", True)["status"] == "settled"
    assert billing.settle_quote(conn, quote["id"], user_id, "task-1", True)["status"] == "settled"
    assert billing.get_balance(conn, user_id)["balance"] == 14


def test_failed_settle_releases_hold(settings, conn):
    create_user(conn, "billing-user", role="operator", actor="test")
    user_id = conn.execute("SELECT id FROM users WHERE name = %s", ("billing-user",)).fetchone()["id"]
    billing.grant_credits(conn, user_id, 5, idempotency_key="pay:1")
    quote = billing.create_quote(conn, user_id, units=5, idempotency_key="quote:1")
    billing.reserve_quote(conn, quote["id"], user_id, "task-1")
    billing.settle_quote(conn, quote["id"], user_id, "task-1", False)
    assert billing.get_balance(conn, user_id)["balance"] == 5
    assert billing.get_quote(conn, quote["id"])["status"] == "released"


def test_reserve_never_overdraws(settings, conn):
    create_user(conn, "billing-user", role="operator", actor="test")
    user_id = conn.execute("SELECT id FROM users WHERE name = %s", ("billing-user",)).fetchone()["id"]
    quote = billing.create_quote(conn, user_id, units=1, idempotency_key="quote:1")
    with pytest.raises(WorkError, match="余额不足"):
        billing.reserve_quote(conn, quote["id"], user_id, "task-1")
