"""邀请码与防刷（M5a）。

四道闸各测一条，外加「奖励只发一次」与「被邀请人没付费就不发」。
"""

from __future__ import annotations

import pytest

from drama_cloud.commerce import credits, invites
from drama_cloud.commerce.payments import mock as mock_provider

from .conftest import MOCK_PAY_SECRET, auth, device_id

PASSWORD = "correct-horse-1"


def _user_id(body: dict) -> str:
    return body["user"]["id"]


@pytest.fixture()
def inviter(client, register, pg):
    body = register("inviter@example.com", PASSWORD)
    user_id = _user_id(body)
    # 注册时已写入邀请码。
    code = pg.execute("SELECT invite_code FROM users WHERE id = %s::uuid", (user_id,)).fetchone()[
        "invite_code"
    ]
    assert code, "注册后应当有邀请码"
    return {"id": user_id, "code": code, "token": body["tokens"]["access_token"]}


def test_every_account_gets_a_stable_invite_code(client, register, pg):
    body = register("codes@example.com", PASSWORD)
    user_id = _user_id(body)
    row = pg.execute("SELECT invite_code FROM users WHERE id = %s::uuid", (user_id,)).fetchone()
    # 可复现：由 user_id 派生，重复算得到同一个值。
    assert row["invite_code"] == invites.invite_code_for(user_id)
    assert len(row["invite_code"]) == 10


def test_register_with_a_valid_code_binds_the_inviter(client, register, pg, inviter):
    body = register("invitee@example.com", PASSWORD)
    invitee = _user_id(body)
    # 注册接口本身不接受邀请码时，用 bind 直接验绑定逻辑（接口层在下面单独测）。
    bound = invites.bind(pg, invitee, inviter["code"], None)
    assert bound == inviter["id"]
    row = pg.execute("SELECT invited_by FROM users WHERE id = %s::uuid", (invitee,)).fetchone()
    assert str(row["invited_by"]) == inviter["id"]
    # 邀请记录里有待发奖励的一行。
    reward = pg.execute(
        "SELECT granted_at FROM invite_rewards WHERE invitee_id = %s::uuid", (invitee,)
    ).fetchone()
    assert reward["granted_at"] is None


def test_register_endpoint_binds_inviter_when_code_is_sent(client, mailer, pg, inviter):
    """走真实注册接口带邀请码：应当当场绑上邀请人，并留下待发奖励。

    这条钉的是「接口层真的把 invite_code 传下去了」——桌面端注册表单里的
    邀请码就是打到这个字段，光测 invites.bind 覆盖不到这里。
    """
    email = "via-api@example.com"
    assert client.post("/api/v1/auth/codes", json={"email": email, "purpose": "register"}).status_code == 202
    response = client.post(
        "/api/v1/auth/register",
        json={
            "email": email,
            "code": mailer.last_code(email),
            "password": PASSWORD,
            "accept_terms": True,
            "invite_code": inviter["code"],
            "device_id": "a" * 64,
        },
    )
    assert response.status_code == 201, response.text
    invitee = _user_id(response.json())
    row = pg.execute("SELECT invited_by FROM users WHERE id = %s::uuid", (invitee,)).fetchone()
    assert str(row["invited_by"]) == inviter["id"]
    reward = pg.execute(
        "SELECT granted_at FROM invite_rewards WHERE invitee_id = %s::uuid", (invitee,)
    ).fetchone()
    assert reward is not None and reward["granted_at"] is None


def test_register_endpoint_ignores_a_wrong_code(client, mailer, pg):
    """邀请码填错不能让注册失败——用户可能只是手滑打错了。"""
    email = "typo-api@example.com"
    assert client.post("/api/v1/auth/codes", json={"email": email, "purpose": "register"}).status_code == 202
    response = client.post(
        "/api/v1/auth/register",
        json={
            "email": email,
            "code": mailer.last_code(email),
            "password": PASSWORD,
            "accept_terms": True,
            "invite_code": "NOSUCHCODE",
        },
    )
    assert response.status_code == 201, response.text
    row = pg.execute(
        "SELECT invited_by FROM users WHERE id = %s::uuid", (_user_id(response.json()),)
    ).fetchone()
    assert row["invited_by"] is None


