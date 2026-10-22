"""积分账本（M5a）。

这一组测的是「钱」，所以每条用例都要能说清「失败了会怎样」：
余额算错、重复入账、退了又扣、锁不住——每一种都是真金白银的损失。
"""

from __future__ import annotations

import threading

import pytest

from drama_cloud.commerce import credits
from drama_cloud.commerce.errors import CommerceError


def _user(pg) -> str:
    """直接插一个账号（积分只关心 user_id，不需要走注册流程）。"""
    row = pg.execute(
        "INSERT INTO users (email, password_hash, terms_version, terms_accepted_at)"
        " VALUES (%s, %s, %s, now()) RETURNING id",
        (f"credits-{threading.get_ident()}-{id(pg)}@example.test", "x", "2026-10-draft"),
    ).fetchone()
    return str(row["id"])


def test_grant_is_idempotent_and_balance_equals_sum_of_ledger(pg):
    user = _user(pg)
    assert credits.balance(pg, user) == 0

    assert credits.grant(pg, user, 100, idempotency_key="top-up-1") == 100
    # 同一个键再调一次：不重复入账。
    assert credits.grant(pg, user, 100, idempotency_key="top-up-1") == 100
    assert credits.grant(pg, user, 50, idempotency_key="top-up-2") == 150

    # 余额恒等于流水之和 —— 这是「只增不改」的核心保证。
    total = pg.execute(
        "SELECT COALESCE(SUM(delta), 0) AS total FROM credit_ledger WHERE user_id = %s::uuid",
        (user,),
    ).fetchone()["total"]
    assert int(total) == credits.balance(pg, user) == 150


def test_grant_rejects_non_positive_amount(pg):
    user = _user(pg)
    for amount in (0, -5):
        with pytest.raises(CommerceError) as caught:
            credits.grant(pg, user, amount, idempotency_key=f"bad-{amount}")
        assert caught.value.code == "invalid_request"
    assert credits.balance(pg, user) == 0


def test_grant_refuses_to_reuse_a_key_for_a_different_amount(pg):
    """同一个幂等键换了金额：说明调用方用错了键，必须报错而不是静默吞掉。"""
    user = _user(pg)
    credits.grant(pg, user, 100, idempotency_key="order:abc")
    with pytest.raises(CommerceError) as caught:
        credits.grant(pg, user, 200, idempotency_key="order:abc")
    assert caught.value.code == "idempotency_conflict"
    # 不能留下第二笔。
    assert credits.balance(pg, user) == 100


def test_balance_requires_price_rules(pg):
    """价目表少了某一行时必须显式报错，不能默认成 0（那等于免费）。"""
    pg.execute("DELETE FROM price_rules WHERE subject = 'version'")
    with pytest.raises(CommerceError) as caught:
        credits.price_per_unit(pg, "version")
    assert caught.value.code == "price_missing"


def test_cost_uses_per_episode_price_and_min_units(pg):
    # 用户定的价：一个视频 5 积分。
    assert credits.cost_for(pg, "version", 5) == 25
    assert credits.cost_for(pg, "package", 1) == 5
    # 集数是 0 或负数：不猜，直接报参数错误（调用方应当先挡住）。
    for units in (0, -1):
        with pytest.raises(CommerceError) as caught:
            credits.cost_for(pg, "version", units)
        assert caught.value.code == "invalid_request"
    # 认不出的计价对象同样报错，不默认免费。
    with pytest.raises(CommerceError):
        credits.cost_for(pg, "no-such-subject", 1)
    # 起扣下限：把 min_units 调成 3，1 集也按 3 集算。
    pg.execute("UPDATE price_rules SET min_units = 3 WHERE subject = 'version'")
    assert credits.cost_for(pg, "version", 1) == 15


def test_quote_reports_required_balance_and_enough(pg):
    user = _user(pg)
    credits.grant(pg, user, 20, idempotency_key="seed")
    short = credits.quote(pg, user, "version", 5)
    assert short == {"subject": "version", "units": 5, "required": 25, "balance": 20, "enough": False}
    credits.grant(pg, user, 10, idempotency_key="seed-2")
    enough = credits.quote(pg, user, "version", 5)
    assert enough["balance"] == 30 and enough["enough"] is True


