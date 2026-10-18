"""身份认证与角色授权（M5b 起改为「内省 commerce 令牌」）。

**为什么改**：M5b 之前，媒体控制面自己有 `users` 表与不透明令牌（CLI 用 `initdb --admin`
建出来）。但桌面端用户是**邮箱注册在 commerce 里**的，在媒体控制面根本没有身份——
那样「服务端处理」就无从知道「这是谁的任务」「扣谁的积分」。

现在只有一处登录：桌面端登录 commerce，拿到的访问令牌同时用于媒体控制面。
媒体控制面拿这个令牌去问 commerce 的内部接口（`/internal/introspect`）换出
`user_id`，再用这个 id 做任务归属与积分扣费。好处是两边的用户 id 天然一致。

角色：commerce 用户默认 `operator`（能传素材、建任务、看自己的东西）；
`admin` 由本库的 `media_admins` 白名单决定（把某个 commerce 邮箱加进去）。

**保留 `users` 表**：老的 `create_user`/`resolve_token` 不再用于认证，
但表结构与历史数据保留（迁移兼容、审计里还能查到当时的 actor）。
"""

from __future__ import annotations

import hashlib
import secrets
from dataclasses import dataclass
from typing import Optional

from .db import audit, now_iso
from .errors import WorkError, ErrorClass

ROLE_RANK = {"viewer": 1, "operator": 2, "admin": 3}


@dataclass(frozen=True)
class Principal:
    user_id: str
    name: str
    role: str

    def require(self, minimum: str) -> None:
        if ROLE_RANK.get(self.role, 0) < ROLE_RANK[minimum]:
            raise WorkError(
                ErrorClass.PARAM_OUT_OF_RANGE,
                f"需要 {minimum} 及以上权限，当前角色为 {self.role}",
            )


def hash_token(token: str) -> str:
    return hashlib.sha256(token.encode("utf-8")).hexdigest()


def create_user(conn, name: str, role: str = "operator", actor: str = "system") -> str:
    """**已废弃**：M5b 起认证走 commerce 内省，这里只保留给历史数据与测试用。"""
    if role not in ROLE_RANK:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"未知角色：{role}")
    user_id = "usr_" + secrets.token_hex(6)
    token = secrets.token_urlsafe(32)
    conn.execute(
        "INSERT INTO users (id, name, role, token_hash, created_at) VALUES (%s,%s,%s,%s,%s)",
        (user_id, name, role, hash_token(token), now_iso()),
    )
    audit(conn, actor, "user.create", user_id, {"name": name, "role": role})
    return token


def resolve_token(conn, token: str):
    """**已废弃**：只在迁移期保留，新代码不要用。见 `principal_for_email`。"""
    row = conn.execute(
        "SELECT id, name, role FROM users WHERE token_hash = %s", (hash_token(token),)
    ).fetchone()
    if row is None:
        return None
    return Principal(row["id"], row["name"], row["role"])


def admin_emails(conn) -> set:
    """管理员白名单里的邮箱（小写）。"""
    return {row["email"] for row in conn.execute("SELECT email FROM media_admins")}


def viewer_emails(conn) -> set:
    """只读白名单里的邮箱（小写）。"""
    return {row["email"] for row in conn.execute("SELECT email FROM media_viewers")}


def role_for_email(conn, email: str) -> str:
    """按白名单定角色：admin > viewer > operator（默认）。

    默认给 operator 而不是 viewer：能传素材、建任务、看自己的东西，
    这是付费用户该有的能力；只读档留给明确要限制的账号。
    """
    normalized = (email or "").strip().lower()
    if not normalized:
        return "operator"
    if normalized in admin_emails(conn):
        return "admin"
    if normalized in viewer_emails(conn):
        return "viewer"
    return "operator"


def principal_for_email(conn, user_id: str, email: str) -> Principal:
    """由 commerce 内省结果构造 Principal，并确保本地有对应的影子用户行。

    为什么要有影子行：`projects.owner_id`、`assets`、`audit_events.actor` 等
    到处是 `REFERENCES users(id)`。身份来自 commerce 之后，如果本地没有这一行，
    外键会直接拦住建项目的操作。

    做法：本地 `users` 表保留，但它现在只是**commerce 身份的镜像**：
    `id` 用 commerce 的 user_id，`name` 用邮箱，`role` 跟着白名单走。
    影子行不含任何凭据（`token_hash` 填一个不可能被用到的占位值），
    所以它不能用来登录——登录只认 commerce。

    角色只看白名单（`media_admins` / `media_viewers`，默认 operator）。
    **不看 commerce 那边的任何角色字段**——两边角色体系不同，混用会出现
    「commerce 的普通用户在这里也是管理员」这种提权。
    """
    normalized = (email or "").strip().lower()
    role = role_for_email(conn, normalized)
    # 影子行的 token_hash 必须唯一，但绝不能被任何真实令牌命中：
    # 用一个带前缀的派生值，`resolve_token` 走的是 SHA-256 摘要，
    # 不会等于这个明文前缀串。
    shadow_hash = f"commerce:{user_id}"
    conn.execute(
        "INSERT INTO users (id, name, role, token_hash, created_at) VALUES (%s,%s,%s,%s,%s)"
        " ON CONFLICT (id) DO UPDATE SET name = excluded.name, role = excluded.role",
        (user_id, normalized or user_id, role, shadow_hash, now_iso()),
    )
    return Principal(user_id=user_id, name=normalized or user_id, role=role)


def add_admin(conn, email: str, actor: str = "cli") -> None:
    """把某个邮箱加进管理员白名单。重复添加不报错（幂等）。"""
    normalized = (email or "").strip().lower()
    if not normalized:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "邮箱不能为空")
    conn.execute(
        "INSERT INTO media_admins (email, created_at) VALUES (%s,%s)"
        " ON CONFLICT (email) DO NOTHING",
        (normalized, now_iso()),
    )
    audit(conn, actor, "media_admin.add", normalized)
