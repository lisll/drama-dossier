"""积分账本（M5a）。

**只增不改。** 余额一律由 `SUM(delta)` 算，不存可变的 `balance` 字段。
理由见迁移 0006 的注释：余额是钱，存一个可变字段就意味着「余额」和「流水」是两份
独立数据，算错一次之后再也无法从流水推回正确余额。

三种流水形态：

* `grant` / `reward` / `expire`：纯加减，没有去向；
* `hold`：预扣，`settled_state = 'reserved'`；预扣期间余额已经减掉了；
* 预扣的结局有两种，都不再写新流水改变金额方向：
  - 成功：把 hold 标 `settled`（钱就是花掉了）；
  - 失败：写一条**新的** `release` 正数流水，并把原 hold 标 `released`。

为什么失败要写新流水而不是把 hold 删掉或改成 0：账本只增不改，
「退过钱」这件事本身必须留下痕迹，否则事后看不出一笔钱进来过又出去过。
"""

from __future__ import annotations

import secrets
from datetime import datetime, timedelta, timezone
from typing import List, Optional

from .db import lock_user
from .errors import CommerceError

#: 预扣的悬挂时限：超过这么久还没结算，视为放弃并自动退还。
#:
#: 只用于「报审包」这类**服务端自己执行**的任务。处理版本的预扣与结算在同一个事务里
#: 完成，不存在悬挂状态。**服务端处理**（`server_task:` 前缀）在 `release_stale_holds`
#: 里被明确排除：它排队 + 最多 3 次重试、每次 ffmpeg 上限 30 分钟，超过 2 小时是常态，
#: 预扣的结算与退还由媒体服务在任务终态主动完成，任何一条路径都会调
#: `settle_task_billing`（runner.py / service.py）——中途被自动退回会让任务完成后
#: 结算得到 409，变成「白送积分」。
HOLD_TTL_SECONDS = 2 * 3600

#: 目前支持的计价对象。
SUBJECTS = ("version", "package", "server_version")

#: 媒体控制面持有的预扣引用前缀。`release_stale_holds` 不回收这类预扣（见上）。
SERVER_TASK_REF_PREFIX = "server_task:"


def _idempotency_key(kind: str, ref: str) -> str:
    return f"{kind}:{ref}"


def hold_key_for(conn, ref: str) -> Optional[str]:
    """按业务引用找回某笔预扣的幂等键；没有这笔预扣时返回 None。

    「服务端处理」的控制面只知道自己的 `ref`（例如 `server_task:<task_id>`），
    不知道我们内部的幂等键格式，所以给它这个入口去结算/退还。
    """
    key = _idempotency_key("hold", ref)
    row = conn.execute(
        "SELECT 1 FROM credit_ledger WHERE idempotency_key = %s AND kind = 'hold'", (key,)
    ).fetchone()
    return key if row else None


def balance(conn, user_id: str) -> int:
    """当前可用积分。

    先把这个人悬挂过久的预扣回收掉，再算余额：否则用户看到的余额里有一笔其实已经
    该退回来的钱，会以为自己的积分丢了。
    """
    release_stale_holds(conn, user_id=user_id)
    return _sum(conn, user_id)


def price_per_unit(conn, subject: str) -> int:
    """某个计价对象每个单位多少积分。"""
    row = conn.execute(
        "SELECT credits_per_unit FROM price_rules WHERE subject = %s", (subject,)
    ).fetchone()
    if row is None:
        # 计价对象是代码里的常量，取不到说明迁移没跑或有人删了数据。不能默认成 0
        # （那等于免费），也不能猜一个数。
        raise CommerceError(503, "price_missing", f"缺少计价规则：{subject}，请联系管理员")
    return int(row["credits_per_unit"])


def quote(conn, user_id: str, subject: str, units: int) -> dict:
    """算这次要花多少、够不够。**不扣钱**，只用于在界面上提前告诉用户价格。"""
    required = cost_for(conn, subject, units)
    current = balance(conn, user_id)
    return {
        "subject": subject,
        "units": units,
        "required": required,
        "balance": current,
        "enough": current >= required,
    }


def cost_for(conn, subject: str, units: int) -> int:
    """按价目表算总价；`units` 受起扣下限约束。"""
    if units <= 0:
        raise CommerceError(422, "invalid_request", "计价单位数必须大于 0")
    if subject not in SUBJECTS:
        raise CommerceError(422, "invalid_request", f"不认识的计价对象：{subject}")
    row = conn.execute(
        "SELECT credits_per_unit, min_units FROM price_rules WHERE subject = %s", (subject,)
    ).fetchone()
    if row is None:
        raise CommerceError(503, "price_missing", f"缺少计价规则：{subject}，请联系管理员")
    effective = max(units, int(row["min_units"]))
    return effective * int(row["credits_per_unit"])


