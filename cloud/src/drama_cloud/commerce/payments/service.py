"""订单与入账（M5a）。

**这一层是钱的核心，规则写在下面每条注释里。**

三条硬规则：

1. **只在「已验签的 PaymentEvent」上入账。** 调用方（路由）必须先把渠道回调变成
   `PaymentEvent`；这里不接受「回调原文」。
2. **订单锁 + 账本幂等键，两层去重。** 渠道会重复投递回调：先锁订单行看状态，
   已经是 `paid` 就直接返回；即使这一层漏了，账本的
   `idempotency_key = "order:<订单号>"` 唯一约束也会挡住第二次入账。
3. **金额与订单号必须对得上。** 渠道回调和订单金额不一致，通常意味着有人在伪造，
   一律拒绝并记审计日志。
"""

from __future__ import annotations

import uuid
from datetime import datetime, timedelta, timezone
from typing import List, Optional

from .. import credits
from ..db import audit
from ..errors import CommerceError
from .base import CreateOrder, PaymentEvent


#: 订单有效期：2 小时。和支付宝 / 微信的默认有效期一致。
ORDER_TTL_SECONDS = 2 * 3600


def list_tiers(conn) -> List[dict]:
    """可用的充值档位（下架的不返回）。"""
    rows = conn.execute(
        "SELECT id, amount_fen, credits FROM recharge_tiers WHERE active"
        " ORDER BY sort_order, amount_fen"
    ).fetchall()
    return [
        {"id": row["id"], "amountFen": int(row["amount_fen"]), "credits": int(row["credits"])}
        for row in rows
    ]


def create_order(
    conn,
    user_id: str,
    tier_id: str,
    channel: str,
    provider,
    notify_url: str,
) -> dict:
    """按档位下单。订单里的积分与金额在**下单时**定死，之后改档位不影响它。"""
    tier = conn.execute(
        "SELECT amount_fen, credits FROM recharge_tiers WHERE id = %s AND active", (tier_id,)
    ).fetchone()
    if tier is None:
        raise CommerceError(404, "tier_not_found", "充值档位不存在或已下架")
    amount_fen = int(tier["amount_fen"])
    order_credits = int(tier["credits"])
    out_trade_no = credits.new_order_no()
    expires_at = datetime.now(timezone.utc) + timedelta(seconds=ORDER_TTL_SECONDS)

    with conn.transaction():
        conn.execute(
            "INSERT INTO payment_orders"
            " (user_id, out_trade_no, channel, amount_fen, credits, status, tier_id, expires_at)"
            " VALUES (%s::uuid, %s, %s, %s, %s, 'created', %s, %s)",
            (user_id, out_trade_no, channel, amount_fen, order_credits, tier_id, expires_at),
        )
        audit(conn, user_id, "payment.order", out_trade_no,
              {"channel": channel, "amount_fen": amount_fen, "credits": order_credits})

    # 下单本身可能失败（渠道没配好 / 网络）。失败时把订单标成 closed 而不是删掉：
    # 留一行「曾经尝试过但渠道没成功」对排查有用，而删掉会让对账少一条记录。
    # 注意不能留着 `created`——那会让「待支付」的统计永远包含一笔根本付不了的订单。
    try:
        checkout = provider.create(CreateOrder(
            out_trade_no=out_trade_no,
            amount_fen=amount_fen,
            subject=f"{order_credits} 积分",
            expire_at=expires_at,
            notify_url=notify_url,
        ))
    except Exception:
        with conn.transaction():
            conn.execute(
                "UPDATE payment_orders SET status = 'closed' WHERE out_trade_no = %s",
                (out_trade_no,),
            )
            audit(conn, "system", "payment.channel_failed", out_trade_no, {"channel": channel})
        raise
    return {
        "outTradeNo": out_trade_no,
        "channel": channel,
        "checkout": {"kind": checkout.kind, "payload": checkout.payload},
        "amountFen": amount_fen,
        "credits": order_credits,
        "expiresAt": expires_at.isoformat(),
    }


def _audit_alone(conn, action: str, subject: str, detail: dict) -> None:
    """在**独立事务**里写审计。

    为什么需要它：拒绝入账时我们会 `raise`，如果把审计写在同一个事务里，
    异常会把审计一起回滚掉——而「有人拿错金额来撞接口」这件事恰恰最需要留痕。
    """
    with conn.transaction():
        audit(conn, "system", action, subject, detail)


