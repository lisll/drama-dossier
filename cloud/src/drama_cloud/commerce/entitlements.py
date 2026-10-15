"""使用权：账号能不能用付费功能、用到什么时候。

kind 取 none（没有）/ timed（按天计，有到期时间）/ lifetime（永久）。
到期判断一律用数据库时间 now()，不用应用服务器的时钟。
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime
from typing import Optional

from .errors import CommerceError


@dataclass(frozen=True)
class Entitlement:
    kind: str
    expires_at: Optional[datetime]
    active: bool

    def view(self) -> dict:
        return {
            "kind": self.kind,
            "active": self.active,
            "expires_at": self.expires_at.isoformat() if self.expires_at else None,
        }


def already_lifetime() -> CommerceError:
    return CommerceError(409, "already_lifetime", "这个账号已经是永久授权，不需要再兑换卡密")


def current(conn, user_id: str) -> Entitlement:
    row = conn.execute(
        """
        SELECT kind, expires_at, (kind = 'lifetime' OR expires_at > now()) AS active
        FROM entitlements WHERE user_id = %s::uuid
        """,
        (user_id,),
    ).fetchone()
    if row is None:
        return Entitlement("none", None, False)
    return Entitlement(row["kind"], row["expires_at"], row["active"])


def grant(conn, user_id: str, days: Optional[int]) -> None:
    """加使用权。days 为 None 表示永久；否则从「当前到期时间」和「现在」中较晚的那个往后顺延。

    已经是永久的账号不能再加（抛 already_lifetime）：永久的到期时间是 NULL，
    这里若照常更新，greatest() 会忽略 NULL，把永久悄悄降成按天计。
    调用方要在同一个事务里先锁住用户行（db.lock_user）。
    """
    if days is None:
        cursor = conn.execute(
            """
            INSERT INTO entitlements (user_id, kind, expires_at) VALUES (%s::uuid, 'lifetime', NULL)
            ON CONFLICT (user_id) DO UPDATE SET kind = 'lifetime', expires_at = NULL, updated_at = now()
            WHERE entitlements.kind <> 'lifetime'
            """,
            (user_id,),
        )
    else:
        cursor = conn.execute(
            """
            INSERT INTO entitlements (user_id, kind, expires_at)
            VALUES (%s::uuid, 'timed', now() + %s * interval '1 day')
            ON CONFLICT (user_id) DO UPDATE
            SET expires_at = greatest(entitlements.expires_at, now()) + %s * interval '1 day',
                updated_at = now()
            WHERE entitlements.kind <> 'lifetime'
            """,
            (user_id, days, days),
        )
    if cursor.rowcount != 1:
        raise already_lifetime()
