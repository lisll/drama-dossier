"""会话与令牌：访问令牌 15 分钟有效；刷新令牌 30 天有效，只能用一次。

每次刷新都换一对新令牌（轮换）。旧刷新令牌被再次使用，说明它可能被偷了，
也可能是客户端并发刷新——两种情况都直接吊销整个会话，让用户重新登录。
所以桌面端必须保证同一时刻只发一个刷新请求。
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Optional

from . import security
from .db import audit
from .errors import unauthorized

ACCESS_TTL_SECONDS = 15 * 60
REFRESH_TTL_SECONDS = 30 * 24 * 3600


@dataclass(frozen=True)
class TokenPair:
    access_token: str
    refresh_token: str

    def payload(self) -> dict:
        return {
            "access_token": self.access_token,
            "refresh_token": self.refresh_token,
            "token_type": "Bearer",
            "expires_in": ACCESS_TTL_SECONDS,
            "refresh_expires_in": REFRESH_TTL_SECONDS,
        }


@dataclass(frozen=True)
class AuthContext:
    user_id: str
    session_id: str


def _issue(conn, session_id) -> TokenPair:
    access = security.new_access_token()
    refresh = security.new_refresh_token()
    conn.execute(
        "INSERT INTO access_tokens (token_digest, session_id, expires_at)"
        " VALUES (%s, %s::uuid, now() + make_interval(secs => %s::int))",
        (security.token_digest(access), str(session_id), ACCESS_TTL_SECONDS),
    )
    conn.execute(
        "INSERT INTO refresh_tokens (token_digest, session_id, expires_at)"
        " VALUES (%s, %s::uuid, now() + make_interval(secs => %s::int))",
        (security.token_digest(refresh), str(session_id), REFRESH_TTL_SECONDS),
    )
    return TokenPair(access, refresh)


def create(conn, user_id) -> TokenPair:
    """新建会话并签发第一对令牌。在调用方的事务里执行。"""
    row = conn.execute(
        "INSERT INTO sessions (user_id, expires_at)"
        " VALUES (%s::uuid, now() + make_interval(secs => %s::int)) RETURNING id",
        (str(user_id), REFRESH_TTL_SECONDS),
    ).fetchone()
    return _issue(conn, row["id"])


def rotate(conn, refresh_token: str) -> TokenPair:
    """用刷新令牌换一对新令牌，同时把会话有效期往后顺延。发现重放就吊销会话，再报 401。"""
    digest = security.token_digest(refresh_token)
    reused = False
    with conn.transaction():
        row = conn.execute(
            """
            SELECT r.session_id, s.user_id, r.used_at IS NOT NULL AS used,
                   (r.expires_at <= now() OR s.expires_at <= now() OR s.revoked_at IS NOT NULL)
                       AS dead
            FROM refresh_tokens r JOIN sessions s ON s.id = r.session_id
            WHERE r.token_digest = %s
            FOR UPDATE OF r, s
            """,
            (digest,),
        ).fetchone()
        if row is None or row["dead"]:
            raise unauthorized()
        if row["used"]:
            # 先把吊销提交了，再报错：不能让一个被偷的令牌把会话留着。
            conn.execute(
                "UPDATE sessions SET revoked_at = now(), revoke_reason = 'refresh_reuse'"
                " WHERE id = %s",
                (row["session_id"],),
            )
            audit(conn, str(row["user_id"]), "session.refresh_reuse", str(row["session_id"]))
            reused = True
        else:
            conn.execute("UPDATE refresh_tokens SET used_at = now() WHERE token_digest = %s", (digest,))
            conn.execute(
                "UPDATE sessions SET expires_at = now() + make_interval(secs => %s::int) WHERE id = %s",
                (REFRESH_TTL_SECONDS, row["session_id"]),
            )
            pair = _issue(conn, row["session_id"])
    if reused:
        raise unauthorized("登录凭证被重复使用，为了安全已退出登录，请重新登录")
    return pair


def authenticate(conn, access_token: str) -> Optional[AuthContext]:
    row = conn.execute(
        """
        SELECT s.user_id, s.id AS session_id
        FROM access_tokens a JOIN sessions s ON s.id = a.session_id
        WHERE a.token_digest = %s AND a.expires_at > now()
          AND s.revoked_at IS NULL AND s.expires_at > now()
        """,
        (security.token_digest(access_token),),
    ).fetchone()
    if row is None:
        return None
    return AuthContext(str(row["user_id"]), str(row["session_id"]))


def revoke(conn, session_id: str, reason: str) -> None:
    conn.execute(
        "UPDATE sessions SET revoked_at = now(), revoke_reason = %s"
        " WHERE id = %s::uuid AND revoked_at IS NULL",
        (reason, session_id),
    )


def revoke_all(conn, user_id: str, reason: str, keep_session: Optional[str] = None) -> int:
    """吊销该用户的全部会话，keep_session 指定的那个除外（修改密码时保留当前会话）。"""
    cursor = conn.execute(
        """
        UPDATE sessions SET revoked_at = now(), revoke_reason = %s
        WHERE user_id = %s::uuid AND revoked_at IS NULL
          AND (%s::uuid IS NULL OR id <> %s::uuid)
        """,
        (reason, user_id, keep_session, keep_session),
    )
    return cursor.rowcount