def handle_event(conn, event: PaymentEvent) -> dict:
    """把一次已验签的支付事件落账。**幂等**：重复的同一笔只会入账一次。

    返回 `{"out_trade_no", "status", "credited", "already"}`。
    `already=True` 表示这次是重复投递，没有重复入账。
    """
    with conn.transaction():
        order = conn.execute(
            "SELECT user_id, amount_fen, credits, status FROM payment_orders"
            " WHERE out_trade_no = %s FOR UPDATE",
            (event.out_trade_no,),
        ).fetchone()
        if order is None:
            # 订单号对不上：可能是伪造，也可能是别的环境的回调打到这台机器上了。
            # 两种情况都不能入账，而且要留下痕迹（用独立事务写，见 `_audit_alone`）。
            pass
        elif event.amount_fen != int(order["amount_fen"]):
            pass
        else:
            return _apply(conn, event, order)

    # 走到这里只有两种可能：订单不存在，或金额不符。两种都要留痕再拒绝。
    if order is None:
        _audit_alone(conn, "payment.unknown_order", event.out_trade_no, {
            "channel_trade_no": event.channel_trade_no,
            "amount_fen": event.amount_fen,
        })
        raise CommerceError(404, "order_not_found", "找不到这笔订单")

    # 金额不符：几乎一定是伪造或错配。拒绝 + 记审计，不入账。
    _audit_alone(conn, "payment.amount_mismatch", event.out_trade_no, {
        "expected_fen": int(order["amount_fen"]),
        "actual_fen": event.amount_fen,
    })
    raise CommerceError(409, "amount_mismatch", "回调金额与订单不一致，已拒绝入账")


def _apply(conn, event: PaymentEvent, order) -> dict:
    """订单存在且金额对得上：入账或只更新状态。调用方已在事务里。"""
    if order["status"] == "paid":
        # 重复投递：直接告诉渠道「已处理」，不再入账。
        return {"out_trade_no": event.out_trade_no, "status": "paid",
                "credited": 0, "already": True}

    if event.status != "paid":
        # closed / refunded：只更新订单状态，不动账本。
        conn.execute(
            "UPDATE payment_orders SET status = %s WHERE out_trade_no = %s",
            (event.status, event.out_trade_no),
        )
        audit(conn, "system", f"payment.{event.status}", event.out_trade_no)
        return {"out_trade_no": event.out_trade_no, "status": event.status,
                "credited": 0, "already": False}

    user_id = str(order["user_id"])
    credited = int(order["credits"])
    conn.execute(
        "UPDATE payment_orders SET status = 'paid', paid_at = %s, channel_trade_no = %s"
        " WHERE out_trade_no = %s",
        (event.paid_at or datetime.now(timezone.utc), event.channel_trade_no, event.out_trade_no),
    )
    # 入账。幂等键用订单号：即使订单状态那层因为某种原因漏了，唯一约束也会挡住第二次。
    credits.grant(
        conn, user_id, credited,
        idempotency_key=f"order:{event.out_trade_no}",
        ref=event.out_trade_no,
        memo="充值",
        actor="system",
    )
    audit(conn, "system", "payment.paid", event.out_trade_no, {
        "amount_fen": int(order["amount_fen"]), "credits": credited,
    })
    # 首次付费后触发邀请奖励（幂等：重复付费不会重复发）。
    #
    # 为什么不放在「注册」时发：注册就发等于白送，刷号成本几乎为零。
    # 放在这里意味着「真的付过钱」才算数。
    from .. import invites

    invites.grant_on_first_payment(conn, user_id)
    return {"out_trade_no": event.out_trade_no, "status": "paid",
            "credited": credited, "already": False}


def get_order(conn, user_id: str, out_trade_no: str) -> dict:
    """查一笔订单（只能查自己的）。"""
    row = conn.execute(
        "SELECT out_trade_no, channel, amount_fen, credits, status, paid_at, created_at, expires_at"
        " FROM payment_orders WHERE out_trade_no = %s AND user_id = %s::uuid",
        (out_trade_no, user_id),
    ).fetchone()
    if row is None:
        # 别人的订单和不存在的订单回同一个错误：不透露单号是否存在。
        raise CommerceError(404, "order_not_found", "找不到这笔订单")
    return {
        "outTradeNo": row["out_trade_no"],
        "channel": row["channel"],
        "amountFen": int(row["amount_fen"]),
        "credits": int(row["credits"]),
        "status": row["status"],
        "paidAt": row["paid_at"].isoformat() if row["paid_at"] else None,
        "createdAt": row["created_at"].isoformat(),
        "expiresAt": row["expires_at"].isoformat(),
    }


def reconcile(conn, user_id: str, out_trade_no: str, provider) -> dict:
    """用户点「我已完成支付」时主动查一次单。

    回调可能因网络或服务重启丢失，这是唯一的兜底。查不到就如实说「还没付」，
    不猜、不手动置为已付。
    """
    order = get_order(conn, user_id, out_trade_no)
    if order["status"] == "paid":
        return {"status": "paid", "already": True, "credited": 0}
    event = provider.query(out_trade_no)
    return handle_event(conn, event)


def expired_orders_summary(conn) -> int:
    """统计已过期但仍未支付的订单数（运维用，只读）。"""
    row = conn.execute(
        "SELECT count(*) AS n FROM payment_orders WHERE status = 'created' AND expires_at < now()"
    ).fetchone()
    return int(row["n"])
