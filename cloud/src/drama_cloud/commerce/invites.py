"""邀请码与防刷（M5a）。

四道闸，缺一不可：

1. **一个账号只能被邀请一次**：`users.invited_by` 只写一次，重复绑定不覆盖；
2. **同一台电脑最多给一个账号当邀请来源**：刷号的人通常在同一台机器上注册多个账号；
   用已有的 `devices.device_id`（机器码）判断，不需要新的指纹；
3. **被邀请人真的付过钱才发奖励**：注册就发等于白送，刷号成本几乎为零；
4. **每个邀请人有奖励次数上限**：防止有人用一批小号互相邀请。

奖励发放本身走 `credits.grant()`，幂等键是 `invite:<被邀请人>`，
所以重复触发（多次付费、重复调用）都只会发一次。
"""

from __future__ import annotations

import hashlib
from typing import Optional

from . import credits
from .db import audit, lock_user
from .errors import CommerceError


def invite_code_for(user_id: str) -> str:
    """由 user_id 派生邀请码：可复现、不需要额外存储、也不会撞（取 10 位十六进制）。"""
    digest = hashlib.sha256(f"invite:{user_id}".encode("utf-8")).hexdigest()
    return digest[:10].upper()


def settings(conn) -> dict:
    row = conn.execute(
        "SELECT inviter_credits, invitee_credits, max_rewards FROM invite_settings"
    ).fetchone()
    if row is None:
        # 配置行是迁移里的种子数据。丢了就不能发奖：宁可报错，也不要按 0 发。
        raise CommerceError(503, "invite_not_configured", "邀请奖励还没配置，请联系管理员")
    return {
        "inviterCredits": int(row["inviter_credits"]),
        "inviteeCredits": int(row["invitee_credits"]),
        "maxRewards": int(row["max_rewards"]),
    }


def find_by_code(conn, code: str) -> Optional[str]:
    """按邀请码找邀请人。找不到返回 None（不报错：注册时填错码不该拦住注册）。"""
    normalized = (code or "").strip().upper()
    if not normalized:
        return None
    row = conn.execute(
        "SELECT id FROM users WHERE invite_code = %s AND deleted_at IS NULL", (normalized,)
    ).fetchone()
    return str(row["id"]) if row else None


def bind(conn, invitee_id: str, code: str, device_id: Optional[str]) -> Optional[str]:
    """把新账号绑到邀请人。返回邀请人 ID；没绑上（码无效 / 触发防刷）返回 None。

    **不抛异常**：注册流程不应该因为邀请码无效而失败——用户可能是手滑打错了。
    但防刷触发会记审计日志，方便事后查。
    """
    inviter_id = find_by_code(conn, code)
    if inviter_id is None:
        return None
    if inviter_id == invitee_id:
        # 自己邀请自己：直接忽略。
        audit(conn, invitee_id, "invite.self", invitee_id)
        return None
    with conn.transaction():
        lock_user(conn, invitee_id)
        current = conn.execute(
            "SELECT invited_by FROM users WHERE id = %s::uuid", (invitee_id,)
        ).fetchone()
        if current is None or current["invited_by"] is not None:
            # 已经绑过邀请人了，不覆盖。
            return None
        # 闸 2：这台电脑上已经给别的账号当过邀请来源，就不再算。
        if device_id and _device_already_used(conn, device_id, invitee_id):
            audit(conn, invitee_id, "invite.same_device", inviter_id,
                  {"device": hashlib.sha256(device_id.encode()).hexdigest()[:16]})
            return None
        conn.execute(
            "UPDATE users SET invited_by = %s::uuid WHERE id = %s::uuid",
            (inviter_id, invitee_id),
        )
        conn.execute(
            "INSERT INTO invite_rewards (inviter_id, invitee_id) VALUES (%s::uuid, %s::uuid)"
            " ON CONFLICT (invitee_id) DO NOTHING",
            (inviter_id, invitee_id),
        )
        audit(conn, invitee_id, "invite.bind", inviter_id)
    return inviter_id