def test_hold_deducts_immediately_and_settle_keeps_it(pg):
    user = _user(pg)
    credits.grant(pg, user, 100, idempotency_key="seed")
    with pg.transaction():
        key = credits.hold(pg, user, 25, ref="recipe-1")
    # 预扣之后余额立刻是减过的。
    assert credits.balance(pg, user) == 75
    credits.settle(pg, key)
    assert credits.balance(pg, user) == 75, "成功结算不该再退钱"
    # 结算幂等。
    credits.settle(pg, key)
    assert credits.balance(pg, user) == 75


def test_release_returns_the_money_and_only_once(pg):
    user = _user(pg)
    credits.grant(pg, user, 100, idempotency_key="seed")
    with pg.transaction():
        key = credits.hold(pg, user, 25, ref="recipe-2")
    credits.release(pg, key, memo="处理失败")
    assert credits.balance(pg, user) == 100
    # 重复退还不会退第二次。
    credits.release(pg, key)
    assert credits.balance(pg, user) == 100
    # 退过之后不能再结算。
    with pytest.raises(CommerceError) as caught:
        credits.settle(pg, key)
    assert caught.value.code == "hold_closed"


def test_settled_hold_cannot_be_released(pg):
    user = _user(pg)
    credits.grant(pg, user, 100, idempotency_key="seed")
    with pg.transaction():
        key = credits.hold(pg, user, 25, ref="recipe-3")
    credits.settle(pg, key)
    with pytest.raises(CommerceError) as caught:
        credits.release(pg, key)
    assert caught.value.code == "hold_closed"
    assert credits.balance(pg, user) == 75


def test_hold_refuses_when_balance_is_short_and_takes_nothing(pg):
    user = _user(pg)
    credits.grant(pg, user, 10, idempotency_key="seed")
    with pytest.raises(CommerceError) as caught:
        with pg.transaction():
            credits.hold(pg, user, 25, ref="recipe-4")
    error = caught.value
    assert error.status == 402, "余额不足要用 402（要钱），不是 400/422"
    assert error.code == "insufficient_credits"
    assert error.extra == {"required": 25, "balance": 10}
    # 一分钱都不能动，也不能留下 hold 流水。
    assert credits.balance(pg, user) == 10
    holds = pg.execute(
        "SELECT count(*) AS n FROM credit_ledger WHERE user_id = %s::uuid AND kind = 'hold'",
        (user,),
    ).fetchone()["n"]
    assert holds == 0


def test_stale_holds_are_released_after_the_ttl(pg):
    user = _user(pg)
    credits.grant(pg, user, 100, idempotency_key="seed")
    with pg.transaction():
        key = credits.hold(pg, user, 25, ref="recipe-5")
    assert credits.balance(pg, user) == 75
    # 把预扣的时间推到 TTL 之前：模拟「进程崩了，没人结算」。
    pg.execute(
        "UPDATE credit_ledger SET created_at = now() - make_interval(secs => %s)"
        " WHERE idempotency_key = %s",
        (credits.HOLD_TTL_SECONDS + 60, key),
    )
    # 查余额时会顺带回收。
    assert credits.balance(pg, user) == 100, "悬挂的预扣应当被自动退还"
    # 再查一次不会重复退。
    assert credits.balance(pg, user) == 100
    releases = pg.execute(
        "SELECT count(*) AS n FROM credit_ledger WHERE user_id = %s::uuid AND kind = 'expire'",
        (user,),
    ).fetchone()["n"]
    assert releases == 1


