"""设备绑定与授权凭证。

- 名额：每个账号同时绑定 device_limit 台电脑（默认 1 台）。退出登录不归还名额，要换电脑得先解绑。
- 解绑（踢设备）：每 unbind_window_days 天最多 unbind_limit 次（默认 30 天 3 次），
  防止一个账号轮流给多台电脑用。被踢的电脑：绑定作废，它最近一次使用的登录会话一并吊销。
- 授权凭证：签名的短期凭证，绑定账号和机器码，有效期 license_ttl_hours（默认 72 小时）。
  断网后凭证到期前，桌面端还能用本地功能；联网时每 license_refresh_hours 刷新一次。
"""

from __future__ import annotations

import math
from datetime import datetime, timezone
from typing import Optional

from . import entitlements, sessions
from .config import CommerceSettings
from .db import audit, lock_user
from .errors import CommerceError
from .license_token import PAYLOAD_VERSION, Signer
from .sessions import AuthContext

#: 设备查询的列。这是模块常量，不含任何外部输入，拼进 SQL 是安全的。
_DEVICE_COLUMNS = "id, device_id, name, platform, app_version, bound_at, last_seen_at"


class LicenseService:
    def __init__(self, settings: CommerceSettings, signer: Signer):
        self.settings = settings
        self.signer = signer

    def activate(self, conn, ctx: AuthContext, device_id: str, name: str, platform: str,
                 app_version: str) -> dict:
        """绑定这台电脑并签发授权凭证。已经绑定过的电脑再调用一次不占新名额，只刷新信息。"""
        with conn.transaction():
            lock_user(conn, ctx.user_id)
            row = conn.execute(
                f"""
                UPDATE devices
                SET name = %s, platform = %s, app_version = %s, session_id = %s::uuid, last_seen_at = now()
                WHERE user_id = %s::uuid AND device_id = %s AND unbound_at IS NULL
                RETURNING {_DEVICE_COLUMNS}
                """,
                (name, platform, app_version, ctx.session_id, ctx.user_id, device_id),
            ).fetchone()
            if row is None:
                bound = self._bound(conn, ctx.user_id)
                if len(bound) >= self.settings.device_limit:
                    raise CommerceError(
                        409,
                        "device_limit",
                        f"这个账号最多同时在 {self.settings.device_limit} 台电脑上使用，"
                        "请先解绑一台已绑定的电脑，再绑定这台",
                        extra={
                            "devices": [_device_view(item, device_id) for item in bound],
                            "device_limit": self.settings.device_limit,
                            "unbind_quota": self._quota(conn, ctx.user_id)[0],
                        },
                    )
                row = conn.execute(
                    f"""
                    INSERT INTO devices (user_id, device_id, name, platform, app_version, session_id)
                    VALUES (%s::uuid, %s, %s, %s, %s, %s::uuid)
                    RETURNING {_DEVICE_COLUMNS}
                    """,
                    (ctx.user_id, device_id, name, platform, app_version, ctx.session_id),
                ).fetchone()
                audit(conn, ctx.user_id, "device.bind", str(row["id"]), {"platform": platform})
            return self._issue(conn, ctx.user_id, row, device_id)

    def refresh(self, conn, ctx: AuthContext, device_id: str, app_version: Optional[str]) -> dict:
        """给已绑定的电脑换一张新凭证，同时带回最新的使用权（兑换卡密后刷新一次就生效）。"""
        with conn.transaction():
            row = conn.execute(
                f"""
                UPDATE devices
                SET last_seen_at = now(), session_id = %s::uuid, app_version = coalesce(%s::text, app_version)
                WHERE user_id = %s::uuid AND device_id = %s AND unbound_at IS NULL
                RETURNING {_DEVICE_COLUMNS}
                """,
                (ctx.session_id, app_version, ctx.user_id, device_id),
            ).fetchone()
            if row is None:
                raise CommerceError(403, "device_not_bound", "这台电脑没有绑定到当前账号，或者已经被解绑，请重新绑定")
            return self._issue(conn, ctx.user_id, row, device_id)

    def list_devices(self, conn, ctx: AuthContext, current_device: Optional[str]) -> dict:
        return {
            "devices": [_device_view(item, current_device) for item in self._bound(conn, ctx.user_id)],
            "device_limit": self.settings.device_limit,
            "unbind_quota": self._quota(conn, ctx.user_id)[0],
        }

    def unbind(self, conn, ctx: AuthContext, binding_id: str) -> None:
        """解绑（踢掉）一台电脑：绑定作废，那台电脑最近一次用的登录会话一并吊销。

        踢的如果正是发起请求的这台，当前登录也随之失效。
        """
        with conn.transaction():
            lock_user(conn, ctx.user_id)
            row = conn.execute(
                "SELECT id, session_id FROM devices"
                " WHERE id = %s::uuid AND user_id = %s::uuid AND unbound_at IS NULL",
                (binding_id, ctx.user_id),
            ).fetchone()
            if row is None:
                raise CommerceError(404, "device_not_found", "没有找到这台电脑，可能已经解绑了")
            quota, wait_seconds = self._quota(conn, ctx.user_id)
            if quota["remaining"] == 0:
                raise CommerceError(
                    429,
                    "unbind_limit",
                    f"每 {quota['window_days']} 天最多解绑 {quota['limit']} 次，这个周期的次数已经用完了",
                    retry_after=wait_seconds,
                    extra={"unbind_quota": quota},
                )
            conn.execute(
                "UPDATE devices SET unbound_at = now(), unbind_reason = 'user' WHERE id = %s::uuid",
                (binding_id,),
            )
            if row["session_id"] is not None:
                sessions.revoke(conn, str(row["session_id"]), "device_unbound")
            audit(conn, ctx.user_id, "device.unbind", binding_id, {"remaining": quota["remaining"] - 1})

    def _bound(self, conn, user_id: str) -> list:
        return conn.execute(
            f"SELECT {_DEVICE_COLUMNS} FROM devices"
            " WHERE user_id = %s::uuid AND unbound_at IS NULL ORDER BY bound_at",
            (user_id,),
        ).fetchall()

    def _quota(self, conn, user_id: str):
        """本周期还能解绑几次；次数用完时，另外返回还要等多少秒才能再解绑。"""
        limit, days = self.settings.unbind_limit, self.settings.unbind_window_days
        rows = conn.execute(
            """
            SELECT extract(epoch FROM unbound_at + %s * interval '1 day' - now()) AS frees_in
            FROM devices
            WHERE user_id = %s::uuid AND unbind_reason = 'user' AND unbound_at > now() - %s * interval '1 day'
            ORDER BY unbound_at
            """,
            (days, user_id, days),
        ).fetchall()
        used = len(rows)
        remaining = max(0, limit - used)
        wait_seconds = None
        if remaining == 0:
            # 最早的几次过了周期，次数才会回来：要等到用掉的次数降到 limit - 1 以下。
            wait_seconds = max(1, math.ceil(float(rows[used - limit]["frees_in"])))
        return {"limit": limit, "window_days": days, "remaining": remaining}, wait_seconds

    def _issue(self, conn, user_id: str, device_row, device_id: str) -> dict:
        entitlement = entitlements.current(conn, user_id)
        now = conn.execute("SELECT floor(extract(epoch FROM now()))::bigint AS now").fetchone()["now"]
        expires = now + self.settings.license_ttl_hours * 3600
        token = self.signer.sign({
            "v": PAYLOAD_VERSION,
            "kid": self.signer.kid,
            "sub": user_id,
            "dev": device_id,
            "ent": entitlement.kind,
            # 按天计的使用权可能在凭证有效期内到期，桌面端要拿 ent_until 和当前时间比。
            "ent_until": int(entitlement.expires_at.timestamp()) if entitlement.kind == "timed" else None,
            "iat": now,
            "exp": expires,
        })
        return {
            "license": {
                "token": token,
                "kid": self.signer.kid,
                "issued_at": _iso(now),
                "expires_at": _iso(expires),
                "refresh_after": _iso(now + self.settings.license_refresh_hours * 3600),
            },
            "device": _device_view(device_row, device_id),
            "entitlement": entitlement.view(),
        }


def _iso(epoch: int) -> str:
    return datetime.fromtimestamp(epoch, tz=timezone.utc).isoformat()


def device_code(device_id: str) -> str:
    """给人看的机器码：机器码摘要的前 16 位，转大写、4 位一组。联系发卡方时报这个。"""
    code = device_id[:16].upper()
    return "-".join(code[index:index + 4] for index in range(0, 16, 4))


def _device_view(row, current_device: Optional[str]) -> dict:
    return {
        "id": str(row["id"]),
        "code": device_code(row["device_id"]),
        "name": row["name"],
        "platform": row["platform"],
        "app_version": row["app_version"],
        "bound_at": row["bound_at"].isoformat(),
        "last_seen_at": row["last_seen_at"].isoformat(),
        "current": row["device_id"] == current_device,
    }
