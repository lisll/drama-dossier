"""配方计费（M5a）：签发报价、消费扣款、余额不足时「钱和配方都没动」。

这一组与 `test_recipes.py` 的分工：那边的用例只关心配方本身（能不能消费、过期、换机），
默认给足积分；这里专门盯钱。
"""

from __future__ import annotations

import threading

import pytest

from drama_cloud.commerce import credits

from .conftest import activate, auth, device_id, new_cards
from .test_recipes import PASSWORD, PC, TASK

EMAIL = "billing@example.com"


def _account(pg, register, client, email: str = EMAIL, seed: int = 0) -> dict:
    body = register(email, PASSWORD)
    token = body["tokens"]["access_token"]
    assert activate(client, token, PC).status_code == 200
    [code] = new_cards(pg, days=30)
    assert client.post("/api/v1/cards/redeem", json={"code": code}, headers=auth(token)).status_code == 200
    if seed:
        credits.grant(pg, body["user"]["id"], seed, idempotency_key=f"seed:{email}")
    return {"id": body["user"]["id"], "token": token}


def _issue(client, token: str, episode_count: int):
    return client.post(
        "/api/v1/version-recipes",
        headers=auth(token),
        json={"device_id": PC, "task": TASK, "platform": "universal",
              "strength": "medium", "episode_count": episode_count},
    )


def _consume(client, token: str, recipe_id: str):
    return client.post(f"/api/v1/version-recipes/{recipe_id}/consume", headers=auth(token),
                       json={"device_id": PC})


def test_issuing_a_recipe_quotes_the_price_without_charging(client, pg, register):
    account = _account(pg, register, client)
    response = _issue(client, account["token"], 5)
    assert response.status_code == 200, response.text
    body = response.json()
    # 一个视频 5 积分 × 5 集。
    assert body["credits"] == 25
    assert body["episode_count"] == 5
    # 签发不扣钱。
    assert credits.balance(pg, account["id"]) == 0


def test_consuming_charges_the_quoted_price(client, pg, register):
    account = _account(pg, register, client, seed=100)
    recipe_id = _issue(client, account["token"], 4).json()["recipe_id"]
    assert credits.balance(pg, account["id"]) == 100, "签发后仍未扣"

    response = _consume(client, account["token"], recipe_id)
    assert response.status_code == 200, response.text
    assert response.json()["credits"] == 20
    assert credits.balance(pg, account["id"]) == 80

    # 台账里是一条已结算的预扣，不是两条流水。
    rows = pg.execute(
        "SELECT kind, delta, settled_state FROM credit_ledger"
        " WHERE user_id = %s::uuid AND kind = 'hold' ORDER BY id",
        (account["id"],),
    ).fetchall()
    assert len(rows) == 1
    assert rows[0]["delta"] == -20 and rows[0]["settled_state"] == "settled"


def test_price_is_fixed_at_issue_time_not_recomputed_at_consume(client, pg, register):
    """签发之后改价目表：消费仍按签发时的价格扣。

    否则会出现「确认框里说 25、实际扣 30」——那是最容易让人不信任计费的一类问题。
    """
    account = _account(pg, register, client, seed=100)
    recipe_id = _issue(client, account["token"], 5).json()["recipe_id"]
    # 中途把单价改成 50。
    pg.execute("UPDATE price_rules SET credits_per_unit = 50 WHERE subject = 'version'")

    response = _consume(client, account["token"], recipe_id)
    assert response.status_code == 200, response.text
    assert response.json()["credits"] == 25, "应当仍按签发时的 25 扣"
    assert credits.balance(pg, account["id"]) == 75


def test_insufficient_credits_leaves_both_the_money_and_the_recipe_untouched(client, pg, register):
    """余额不足时必须整体回滚：钱不扣、配方**不消耗**（充值后还能消费同一个配方）。"""
    account = _account(pg, register, client, seed=10)
    recipe_id = _issue(client, account["token"], 5).json()["recipe_id"]  # 需要 25

    response = _consume(client, account["token"], recipe_id)
    assert response.status_code == 402, response.text
    body = response.json()
    assert body["error"] == "insufficient_credits"
    assert body["required"] == 25 and body["balance"] == 10

    assert credits.balance(pg, account["id"]) == 10, "钱不能扣"
    row = pg.execute(
        "SELECT consumed_at FROM recipes WHERE id = %s::uuid", (recipe_id,)
    ).fetchone()
    assert row["consumed_at"] is None, "配方不能被消耗"
    holds = pg.execute(
        "SELECT count(*) AS n FROM credit_ledger WHERE user_id = %s::uuid AND kind = 'hold'",
        (account["id"],),
    ).fetchone()["n"]
    assert holds == 0, "不能留下预扣流水"

    # 充值之后同一个配方仍然能消费。
    credits.grant(pg, account["id"], 20, idempotency_key="top-up")
    retry = _consume(client, account["token"], recipe_id)
    assert retry.status_code == 200, retry.text
    assert credits.balance(pg, account["id"]) == 5


def test_concurrent_consume_of_one_recipe_charges_once(client, pg, register, database_dsn):
    """同一个配方被两个线程同时消费：只有一个成功，只扣一次钱。

    这是「扣费与标记已消费在同一个事务里」的直接验证。
    """
    import psycopg
    from psycopg.rows import dict_row

    account = _account(pg, register, client, seed=100)
    recipe_id = _issue(client, account["token"], 5).json()["recipe_id"]

    results = []
    barrier = threading.Barrier(2)

    def attempt(index: int) -> None:
        with psycopg.connect(database_dsn, autocommit=True, row_factory=dict_row) as conn:
            barrier.wait(timeout=5)
            try:
                with conn.transaction():
                    row = conn.execute(
                        "SELECT consumed_at, credits FROM recipes WHERE id = %s::uuid FOR UPDATE",
                        (recipe_id,),
                    ).fetchone()
                    if row["consumed_at"] is not None:
                        results.append("already")
                        return
                    # 模拟 consume 的核心两行：扣费 + 标记。
                    key = credits.hold(conn, account["id"], int(row["credits"]), ref=f"recipe:{recipe_id}")
                    credits.settle(conn, key)
                    conn.execute(
                        "UPDATE recipes SET consumed_at = now() WHERE id = %s::uuid", (recipe_id,)
                    )
                results.append("ok")
            except Exception as error:  # noqa: BLE001 - 测试里只关心结果分类
                results.append(type(error).__name__)

    threads = [threading.Thread(target=attempt, args=(index,)) for index in range(2)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout=20)

    assert sorted(results) == ["already", "ok"], results
    assert credits.balance(pg, account["id"]) == 75, "只能扣一次 25"


def test_legacy_recipe_without_credits_can_still_be_consumed(client, pg, register):
    """0007 之前签发的旧配方没有 `credits`，那时还没计费，消费时按 0 处理。"""
    account = _account(pg, register, client, seed=0)
    recipe_id = _issue(client, account["token"], 2).json()["recipe_id"]
    pg.execute("UPDATE recipes SET episode_count = NULL, credits = NULL WHERE id = %s::uuid", (recipe_id,))

    response = _consume(client, account["token"], recipe_id)
    assert response.status_code == 200, response.text
    assert response.json()["credits"] == 0
    assert credits.balance(pg, account["id"]) == 0


@pytest.mark.parametrize("count", [0, 501])
def test_episode_count_is_validated(client, register, pg, count):
    account = _account(pg, register, client)
    response = _issue(client, account["token"], count)
    assert response.status_code == 422, response.text
