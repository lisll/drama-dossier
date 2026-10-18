"""管理员账号与会话（M7-1）。

与用户账号刻意分开：独立的表、独立的令牌前缀（`adm_`）、独立的会话。
这样即使有人把管理员令牌当用户令牌用，也过不了 users 侧的校验——
「用途隔离」和签名用途前缀是同一个思路。

**没有注册入口**：管理员只能由 CLI 创建（`drama-commerce admin create`）。
网页上开放注册的管理后台，注册入口本身就是攻击面。
"""

from __future__ import annotations

import secrets
import threading
from dataclasses import dataclass
from typing import Optional

from argon2 import PasswordHasher
from argon2.exceptions import InvalidHashError, VerificationError, VerifyMismatchError

from . import security
from .db import audit
from .errors import CommerceError

#: 管理员访问令牌前缀。与用户令牌的 `at_` 区分，用途一眼可辨。
ADMIN_TOKEN_PREFIX = "adm_"
#: 管理员会话有效期：8 小时。比用户短——管理后台能动钱和价目，不该长期挂着。
ADMIN_SESSION_TTL_SECONDS = 8 * 3600


@dataclass(frozen=True)
class AdminContext:
    admin_id: str
    email: str
    session_id: str


def new_admin_token() -> str:
    return ADMIN_TOKEN_PREFIX + secrets.token_urlsafe(32)


def is_admin_token(token: str) -> bool:
    """快速判断一个令牌是不是管理员令牌。

    只用于「肯定不是」的提前拒绝；真正的校验一定要走数据库，
    因为前缀可以伪造。
    """
    return token.startswith(ADMIN_TOKEN_PREFIX)