def test_stale_server_task_holds_survive_the_ttl_and_can_still_settle(pg):
    """服务端处理的预扣不参与自动回收。

    任务要排队、失败还会重试，预扣超过 2 小时是常态；自动回收会让任务完成后
    结算得到 409，等于白送积分。它的结算与退还由媒体服务在任务终态主动完成。
    """
    user = _user(pg)
    credits.grant(pg, user, 100, idempotency_key="seed")
    with pg.transaction():
        key = credits.hold(pg, user, 25, ref="server_task:tsk_1")
    # 推到 TTL 之前：模拟「任务还在排队，还没到终态」。
    pg.execute(
        "UPDATE credit_ledger SET created_at = now() - make_interval(secs => %s)"
        " WHERE idempotency_key = %s",
        (credits.HOLD_TTL_SECONDS + 60, key),
    )
    # 查余额（顺带触发回收）后，这笔预扣仍然在扣着。
    assert credits.balance(pg, user) == 75, "服务端处理的预扣不该被 TTL 回收"
    # 媒体服务按自己的 ref 找回幂等键（它不知道我们内部的键格式）。
    assert credits.hold_key_for(pg, "server_task:tsk_1") == key
    # 任务完成，正常结算——钱是花掉的，不是退回的。
    credits.settle(pg, key)
    assert credits.balance(pg, user) == 75, "结算后这笔钱就是花掉了"
    # 预扣已进终态，退款路径拒绝（模拟「媒体侧想退」的迟到调用不会双花）。
    with pytest.raises(CommerceError) as caught:
        credits.release(pg, key)
    assert caught.value.code == "hold_closed"


def test_concurrent_holds_cannot_both_spend_the_same_money(pg, database_dsn):
    """两个人（或两个线程）同时预扣同一笔余额：只能有一个成功。

    这是「余额检查与写入之间不能有人插进来」的直接验证——
    没有用户行锁的话，两边都会读到 100、都以为够，最后余额变成负数。
    """
    import psycopg
    from psycopg.rows import dict_row

    user = _user(pg)
    credits.grant(pg, user, 100, idempotency_key="seed")

    results = []
    barrier = threading.Barrier(2)

    def attempt(index: int) -> None:
        # 行工厂必须和 `db.connect` 一致（dict_row）：生产代码按列名取值。
        with psycopg.connect(database_dsn, autocommit=True, row_factory=dict_row) as conn:
            barrier.wait(timeout=5)
            try:
                with conn.transaction():
                    credits.hold(conn, user, 60, ref=f"concurrent-{index}")
                results.append("ok")
            except CommerceError as error:
                results.append(error.code)

    threads = [threading.Thread(target=attempt, args=(index,)) for index in range(2)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout=20)

    assert sorted(results) == ["insufficient_credits", "ok"], results
    # 只扣了一次 60。
    assert credits.balance(pg, user) == 40


def test_history_lists_newest_first_with_kinds(pg):
    user = _user(pg)
    credits.grant(pg, user, 100, idempotency_key="h1", memo="充值")
    with pg.transaction():
        key = credits.hold(pg, user, 25, ref="recipe-6")
    credits.release(pg, key, memo="退还")

    rows = credits.history(pg, user)
    kinds = [row["kind"] for row in rows]
    # 新到旧：release 是最后发生的。
    assert kinds[0] == "release"
    assert "hold" in kinds and "grant" in kinds
    assert rows[0]["memo"] == "退还"
    assert rows[0]["delta"] == 25, "退还是正数"


def test_ledger_has_no_update_or_delete_path():
    """账本只增不改：确认代码里没有改金额或删流水的入口。

    这条不能靠单元测试「证明」，只能靠检查源码里不存在这类语句。
    写在这里是为了让它在 CI 里跑一次——有人哪天加了 UPDATE credit_ledger，
    除了改 `settled_state`（那是状态，不是金额）以外，这里就会失败。
    """
    import inspect
    import re

    source = inspect.getsource(credits)
    # 允许的 UPDATE：只改 settled_state（预扣的去向）。
    updates = re.findall(r"UPDATE credit_ledger SET ([^\"]+)", source)
    assert updates, "预期至少有一处更新预扣状态"
    for target in updates:
        assert target.strip().startswith("settled_state"), f"不许改流水金额或其它列：{target}"
    assert "DELETE FROM credit_ledger" not in source, "账本不许删行"