def grant(
    conn,
    user_id: str,
    amount: int,
    *,
    idempotency_key: str,
    kind: str = "grant",
    ref: Optional[str] = None,
    memo: str = "",
    actor: str = "system",
) -> int:
    """加积分（充值、邀请奖励）。同一个幂等键只入账一次。

    支付回调必须先自己完成验签，再调这里。
    """
    if amount <= 0:
        raise CommerceError(422, "invalid_request", "加积分的数量必须大于 0")
    if kind not in ("grant", "reward"):
        raise CommerceError(422, "invalid_request", f"这个操作不能用来加积分：{kind}")
    if not idempotency_key.strip():
        raise CommerceError(422, "invalid_request", "加积分需要一个幂等键")
    with conn.transaction():
        existing = conn.execute(
            "SELECT user_id, delta FROM credit_ledger WHERE idempotency_key = %s",
            (idempotency_key,),
        ).fetchone()
        if existing is not None:
            # 同一个键已经用过：参数必须完全一样，否则说明调用方把键用错了地方。
            if str(existing["user_id"]) != user_id or int(existing["delta"]) != amount:
                raise CommerceError(
                    409, "idempotency_conflict", "这个幂等键已经用于别的账户或金额"
                )
            return balance(conn, user_id)
        _insert(
            conn,
            user_id=user_id,
            delta=amount,
            kind=kind,
            idempotency_key=idempotency_key,
            ref=ref,
            memo=memo,
        )
        _audit(conn, actor, "credit.grant", user_id, {"amount": amount, "kind": kind})
        return balance(conn, user_id)


def hold(
    conn,
    user_id: str,
    amount: int,
    *,
    ref: str,
    memo: str = "",
    actor: str = "system",
) -> str:
    """预扣积分，返回这条流水的幂等键（结算 / 退还时要用）。

    **必须在调用方已经开启的事务里调用**（例如配方消费的同一个事务）：
    余额检查与写入之间不能有别人插进来。

    加锁顺序：**先锁用户行，再动流水**。与 `settle` / `release` 保持一致
    （它们只动流水、不需要用户行锁，但顺序固定才不会互相等死）。
    """
    if amount <= 0:
        raise CommerceError(422, "invalid_request", "预扣数量必须大于 0")
    if not ref.strip():
        raise CommerceError(422, "invalid_request", "预扣需要一个业务引用")
    key = _idempotency_key("hold", ref)
    # 幂等：同一个 ref 再来一次直接返回已有键，不再扣一次。
    # 调用方（媒体控制面）会在网络抖动时重试预扣，这一步必须安全。
    existing = conn.execute(
        "SELECT settled_state FROM credit_ledger WHERE idempotency_key = %s AND kind = 'hold'",
        (key,),
    ).fetchone()
    if existing is not None:
        return key
    lock_user(conn, user_id)
    # 先回收悬挂的旧预扣，再判断余额：否则用户可能因为几笔早就该退回的钱而「余额不足」。
    # 这里已经在用户行锁内，回收只碰这个人的流水，不会和别人抢。
    release_stale_holds(conn, user_id=user_id)
    current = _sum(conn, user_id)
    if current < amount:
        raise CommerceError(
            402,
            "insufficient_credits",
            f"积分不足：需要 {amount} 积分，当前 {current} 积分",
            extra={"required": amount, "balance": current},
        )
    _insert(
        conn,
        user_id=user_id,
        delta=-amount,
        kind="hold",
        idempotency_key=key,
        ref=ref,
        settled_state="reserved",
        memo=memo,
    )
    _audit(conn, actor, "credit.hold", user_id, {"amount": amount, "ref": ref})
    return key


def settle(conn, hold_key: str, actor: str = "system") -> None:
    """预扣成功：标记为已消费。重复结算不报错（幂等）。"""
    with conn.transaction():
        row = conn.execute(
            "SELECT settled_state, ref, user_id FROM credit_ledger"
            " WHERE idempotency_key = %s AND kind = 'hold' FOR UPDATE",
            (hold_key,),
        ).fetchone()
        if row is None:
            raise CommerceError(404, "hold_not_found", "找不到这笔预扣")
        if row["settled_state"] == "settled":
            return
        if row["settled_state"] != "reserved":
            raise CommerceError(409, "hold_closed", "这笔预扣已经退还，不能再结算")
        conn.execute(
            "UPDATE credit_ledger SET settled_state = 'settled' WHERE idempotency_key = %s",
            (hold_key,),
        )
        _audit(conn, actor, "credit.settle", str(row["user_id"]), {"ref": row["ref"]})


def release(conn, hold_key: str, *, memo: str = "", actor: str = "system") -> None:
    """预扣失败：原路退还。重复退还不会退第二次（幂等）。

    写一条 `release` 正数流水，并把原 hold 标为 `released` —— 见模块注释。
    """
    with conn.transaction():
        row = conn.execute(
            "SELECT user_id, delta, ref, settled_state FROM credit_ledger"
            " WHERE idempotency_key = %s AND kind = 'hold' FOR UPDATE",
            (hold_key,),
        ).fetchone()
        if row is None:
            raise CommerceError(404, "hold_not_found", "找不到这笔预扣")
        if row["settled_state"] == "released":
            return
        if row["settled_state"] != "reserved":
            raise CommerceError(409, "hold_closed", "这笔预扣已经消费，不能退还")
        user_id = str(row["user_id"])
        amount = -int(row["delta"])
        conn.execute(
            "UPDATE credit_ledger SET settled_state = 'released' WHERE idempotency_key = %s",
            (hold_key,),
        )
        _insert(
            conn,
            user_id=user_id,
            delta=amount,
            kind="release",
            idempotency_key=_idempotency_key("release", hold_key),
            ref=row["ref"],
            memo=memo or "预扣退还",
        )
        _audit(conn, actor, "credit.release", user_id, {"amount": amount, "ref": row["ref"]})