class AdminService:
    def __init__(self, hasher: Optional[PasswordHasher] = None):
        self.hasher = hasher or PasswordHasher()
        #: 哈希与校验都占名额：超出上限的请求排队，而不是一起把内存吃光。
        self._hash_slots = threading.BoundedSemaphore(4)
        self._dummy_hash = self.hasher.hash("timing-equalizer")

    def _hash(self, password: str) -> str:
        with self._hash_slots:
            return self.hasher.hash(password)

    def _verify(self, stored: str, password: str) -> bool:
        with self._hash_slots:
            try:
                return self.hasher.verify(stored, password)
            except (VerificationError, InvalidHashError):
                return False

    # ---------- 创建与停用（CLI 用） ----------

    def create(self, conn, email: str, password: str, actor: str = "cli") -> str:
        """新建管理员，返回 admin_id。邮箱已存在时报错而不是静默覆盖密码。"""
        normalized = (email or "").strip().lower()
        if not normalized:
            raise CommerceError(422, "invalid_request", "邮箱不能为空")
        if len(password) < 12:
            # 管理员密码比用户密码要求更高：这个账号能动钱与价目。
            raise CommerceError(422, "invalid_request", "管理员密码至少 12 个字符")
        row = conn.execute(
            "INSERT INTO admins (email, password_hash) VALUES (%s, %s)"
            " ON CONFLICT (email) DO NOTHING RETURNING id",
            (normalized, self._hash(password)),
        ).fetchone()
        if row is None:
            raise CommerceError(409, "admin_exists", f"管理员 {normalized} 已存在")
        admin_id = str(row["id"])
        audit(conn, actor, "admin.create", admin_id, {"email_domain": _domain(normalized)})
        return admin_id

    def disable(self, conn, email: str, actor: str = "cli") -> None:
        """停用（不删除）：审计日志会引用管理员 id，删掉就断了线索。"""
        normalized = (email or "").strip().lower()
        row = conn.execute(
            "UPDATE admins SET disabled_at = now()"
            " WHERE email = %s AND disabled_at IS NULL RETURNING id",
            (normalized,),
        ).fetchone()
        if row is None:
            raise CommerceError(404, "admin_not_found", f"没有这个管理员：{normalized}")
        # 停用后立刻踢掉所有会话：不能等 8 小时自然过期。
        conn.execute(
            "UPDATE admin_sessions SET revoked_at = now(), revoke_reason = 'admin_disabled'"
            " WHERE admin_id = %s AND revoked_at IS NULL",
            (row["id"],),
        )
        audit(conn, actor, "admin.disable", str(row["id"]))

    def enable(self, conn, email: str, actor: str = "cli") -> None:
        """恢复被停用的管理员。

        为什么需要它：停用是运维的常规操作（人在休假、临时收回权限），
        但停错人或在人回来之后要恢复。没有这条命令就只能直接改库，
        而直接改库会绕过审计。
        """
        normalized = (email or "").strip().lower()
        row = conn.execute(
            "UPDATE admins SET disabled_at = NULL"
            " WHERE email = %s AND disabled_at IS NOT NULL RETURNING id",
            (normalized,),
        ).fetchone()
        if row is None:
            # 区分「不存在」与「本来就没停用」：两种情况运维要做的事不同。
            exists = conn.execute(
                "SELECT 1 FROM admins WHERE email = %s", (normalized,)
            ).fetchone()
            if exists is None:
                raise CommerceError(404, "admin_not_found", f"没有这个管理员：{normalized}")
            raise CommerceError(409, "admin_not_disabled", f"管理员 {normalized} 没有被停用")
        audit(conn, actor, "admin.enable", str(row["id"]))

    # ---------- 登录与会话 ----------

    def login(self, conn, email: str, password: str) -> dict:
        """邮箱密码登录。邮箱不存在时也做一次同等开销的校验，不让响应快慢泄露有没有这个人。"""
        normalized = (email or "").strip().lower()
        row = conn.execute(
            "SELECT id, email, password_hash, disabled_at FROM admins WHERE email = %s",
            (normalized,),
        ).fetchone()
        stored = row["password_hash"] if row else self._dummy_hash
        verified = self._verify(stored, password)
        # 停用的账号按「密码不对」处理：不告诉攻击者「这个账号存在但被停用了」。
        if row is None or row["disabled_at"] is not None or not verified:
            raise CommerceError(401, "unauthorized", "邮箱或密码不正确")
        admin_id = str(row["id"])
        with conn.transaction():
            session = conn.execute(
                "INSERT INTO admin_sessions (admin_id, expires_at)"
                " VALUES (%s::uuid, now() + make_interval(secs => %s::int)) RETURNING id",
                (admin_id, ADMIN_SESSION_TTL_SECONDS),
            ).fetchone()
            token = new_admin_token()
            conn.execute(
                "INSERT INTO admin_tokens (token_digest, session_id, expires_at)"
                " VALUES (%s, %s::uuid, now() + make_interval(secs => %s::int))",
                (security.token_digest(token), str(session["id"]), ADMIN_SESSION_TTL_SECONDS),
            )
            audit(conn, admin_id, "admin.login", admin_id)
        return {
            "token": token,
            "expires_in": ADMIN_SESSION_TTL_SECONDS,
            "admin": {"id": admin_id, "email": row["email"]},
        }

    def authenticate(self, conn, token: str) -> Optional[AdminContext]:
        """校验管理员令牌。会话被吊销、过期、账号被停用都返回 None。"""
        token = (token or "").strip()
        if not is_admin_token(token):
            # 前缀不对就不必查库；这能挡掉绝大多数误用。
            return None
        row = conn.execute(
            """
            SELECT a.id AS admin_id, a.email, s.id AS session_id
            FROM admin_tokens t
            JOIN admin_sessions s ON s.id = t.session_id
            JOIN admins a ON a.id = s.admin_id
            WHERE t.token_digest = %s AND t.expires_at > now()
              AND s.revoked_at IS NULL AND s.expires_at > now()
              AND a.disabled_at IS NULL
            """,
            (security.token_digest(token),),
        ).fetchone()
        if row is None:
            return None
        return AdminContext(
            admin_id=str(row["admin_id"]),
            email=row["email"],
            session_id=str(row["session_id"]),
        )

    def logout(self, conn, ctx: AdminContext) -> None:
        with conn.transaction():
            conn.execute(
                "UPDATE admin_sessions SET revoked_at = now(), revoke_reason = 'logout'"
                " WHERE id = %s::uuid",
                (ctx.session_id,),
            )
            audit(conn, ctx.admin_id, "admin.logout", ctx.session_id)


def _domain(email: str) -> str:
    """审计里只记邮箱域名：管理员邮箱本身是个人信息，不该进日志明细。"""
    _, _, domain = email.partition("@")
    return domain
