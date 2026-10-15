"""本地积分账本的最小状态机。

这里故意只实现 quote → task → settle 的幂等账务边界：

* 余额和保留额只通过事务修改，不能由客户端直接覆盖；
* reserve 时先扣除可用余额，成功 settle 保持扣款，失败 settle 原路 release；
* 重复请求返回已有状态，不重复扣款或退款；
* 不接 Z-Pay、卡密、keyring、SMBIOS/注册表指纹或邀请奖励。那些属于独立的外部
  身份/支付集成，应在服务端完成验签后调用本模块的 grant_credits。
"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone
from typing import Optional

from .db import audit, now_iso, transaction
from .errors import ErrorClass, WorkError


def _id(prefix: str) -> str:
    import secrets

    return f"{prefix}_{secrets.token_urlsafe(12)}"


def _parse_iso(value: str) -> datetime:
    return datetime.fromisoformat(value.replace("Z", "+00:00"))


def _account(conn, user_id: str):
    row = conn.execute("SELECT user_id, balance FROM billing_accounts WHERE user_id = ?", (user_id,)).fetchone()
    if row is None:
        conn.execute(
            "INSERT INTO billing_accounts (user_id, balance, updated_at) VALUES (?,?,?)",
            (user_id, 0, now_iso()),
        )
        return conn.execute("SELECT user_id, balance FROM billing_accounts WHERE user_id = ?", (user_id,)).fetchone()
    return row


def grant_credits(conn, user_id: str, amount: int, *, idempotency_key: str, actor: str = "system") -> dict:
    """给本地账户充值积分；外部支付回调必须先完成自己的验签。"""
    if amount <= 0 or not idempotency_key.strip():
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "充值 amount 必须为正数且需要幂等键")
    with transaction(conn):
        existing = conn.execute(
            "SELECT user_id, delta FROM billing_ledger WHERE idempotency_key = ?", (idempotency_key,)
        ).fetchone()
        if existing is not None:
            if existing["user_id"] != user_id or existing["delta"] != amount:
                raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "充值幂等键已用于不同账户或金额")
            return get_balance(conn, user_id)
        _account(conn, user_id)
        conn.execute(
            "UPDATE billing_accounts SET balance = balance + ?, updated_at = ? WHERE user_id = ?",
            (amount, now_iso(), user_id),
        )
        conn.execute(
            "INSERT INTO billing_ledger (idempotency_key,user_id,delta,kind,created_at) VALUES (?,?,?,?,?)",
            (idempotency_key, user_id, amount, "grant", now_iso()),
        )
        audit(conn, actor, "billing.grant", user_id, {"amount": amount, "idempotency_key": idempotency_key})
    return get_balance(conn, user_id)


def get_balance(conn, user_id: str) -> dict:
    row = _account(conn, user_id)
    return {"user_id": row["user_id"], "balance": int(row["balance"])}


def create_quote(conn, user_id: str, units: int, unit_cost: int = 1, ttl_seconds: int = 900,
                 idempotency_key: Optional[str] = None, actor: str = "system") -> dict:
    if units <= 0 or unit_cost <= 0 or not 60 <= ttl_seconds <= 86400:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "quote 参数超出范围")
    quote_id = idempotency_key or _id("quo")
    with transaction(conn):
        if idempotency_key:
            existing = conn.execute(
                "SELECT * FROM billing_quotes WHERE id = ?", (idempotency_key,)
            ).fetchone()
            if existing:
                if (existing["user_id"] != user_id or existing["units"] != units
                        or existing["unit_cost"] != unit_cost):
                    raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "报价幂等键已用于不同参数")
                return dict(existing)
        now = datetime.now(timezone.utc).replace(microsecond=0)
        amount = units * unit_cost
        conn.execute(
            "INSERT INTO billing_quotes (id,user_id,units,unit_cost,amount,status,expires_at,created_at,updated_at) "
            "VALUES (?,?,?,?,?,'issued',?,?,?)",
            (quote_id, user_id, units, unit_cost, amount,
             (now + timedelta(seconds=ttl_seconds)).isoformat(), now.isoformat(), now.isoformat()),
        )
        audit(conn, actor, "billing.quote", quote_id,
              {"user_id": user_id, "units": units, "amount": amount})
    return get_quote(conn, quote_id)


def get_quote(conn, quote_id: str) -> dict:
    row = conn.execute("SELECT * FROM billing_quotes WHERE id = ?", (quote_id,)).fetchone()
    if row is None:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"报价不存在：{quote_id}")
    return dict(row)


def reserve_quote(conn, quote_id: str, user_id: str, task_id: str, actor: str = "system") -> dict:
    with transaction(conn):
        _reserve_quote_locked(conn, quote_id, user_id, task_id, actor)
    return get_quote(conn, quote_id)


def _reserve_quote_locked(conn, quote_id: str, user_id: str, task_id: str, actor: str) -> None:
    """在调用方已持有 SQLite 写事务时保留报价，可与任务创建原子提交。"""
    row = conn.execute("SELECT * FROM billing_quotes WHERE id = ?", (quote_id,)).fetchone()
    if row is None:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"报价不存在：{quote_id}")
    if row["user_id"] != user_id:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "报价不属于当前用户")
    if row["status"] in ("reserved", "settled", "released"):
        if row["task_id"] != task_id:
            raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "报价已绑定其他任务")
        return
    if row["status"] != "issued":
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"报价状态不可保留：{row['status']}")
    if _parse_iso(row["expires_at"]) <= datetime.now(timezone.utc):
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "报价已过期")
    account = _account(conn, user_id)
    if int(account["balance"]) < int(row["amount"]):
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "积分余额不足")
    conn.execute("UPDATE billing_accounts SET balance=balance-?, updated_at=? WHERE user_id=?",
                 (row["amount"], now_iso(), user_id))
    conn.execute("INSERT INTO billing_ledger (idempotency_key,user_id,quote_id,task_id,delta,kind,created_at) "
                 "VALUES (?,?,?,?,?,?,?)",
                 (f"hold:{quote_id}", user_id, quote_id, task_id, -row["amount"], "hold", now_iso()))
    conn.execute("UPDATE billing_quotes SET status='reserved', task_id=?, updated_at=? WHERE id=?",
                 (task_id, now_iso(), quote_id))
    audit(conn, actor, "billing.reserve", quote_id, {"task_id": task_id, "amount": row["amount"]})


def settle_quote(conn, quote_id: str, user_id: str, task_id: str, success: bool,
                 actor: str = "system") -> dict:
    with transaction(conn):
        row = conn.execute("SELECT * FROM billing_quotes WHERE id = ?", (quote_id,)).fetchone()
        if row is None or row["user_id"] != user_id:
            raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "报价不存在或不属于当前用户")
        if row["task_id"] != task_id:
            raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "结算任务与报价不匹配")
        if row["status"] in ("settled", "released"):
            return dict(row)
        if row["status"] != "reserved":
            raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"报价状态不可结算：{row['status']}")
        if success:
            status = "settled"
        else:
            conn.execute("UPDATE billing_accounts SET balance=balance+?, updated_at=? WHERE user_id=?",
                         (row["amount"], now_iso(), user_id))
            conn.execute("INSERT INTO billing_ledger (idempotency_key,user_id,quote_id,task_id,delta,kind,created_at) "
                         "VALUES (?,?,?,?,?,?,?)",
                         (f"release:{quote_id}", user_id, quote_id, task_id, row["amount"], "release", now_iso()))
            status = "released"
        conn.execute("UPDATE billing_quotes SET status=?, updated_at=? WHERE id=?", (status, now_iso(), quote_id))
        audit(conn, actor, "billing.settle", quote_id, {"task_id": task_id, "success": success, "status": status})
    return get_quote(conn, quote_id)