def release_stale_holds(conn, user_id: Optional[str] = None) -> int:
    """回收悬挂过久的预扣（进程崩溃、服务重启后留下 `reserved`）。

    返回回收的笔数。`user_id` 给了就只扫这个人（查余额时用，开销小）；
    不给就扫全部（管理端清理用）。

    **服务端处理的预扣（`ref` 以 `server_task:` 开头）不回收**：任务的结算与退还
    由媒体服务在终态主动完成，回收时限盖不住排队 + 重试的正常时长，见
    [`HOLD_TTL_SECONDS`][drama_cloud.commerce.credits.HOLD_TTL_SECONDS] 的注释。

    **整个扫描与改写都在一个事务里**：`FOR UPDATE SKIP LOCKED` 必须在事务内才有意义，
    否则锁立刻释放，两个并发调用会挑中同一批行。
    """
    cutoff = datetime.now(timezone.utc) - timedelta(seconds=HOLD_TTL_SECONDS)
    sql = (
        "SELECT idempotency_key, user_id, delta, ref FROM credit_ledger"
        " WHERE kind = 'hold' AND settled_state = 'reserved' AND created_at < %s"
        " AND (ref IS NULL OR ref NOT LIKE %s)"
    )
    params: List = [cutoff, SERVER_TASK_REF_PREFIX + "%"]
    if user_id is not None:
        sql += " AND user_id = %s::uuid"
        params.append(user_id)
    released = 0
    with conn.transaction():
        rows = conn.execute(sql + " FOR UPDATE SKIP LOCKED", params).fetchall()
        for row in rows:
            key = row["idempotency_key"]
            # 再确认一次状态：并发调用可能已经处理过这一行。
            claimed = conn.execute(
                "UPDATE credit_ledger SET settled_state = 'released'"
                " WHERE idempotency_key = %s AND settled_state = 'reserved'",
                (key,),
            )
            if claimed.rowcount != 1:
                continue
            amount = -int(row["delta"])
            _insert(
                conn,
                user_id=str(row["user_id"]),
                delta=amount,
                kind="expire",
                idempotency_key=_idempotency_key("expire", key),
                ref=row["ref"],
                memo="预扣超时未结算，已自动退还",
            )
            released += 1
        if released:
            _audit(conn, "system", "credit.expire", user_id or "", {"count": released})
    return released


def history(conn, user_id: str, limit: int = 50) -> List[dict]:
    """最近的流水（新到旧）。只用于展示，不参与任何判断。"""
    limit = max(1, min(int(limit), 200))
    rows = conn.execute(
        "SELECT delta, kind, ref, settled_state, memo, created_at FROM credit_ledger"
        " WHERE user_id = %s::uuid ORDER BY created_at DESC, id DESC LIMIT %s",
        (user_id, limit),
    ).fetchall()
    return [
        {
            "delta": int(row["delta"]),
            "kind": row["kind"],
            "ref": row["ref"],
            "settledState": row["settled_state"],
            "memo": row["memo"],
            "createdAt": row["created_at"].isoformat(),
        }
        for row in rows
    ]


# ---------------------------------------------------------------------------
# 内部
# ---------------------------------------------------------------------------


def _sum(conn, user_id: str) -> int:
    row = conn.execute(
        "SELECT COALESCE(SUM(delta), 0) AS total FROM credit_ledger WHERE user_id = %s::uuid",
        (user_id,),
    ).fetchone()
    return int(row["total"])


def _insert(
    conn,
    *,
    user_id: str,
    delta: int,
    kind: str,
    idempotency_key: str,
    ref: Optional[str] = None,
    settled_state: Optional[str] = None,
    memo: str = "",
) -> None:
    conn.execute(
        "INSERT INTO credit_ledger (user_id, delta, kind, idempotency_key, ref, settled_state, memo)"
        " VALUES (%s::uuid, %s, %s, %s, %s, %s, %s)",
        (user_id, delta, kind, idempotency_key, ref, settled_state, memo),
    )


def _audit(conn, actor: str, action: str, subject: str, detail: dict) -> None:
    # 延迟导入：db.audit 与这里互相引用会让模块加载顺序变脆。
    from .db import audit

    audit(conn, actor, action, subject, detail)


def new_order_no() -> str:
    """订单号：时间前缀 + 随机段，便于按时间排序排查。"""
    stamp = datetime.now(timezone.utc).strftime("%Y%m%d%H%M%S")
    return f"{stamp}{secrets.token_hex(6)}"