def _device_already_used(conn, device_id: str, invitee_id: str) -> bool:
    """这台电脑是否已经和**别的**被邀请账号绑定过。

    只看「被邀请过」的账号：正常用户自己的一台电脑绑自己一个账号，不受影响。
    """
    row = conn.execute(
        """
        SELECT 1 FROM devices d
        JOIN users u ON u.id = d.user_id
        WHERE d.device_id = %s AND u.invited_by IS NOT NULL AND d.user_id <> %s::uuid
        LIMIT 1
        """,
        (device_id, invitee_id),
    ).fetchone()
    return row is not None


def grant_on_first_payment(conn, invitee_id: str) -> Optional[dict]:
    """被邀请人完成首次付费后调用：给双方发奖励。

    **自己再确认一次「真的付过钱」**，不信任调用方：这个方法一旦被误调用（例如接在错误
    的事件上、或者被别的代码顺手调用），就是白送积分。多查一次 SQL 比信任调用方便宜。

    幂等：账本幂等键是 `invite:<被邀请人>` / `invitee:<被邀请人>`，
    重复调用不会重复发。返回 `{"inviter": N, "invitee": M}` 或 None（没有可发的）。
    """
    paid = conn.execute(
        "SELECT 1 FROM payment_orders WHERE user_id = %s::uuid AND status = 'paid' LIMIT 1",
        (invitee_id,),
    ).fetchone()
    if paid is None:
        # 没付过钱：不发。这不是错误，只是时机没到——被邀请人下次付费还会走到这里。
        return None
    with conn.transaction():
        row = conn.execute(
            "SELECT inviter_id, granted_at FROM invite_rewards WHERE invitee_id = %s::uuid FOR UPDATE",
            (invitee_id,),
        ).fetchone()
        if row is None or row["granted_at"] is not None:
            return None
        inviter_id = str(row["inviter_id"])
        config = settings(conn)
        # 闸 4：邀请人的奖励次数上限。
        used = conn.execute(
            "SELECT count(*) AS n FROM invite_rewards WHERE inviter_id = %s::uuid AND granted_at IS NOT NULL",
            (inviter_id,),
        ).fetchone()["n"]
        if int(used) >= config["maxRewards"]:
            audit(conn, "system", "invite.capped", inviter_id, {"used": int(used)})
            return None

        inviter_credits = config["inviterCredits"]
        invitee_credits = config["inviteeCredits"]
        if inviter_credits > 0:
            credits.grant(
                conn, inviter_id, inviter_credits,
                idempotency_key=f"invite:{invitee_id}",
                kind="reward", ref=invitee_id, memo="邀请奖励",
            )
        if invitee_credits > 0:
            credits.grant(
                conn, invitee_id, invitee_credits,
                idempotency_key=f"invitee:{invitee_id}",
                kind="reward", ref=inviter_id, memo="受邀奖励",
            )
        conn.execute(
            "UPDATE invite_rewards SET granted_at = now(), credits = %s WHERE invitee_id = %s::uuid",
            (inviter_credits, invitee_id),
        )
        audit(conn, "system", "invite.reward", inviter_id, {
            "invitee": invitee_id, "inviter_credits": inviter_credits,
            "invitee_credits": invitee_credits,
        })
    return {"inviter": inviter_credits, "invitee": invitee_credits}


def status(conn, user_id: str) -> dict:
    """我的邀请码、已邀请人数、已发奖励。"""
    config = settings(conn)
    invited = conn.execute(
        "SELECT count(*) AS n FROM invite_rewards WHERE inviter_id = %s::uuid", (user_id,)
    ).fetchone()["n"]
    granted = conn.execute(
        "SELECT count(*) AS n, COALESCE(SUM(credits), 0) AS total FROM invite_rewards"
        " WHERE inviter_id = %s::uuid AND granted_at IS NOT NULL",
        (user_id,),
    ).fetchone()
    return {
        "code": invite_code_for(user_id),
        "invited": int(invited),
        "rewards": int(granted["n"]),
        "earnedCredits": int(granted["total"] or 0),
        "inviterCredits": config["inviterCredits"],
        "inviteeCredits": config["inviteeCredits"],
        "maxRewards": config["maxRewards"],
    }
