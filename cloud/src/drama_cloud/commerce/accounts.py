"""账号业务：发验证码、注册、登录、刷新令牌、退出、重设密码、修改密码。

时间判断一律用数据库的 now()，不依赖应用服务器的时钟。
"""

from __future__ import annotations

import logging
import threading
from typing import Optional, Tuple

from argon2 import PasswordHasher
from argon2.exceptions import InvalidHashError, VerificationError, VerifyMismatchError
from psycopg import errors as pg_errors

from . import codes, sessions
from .config import CommerceSettings
from .db import audit
from .errors import CommerceError, unauthorized
from .mailer import MailError, Mailer
from .sessions import AuthContext

log = logging.getLogger(__name__)

_PURPOSE_TEXT = {"register": "注册账号", "reset_password": "重设密码"}
_PURGE = {
    "email_codes": "DELETE FROM email_codes WHERE expires_at < now() - interval '1 day'",
    "access_tokens": "DELETE FROM access_tokens WHERE expires_at < now()",
    "refresh_tokens": "DELETE FROM refresh_tokens WHERE expires_at < now()",
    "sessions": (
        "DELETE FROM sessions WHERE expires_at < now() - interval '30 days'"
        " OR (revoked_at IS NOT NULL AND revoked_at < now() - interval '30 days')"
    ),
    "rate_limits": "DELETE FROM rate_limits WHERE window_start < now() - interval '1 day'",
}


def _email_taken() -> CommerceError:
    return CommerceError(409, "email_taken", "该邮箱已注册，请直接登录")


def _invalid_login() -> CommerceError:
    # 邮箱不存在和密码错误报同一个错，不提示具体错在哪。
    return CommerceError(401, "invalid_credentials", "邮箱或密码不正确")


def _user_view(row) -> dict:
    return {"id": str(row["id"]), "email": row["email"], "created_at": row["created_at"].isoformat()}