def test_an_invalid_code_does_not_break_registration(client, register, pg):
    body = register("typo@example.com", PASSWORD)
    bound = invites.bind(pg, _user_id(body), "NOSUCHCODE", None)
    assert bound is None
    row = pg.execute(
        "SELECT invited_by FROM users WHERE id = %s::uuid", (_user_id(body),)
    ).fetchone()
    assert row["invited_by"] is None, "填错邀请码不该绑上，也不该让注册失败"


def test_a_user_cannot_be_invited_twice(client, register, pg, inviter):
    other = register("second-inviter@example.com", PASSWORD)
    other_code = pg.execute(
        "SELECT invite_code FROM users WHERE id = %s::uuid", (_user_id(other),)
    ).fetchone()["invite_code"]
    invitee = _user_id(register("twice@example.com", PASSWORD))
    assert invites.bind(pg, invitee, inviter["code"], None) == inviter["id"]
    # 已经被邀请过：第二次不生效，也不覆盖。
    assert invites.bind(pg, invitee, other_code, None) is None
    row = pg.execute("SELECT invited_by FROM users WHERE id = %s::uuid", (invitee,)).fetchone()
    assert str(row["invited_by"]) == inviter["id"]


def test_self_invitation_is_ignored(client, register, pg):
    body = register("self@example.com", PASSWORD)
    user_id = _user_id(body)
    code = pg.execute("SELECT invite_code FROM users WHERE id = %s::uuid", (user_id,)).fetchone()[
        "invite_code"
    ]
    assert invites.bind(pg, user_id, code, None) is None
    row = pg.execute("SELECT invited_by FROM users WHERE id = %s::uuid", (user_id,)).fetchone()
    assert row["invited_by"] is None


def _paid_order(pg, user_id: str) -> None:
    """给这个账号造一笔已支付订单：邀请奖励要求「真的付过钱」。"""
    pg.execute(
        "INSERT INTO payment_orders"
        " (user_id, out_trade_no, channel, amount_fen, credits, status, paid_at, channel_trade_no, expires_at)"
        " VALUES (%s::uuid, %s, 'mock', 1000, 100, 'paid', now(), %s, now() + interval '1 hour')",
        (user_id, f"test-{user_id[:8]}", f"trade-{user_id[:8]}"),
    )


def test_rewards_are_not_granted_before_the_invitee_pays(client, register, pg, inviter):
    invitee = _user_id(register("nopay@example.com", PASSWORD))
    invites.bind(pg, invitee, inviter["code"], None)
    # 没付费：一次都不该发。
    assert invites.grant_on_first_payment(pg, invitee) is None
    assert credits.balance(pg, inviter["id"]) == 0
    assert credits.balance(pg, invitee) == 0


def test_first_payment_grants_both_sides_and_only_once(client, register, pg, inviter):
    invitee = _user_id(register("payer@example.com", PASSWORD))
    invites.bind(pg, invitee, inviter["code"], None)
    _paid_order(pg, invitee)

    # 直接调用「首次付费后发奖」（支付回调里也是调它）。
    result = invites.grant_on_first_payment(pg, invitee)
    assert result == {"inviter": 50, "invitee": 20}
    assert credits.balance(pg, inviter["id"]) == 50
    assert credits.balance(pg, invitee) == 20

    # 再触发一次：不重复发。
    assert invites.grant_on_first_payment(pg, invitee) is None
    assert credits.balance(pg, inviter["id"]) == 50
    assert credits.balance(pg, invitee) == 20

    # 奖励在账本里标成 reward 而不是 grant，便于对账时区分来源。
    kinds = {
        row["kind"]
        for row in pg.execute(
            "SELECT kind FROM credit_ledger WHERE user_id = %s::uuid", (inviter["id"],)
        ).fetchall()
    }
    assert kinds == {"reward"}


