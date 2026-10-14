"""身份认证与角色授权。

设计文档要求「管理接口实施认证、角色授权和速率限制」与「长效密钥不应硬编码」。
学习原型用**数据库里的不透明令牌**（只存 SHA-256），角色分 admin / operator / viewer。
生产环境应替换为 OAuth/OIDC；接口形状（`Authorization: Bearer <token>`）保持不变。
"""

from __future__ import annotations

import hashlib
import secrets
from dataclasses import dataclass

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
    """新建用户并返回**一次性可见**的明文令牌。"""
    if role not in ROLE_RANK:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"未知角色：{role}")
    user_id = "usr_" + secrets.token_hex(6)
    token = secrets.token_urlsafe(32)
    conn.execute(
        "INSERT INTO users (id, name, role, token_hash, created_at) VALUES (?,?,?,?,?)",
        (user_id, name, role, hash_token(token), now_iso()),
    )
    audit(conn, actor, "user.create", user_id, {"name": name, "role": role})
    return token


def resolve_token(conn, token: str):
    row = conn.execute(
        "SELECT id, name, role FROM users WHERE token_hash = ?", (hash_token(token),)
    ).fetchone()
    if row is None:
        return None
    return Principal(row["id"], row["name"], row["role"])