class AccountService:
    def __init__(self, settings: CommerceSettings, mailer: Mailer, hasher: Optional[PasswordHasher] = None):
        self.settings = settings
        self.mailer = mailer
        #: argon2id；默认参数是 argon2-cffi 采用的 RFC 9106 低内存档。
        self.hasher = hasher or PasswordHasher()
        # 哈希和校验都要占用这里的名额：超出上限的请求排队等，而不是一起把内存吃光。
        self._hash_slots = threading.BoundedSemaphore(settings.hash_concurrency)
        # 邮箱不存在时，拿它做一次同等开销的校验，不让响应快慢暴露邮箱有没有注册。
        self._dummy_hash = self.hasher.hash("timing-equalizer")

    # ---------- 验证码 ----------

    def send_code(self, conn, email: str, purpose: str) -> dict:
        exists = conn.execute("SELECT 1 FROM users WHERE email = %s", (email,)).fetchone() is not None
        if purpose == "register" and exists:
            raise _email_taken()
        if purpose == "reset_password" and not exists:
            raise CommerceError(404, "email_not_registered", "该邮箱还没有注册")
        code_id, code = codes.issue(conn, self.settings.code_secret, email, purpose)
        subject, body = self._compose(purpose, code)
        try:
            self.mailer.send(email, subject, body)
        except MailError as error:
            codes.discard(conn, code_id)
            raise CommerceError(503, "mail_unavailable", "验证码邮件发送失败，请稍后重试") from error
        return {"cooldown_seconds": codes.RESEND_COOLDOWN_SECONDS, "expires_in": codes.CODE_TTL_SECONDS}

    def _compose(self, purpose: str, code: str) -> Tuple[str, str]:
        action = _PURPOSE_TEXT[purpose]
        subject = f"【{self.settings.product_name}】{action}验证码"
        body = (
            f"你正在{action}，验证码是：{code}\n\n"
            f"验证码 {codes.CODE_TTL_SECONDS // 60} 分钟内有效，只能使用一次。\n"
            "如果不是你本人操作，请忽略这封邮件，也不要把验证码告诉任何人。\n"
        )
        return subject, body

    # ---------- 注册与登录 ----------

    def register(self, conn, email: str, code: str, password: str) -> dict:
        if conn.execute("SELECT 1 FROM users WHERE email = %s", (email,)).fetchone():
            raise _email_taken()
        try:
            # 验证码输错时也要提交事务（把尝试次数记下来），所以错误留到事务外面再抛。
            with conn.transaction():
                verdict, code_id = codes.check(conn, self.settings.code_secret, email, "register", code)
                if verdict is codes.Verdict.OK:
                    codes.consume(conn, code_id)
                    user = conn.execute(
                        """
                        INSERT INTO users (email, password_hash, terms_version, terms_accepted_at)
                        VALUES (%s, %s, %s, now())
                        RETURNING id, email, created_at
                        """,
                        (email, self._hash(password), self.settings.terms_version),
                    ).fetchone()
                    pair = sessions.create(conn, user["id"])
                    audit(conn, str(user["id"]), "user.register", str(user["id"]),
                          {"terms_version": self.settings.terms_version})
        except pg_errors.UniqueViolation as error:
            # 两个请求同时注册同一个邮箱：后插入的那个在这里被唯一约束挡住，整个事务回滚。
            raise _email_taken() from error
        if verdict is not codes.Verdict.OK:
            raise codes.failure(verdict)
        return {"user": _user_view(user), "tokens": pair.payload()}

    def login(self, conn, email: str, password: str) -> dict:
        user = conn.execute(
            "SELECT id, email, password_hash, created_at FROM users WHERE email = %s", (email,)
        ).fetchone()
        verified = self._verify(user["password_hash"] if user else self._dummy_hash, password)
        if user is None or not verified:
            raise _invalid_login()
        with conn.transaction():
            if self.hasher.check_needs_rehash(user["password_hash"]):
                # 哈希参数调高以后，用户下次登录时顺手升级成新参数。
                conn.execute(
                    "UPDATE users SET password_hash = %s WHERE id = %s",
                    (self._hash(password), user["id"]),
                )
            pair = sessions.create(conn, user["id"])
            audit(conn, str(user["id"]), "user.login", str(user["id"]))
        return {"user": _user_view(user), "tokens": pair.payload()}

    def _hash(self, password: str) -> str:
        with self._hash_slots:
            return self.hasher.hash(password)

    def _verify(self, password_hash: str, password: str) -> bool:
        try:
            with self._hash_slots:
                return self.hasher.verify(password_hash, password)
        except VerifyMismatchError:
            return False
        except (VerificationError, InvalidHashError):
            log.error("数据库里的密码哈希无法解析，按登录失败处理")
            return False

    # ---------- 令牌 ----------

    def refresh(self, conn, refresh_token: str) -> dict:
        return {"tokens": sessions.rotate(conn, refresh_token).payload()}

    def authenticate(self, conn, access_token: str) -> Optional[AuthContext]:
        return sessions.authenticate(conn, access_token)

    def logout(self, conn, ctx: AuthContext) -> None:
        with conn.transaction():
            sessions.revoke(conn, ctx.session_id, "logout")
            audit(conn, ctx.user_id, "user.logout", ctx.session_id)

    def me(self, conn, ctx: AuthContext) -> dict:
        row = conn.execute(
            "SELECT id, email, created_at FROM users WHERE id = %s::uuid", (ctx.user_id,)
        ).fetchone()
        if row is None:
            raise unauthorized()
        return {"user": _user_view(row)}

    # ---------- 密码 ----------

    def reset_password(self, conn, email: str, code: str, new_password: str) -> None:
        """用邮件验证码重设密码，并吊销这个账号的全部会话（所有设备都要重新登录）。"""
        user = None
        with conn.transaction():
            verdict, code_id = codes.check(conn, self.settings.code_secret, email, "reset_password", code)
            if verdict is codes.Verdict.OK:
                user = conn.execute(
                    "SELECT id FROM users WHERE email = %s FOR UPDATE", (email,)
                ).fetchone()
            if user is not None:
                codes.consume(conn, code_id)
                conn.execute(
                    "UPDATE users SET password_hash = %s WHERE id = %s",
                    (self._hash(new_password), user["id"]),
                )
                revoked = sessions.revoke_all(conn, str(user["id"]), "password_reset")
                audit(conn, str(user["id"]), "user.password_reset", str(user["id"]),
                      {"revoked_sessions": revoked})
        if verdict is not codes.Verdict.OK:
            raise codes.failure(verdict)
        if user is None:
            # 验证码对，但账号在这期间没了：按验证码无效处理。
            raise codes.failure(codes.Verdict.INVALID)

    def change_password(self, conn, ctx: AuthContext, current_password: str, new_password: str) -> None:
        """登录状态下修改密码：保留当前会话，吊销其他所有会话。"""
        row = conn.execute(
            "SELECT password_hash FROM users WHERE id = %s::uuid", (ctx.user_id,)
        ).fetchone()
        if row is None:
            raise unauthorized()
        if not self._verify(row["password_hash"], current_password):
            raise CommerceError(400, "wrong_password", "当前密码不正确")
        with conn.transaction():
            conn.execute(
                "UPDATE users SET password_hash = %s WHERE id = %s::uuid",
                (self._hash(new_password), ctx.user_id),
            )
            revoked = sessions.revoke_all(conn, ctx.user_id, "password_changed", keep_session=ctx.session_id)
            audit(conn, ctx.user_id, "user.password_change", ctx.user_id, {"revoked_sessions": revoked})


def purge_expired(conn) -> dict:
    """清理过期的验证码、令牌、会话和限流计数，建议每天跑一次。审计日志不在这里清理。"""
    counts = {}
    with conn.transaction():
        for table, statement in _PURGE.items():
            counts[table] = conn.execute(statement).rowcount
    return counts
