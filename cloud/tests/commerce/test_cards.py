"""卡密兑换：月卡顺延、永久卡、重复兑换、别人用过、作废、两人同时兑换同一张卡。"""

from __future__ import annotations

import threading

import pytest

from drama_cloud.commerce import cards, entitlements
from drama_cloud.commerce.db import lock_user
from drama_cloud.commerce.errors import CommerceError
from drama_cloud.commerce.license_routes import REDEEM_PER_USER

from .conftest import auth, new_cards, rival_lock

DAY = 86400


def redeem(client, token: str, code: str):
    return client.post("/api/v1/cards/redeem", json={"code": code}, headers=auth(token))


def days_left(pg, user_id: str) -> float:
    row = pg.execute(
        "SELECT extract(epoch FROM expires_at - now()) AS left FROM entitlements WHERE user_id = %s::uuid",
        (user_id,),
    ).fetchone()
    return float(row["left"]) / DAY


def set_entitlement(pg, user_id: str, kind: str, days_from_now=None) -> None:
    pg.execute(
        """
        INSERT INTO entitlements (user_id, kind, expires_at)
        VALUES (%s::uuid, %s, CASE WHEN %s::int IS NULL THEN NULL ELSE now() + %s::int * interval '1 day' END)
        ON CONFLICT (user_id) DO UPDATE SET kind = excluded.kind, expires_at = excluded.expires_at
        """,
        (user_id, kind, days_from_now, days_from_now),
    )


@pytest.fixture()
def user(register):
    body = register()
    return body["user"]["id"], body["tokens"]["access_token"]


def test_first_monthly_card_starts_now(client, pg, user):
    user_id, token = user
    code = new_cards(pg)[0]
    response = redeem(client, token, cards.display(code).lower())  # 大小写、前缀、短横线都不影响
    assert response.status_code == 200, response.text
    body = response.json()
    assert body["already_redeemed"] is False and body["card"] == {"kind": "timed", "days": 30}
    assert body["entitlement"]["kind"] == "timed" and body["entitlement"]["active"] is True
    assert 29.99 < days_left(pg, user_id) <= 30
    row = pg.execute("SELECT redeemed_by, redeemed_at FROM cards").fetchone()
    assert str(row["redeemed_by"]) == user_id and row["redeemed_at"] is not None
    me = client.get("/api/v1/me", headers=auth(token)).json()
    assert me["entitlement"] == body["entitlement"]


def test_monthly_card_extends_from_current_expiry(client, pg, user):
    user_id, token = user
    set_entitlement(pg, user_id, "timed", 5)
    assert redeem(client, token, new_cards(pg)[0]).status_code == 200
    assert 34.99 < days_left(pg, user_id) <= 35


def test_monthly_card_after_expiry_starts_from_now(client, pg, user):
    user_id, token = user
    set_entitlement(pg, user_id, "timed", -10)
    assert client.get("/api/v1/me", headers=auth(token)).json()["entitlement"]["active"] is False
    assert redeem(client, token, new_cards(pg)[0]).status_code == 200
    assert 29.99 < days_left(pg, user_id) <= 30  # 不是 20 天：过期的那段不补


def test_lifetime_card_replaces_timed(client, pg, user):
    user_id, token = user
    set_entitlement(pg, user_id, "timed", 12)
    body = redeem(client, token, new_cards(pg, days=None)[0]).json()
    assert body["card"] == {"kind": "lifetime", "days": None}
    assert body["entitlement"] == {"kind": "lifetime", "active": True, "expires_at": None}


@pytest.mark.parametrize("days", [30, None])
def test_lifetime_account_rejects_more_cards(client, pg, user, days):
    user_id, token = user
    set_entitlement(pg, user_id, "lifetime")
    response = redeem(client, token, new_cards(pg, days=days)[0])
    assert response.status_code == 409 and response.json()["error"] == "already_lifetime"
    # 卡没被用掉，账号也没被降成按天计。
    assert pg.execute("SELECT count(*) AS n FROM cards WHERE redeemed_by IS NULL").fetchone()["n"] == 1
    assert entitlements.current(pg, user_id).kind == "lifetime"


def test_grant_never_downgrades_lifetime(pg, user):
    user_id, _ = user
    set_entitlement(pg, user_id, "lifetime")
    for days in (30, None):
        with pytest.raises(CommerceError) as caught:
            entitlements.grant(pg, user_id, days)
        assert caught.value.code == "already_lifetime"
    row = pg.execute("SELECT kind, expires_at FROM entitlements").fetchone()
    assert row["kind"] == "lifetime" and row["expires_at"] is None


def test_same_user_redeeming_twice_adds_nothing(client, pg, user):
    user_id, token = user
    code = new_cards(pg)[0]
    first = redeem(client, token, code).json()
    again = redeem(client, token, code)
    assert again.status_code == 200 and again.json()["already_redeemed"] is True
    assert again.json()["entitlement"] == first["entitlement"]
    assert 29.99 < days_left(pg, user_id) <= 30


