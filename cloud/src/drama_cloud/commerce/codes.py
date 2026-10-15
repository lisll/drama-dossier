"""邮件验证码：60 秒内不能重发，10 分钟过期，最多试 5 次，用一次就作废。

同一邮箱、同一用途只认最新的一条；重发之后，旧验证码自动失效。
"""

from __future__ import annotations

import enum
from typing import Optional, Tuple

from . import security
from .errors import CommerceError, rate_limited

CODE_TTL_SECONDS = 600
RESEND_COOLDOWN_SECONDS = 60
MAX_ATTEMPTS = 5


class Verdict(enum.Enum):
    OK = "ok"
    INVALID = "invalid"
    EXPIRED = "expired"
    EXHAUSTED = "exhausted"


_FAILURES = {
    Verdict.INVALID: ("code_invalid", "验证码不正确"),
    Verdict.EXPIRED: ("code_expired", "验证码已过期，请重新获取"),
    Verdict.EXHAUSTED: ("code_exhausted", "验证码输错次数太多，请重新获取"),
}


def failure(verdict: Verdict) -> CommerceError:
    code, message = _FAILURES[verdict]
    return CommerceError(400, code, message)


def issue(conn, secret: str, email: str, purpose: str) -> Tuple[int, str]:
    """签发新验证码，返回 (行 ID, 明文验证码)；还在冷却期就抛 429。

    用事务级咨询锁按「邮箱 + 用途」串行化：两个并发请求不会都绕过冷却检查。
    """
    code = security.new_code()
    with conn.transaction():
        conn.execute(
            "SELECT pg_advisory_xact_lock(hashtextextended(%s, 0))", (f"code:{purpose}:{email}",)
        )
        latest = conn.execute(
            """
            SELECT ceil(extract(epoch FROM (created_at + make_interval(secs => %s::int) - now())))::int
                       AS wait
            FROM email_codes
            WHERE email = %s AND purpose = %s
            ORDER BY created_at DESC, id DESC
            LIMIT 1
            """,
            (RESEND_COOLDOWN_SECONDS, email, purpose),
        ).fetchone()
        if latest is not None and latest["wait"] > 0:
            raise rate_limited(latest["wait"], "验证码刚发过，请稍后再试")
        row = conn.execute(
            """
            INSERT INTO email_codes (email, purpose, code_digest, expires_at)
            VALUES (%s, %s, %s, now() + make_interval(secs => %s::int))
            RETURNING id
            """,
            (email, purpose, security.code_digest(secret, purpose, email, code), CODE_TTL_SECONDS),
        ).fetchone()
    return row["id"], code


def discard(conn, code_id: int) -> None:
    """邮件没发出去：删掉这条验证码，不占冷却时间，用户可以马上重试。"""
    conn.execute("DELETE FROM email_codes WHERE id = %s", (code_id,))


def check(conn, secret: str, email: str, purpose: str, code: str) -> Tuple[Verdict, Optional[int]]:
    """在调用方的事务里核对验证码；输错会把尝试次数加一。

    返回 INVALID 时，调用方要**提交**事务，不能回滚：一回滚，次数就白加了，
    「最多试 5 次」也就失效了。
    """
    row = conn.execute(
        """
        SELECT id, code_digest, attempts,
               consumed_at IS NOT NULL AS consumed,
               expires_at <= now() AS expired
        FROM email_codes
        WHERE email = %s AND purpose = %s
        ORDER BY created_at DESC, id DESC
        LIMIT 1
        FOR UPDATE
        """,
        (email, purpose),
    ).fetchone()
    if row is None or row["consumed"]:
        return Verdict.INVALID, None
    if row["attempts"] >= MAX_ATTEMPTS:
        return Verdict.EXHAUSTED, None
    if row["expired"]:
        return Verdict.EXPIRED, None
    expected = security.code_digest(secret, purpose, email, code)
    if not security.digests_equal(row["code_digest"], expected):
        conn.execute("UPDATE email_codes SET attempts = attempts + 1 WHERE id = %s", (row["id"],))
        return Verdict.INVALID, None
    return Verdict.OK, row["id"]


def consume(conn, code_id: int) -> None:
    conn.execute("UPDATE email_codes SET consumed_at = now() WHERE id = %s", (code_id,))