def test_reward_cap_stops_further_rewards(client, register, pg, inviter):
    pg.execute("UPDATE invite_settings SET max_rewards = 1")
    first = _user_id(register("cap-1@example.com", PASSWORD))
    invites.bind(pg, first, inviter["code"], None)
    _paid_order(pg, first)
    assert invites.grant_on_first_payment(pg, first) is not None

    second = _user_id(register("cap-2@example.com", PASSWORD))
    invites.bind(pg, second, inviter["code"], None)
    _paid_order(pg, second)
    assert invites.grant_on_first_payment(pg, second) is None, "超过上限不再发"
    assert credits.balance(pg, first) == 20
    assert credits.balance(pg, second) == 0


def test_same_device_cannot_feed_two_invited_accounts(client, register, pg, inviter):
    """同一台电脑（机器码）最多给一个账号当邀请来源。"""
    pc = device_id("shared-pc")
    first = _user_id(register("device-a@example.com", PASSWORD))
    second = _user_id(register("device-b@example.com", PASSWORD))

    # 第一个账号：按这台电脑绑上。
    assert invites.bind(pg, first, inviter["code"], pc) == inviter["id"]
    # 让第一个账号「有设备记录」，防刷才有东西可比。
    pg.execute(
        "INSERT INTO devices (user_id, device_id, name, platform, app_version)"
        " VALUES (%s::uuid, %s, %s, %s, %s)",
        (first, pc, "共享电脑", "linux", "0.0.1"),
    )
    # 第二个账号用同一台电脑：拒绝。
    assert invites.bind(pg, second, inviter["code"], pc) is None
    row = pg.execute("SELECT invited_by FROM users WHERE id = %s::uuid", (second,)).fetchone()
    assert row["invited_by"] is None


def test_payment_callback_triggers_the_invite_reward(client, register, pg, inviter):
    """端到端：被邀请人真的走一次支付回调，双方都该拿到奖励。

    这是「邀请」与「支付」两条链路的接点，单独测一遍，
    防止有人在重构回调时把这行调用删掉——那样奖励就永远不会发。
    """
    body = register("real-payer@example.com", PASSWORD)
    invitee = _user_id(body)
    token = body["tokens"]["access_token"]
    invites.bind(pg, invitee, inviter["code"], None)

    order = client.post(
        "/api/v1/payments/orders",
        headers=auth(token),
        json={"tier_id": "tier-100", "channel": "mock"},
    ).json()
    provider = mock_provider.MockProvider(MOCK_PAY_SECRET, None)
    payload = {
        "out_trade_no": order["outTradeNo"],
        "channel_trade_no": "mock-invite-1",
        "amount_fen": order["amountFen"],
        "status": "paid",
        "paid_at": "2026-10-17T12:00:00+00:00",
    }
    notify_body, signature = provider.sign(payload)
    response = client.post(
        "/api/v1/payments/mock/notify",
        content=notify_body,
        headers={"content-type": "application/json", mock_provider.SIGNATURE_HEADER: signature},
    )
    assert response.status_code == 200, response.text

    # 被邀请人：充值的 100 + 受邀奖励 20。
    assert credits.balance(pg, invitee) == 120
    # 邀请人：50。
    assert credits.balance(pg, inviter["id"]) == 50


def test_invite_status_reports_code_and_earnings(client, register, pg, inviter):
    response = client.get("/api/v1/invites", headers=auth(inviter["token"]))
    assert response.status_code == 200, response.text
    body = response.json()
    assert body["code"] == inviter["code"]
    assert body["invited"] == 0 and body["rewards"] == 0
    assert body["inviterCredits"] == 50 and body["inviteeCredits"] == 20
    assert body["maxRewards"] == 20