def test_card_used_by_someone_else(client, pg, register):
    code = new_cards(pg)[0]
    first = register("first@example.com")["tokens"]["access_token"]
    second = register("second@example.com")
    assert redeem(client, first, code).status_code == 200
    response = redeem(client, second["tokens"]["access_token"], code)
    assert response.status_code == 409 and response.json()["error"] == "card_used"
    assert entitlements.current(pg, second["user"]["id"]).kind == "none"


def test_revoked_batch(client, pg, user):
    _, token = user
    used, unused = new_cards(pg, count=2)
    assert redeem(client, token, used).status_code == 200
    batch = pg.execute("SELECT id FROM card_batches").fetchone()["id"]
    assert cards.revoke_batch(pg, str(batch), "test") == {"already": False, "unused": 1}
    assert cards.revoke_batch(pg, str(batch), "test") == {"already": True, "unused": 1}
    response = redeem(client, token, unused)
    assert response.status_code == 409 and response.json()["error"] == "card_revoked"
    # 作废之前兑换的不受影响，重复提交仍是幂等的成功。
    assert redeem(client, token, used).json()["already_redeemed"] is True


def test_two_people_redeem_the_same_card_at_once(client, pg, register, database_dsn):
    code = new_cards(pg)[0]
    users = [register(f"racer{index}@example.com") for index in range(2)]
    results = {}

    def attempt(index):
        results[index] = redeem(client, users[index]["tokens"]["access_token"], code)

    lock = "SELECT 1 FROM cards WHERE code_digest = %s FOR UPDATE"
    with rival_lock(database_dsn, pg, lock, (cards.digest(code),), "FOR UPDATE OF c", 2) as blocked:
        threads = [threading.Thread(target=attempt, args=(index,)) for index in range(2)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=30)
    assert blocked, "两个兑换请求没有同时卡在同一张卡上，竞争场景没有构造出来"
    statuses = sorted(response.status_code for response in results.values())
    assert statuses == [200, 409], [response.text for response in results.values()]
    loser = next(response for response in results.values() if response.status_code == 409)
    assert loser.json()["error"] == "card_used"
    winners = [user["user"]["id"] for user in users if entitlements.current(pg, user["user"]["id"]).active]
    assert len(winners) == 1
    assert str(pg.execute("SELECT redeemed_by FROM cards").fetchone()["redeemed_by"]) == winners[0]


@pytest.mark.parametrize("code", ["", "ABCD", "DD-ABCD-EFGH-JKLM-NPQ0", "x" * 100])
def test_malformed_code_is_422(client, user, code):
    response = redeem(client, user[1], code)
    assert response.status_code == 422
    assert response.json()["fields"] == [{"field": "code", "message": cards.FORMAT_MESSAGE}]


def test_redeem_is_rate_limited_per_user(client, user, fresh_window):
    fresh_window(REDEEM_PER_USER.window_seconds, needed_seconds=30)
    _, token = user
    for _ in range(REDEEM_PER_USER.limit):
        assert redeem(client, token, cards.generate_code()).status_code == 404
    response = redeem(client, token, cards.generate_code())
    assert response.status_code == 429 and response.json()["error"] == "rate_limited"
    assert int(response.headers["Retry-After"]) >= 1


def test_unknown_code_and_auth(client, pg, user):
    _, token = user
    response = redeem(client, token, cards.generate_code())
    assert response.status_code == 404 and response.json()["error"] == "card_not_found"
    code = new_cards(pg)[0]
    assert client.post("/api/v1/cards/redeem", json={"code": code}).status_code == 401
    assert client.post("/api/v1/cards/redeem", json={"code": code}, params={"token": token}).status_code == 401
    assert pg.execute("SELECT count(*) AS n FROM cards WHERE redeemed_by IS NULL").fetchone()["n"] == 1


def test_audit_records_ids_not_codes(client, pg, user):
    user_id, token = user
    code = new_cards(pg)[0]
    redeem(client, token, code)
    row = pg.execute("SELECT actor, detail FROM audit_log WHERE action = 'card.redeem'").fetchone()
    assert row["actor"] == user_id and row["detail"]["kind"] == "timed"
    assert row["detail"]["before"]["kind"] == "none" and row["detail"]["after"]["kind"] == "timed"
    dumped = str(pg.execute("SELECT detail FROM audit_log").fetchall())
    assert code not in dumped and cards.display(code) not in dumped


def test_lock_user_rejects_unknown_user(pg):
    with pytest.raises(CommerceError) as caught:
        with pg.transaction():
            lock_user(pg, "00000000-0000-4000-8000-000000000000")
    assert caught.value.status == 401
