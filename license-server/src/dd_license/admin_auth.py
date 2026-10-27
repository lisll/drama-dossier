"""管理员认证：密码哈希、强度校验、会话、登录锁定。

只用标准库（hashlib.scrypt / secrets / hmac），不额外引入依赖。
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import secrets
import sqlite3
from dataclasses import dataclass

from .db import Database, audit, now
from .errors import LicenseError

SCRYPT_N, SCRYPT_R, SCRYPT_P = 2**14, 8, 1
MIN_PASSWORD_LEN = 16
SESSION_TTL = 8 * 3600
SESSION_IDLE = 30 * 60
LOCK_AFTER = 5
LOCK_SECONDS = 15 * 60
USERNAME_RE_CHARS = set("abcdefghijklmnopqrstuvwxyz0123456789_.-")

_WEAK = {
    "password", "passw0rd", "12345678", "123456789", "1234567890", "admin123", "administrator",
    "qwertyuiop", "qwerty123", "1qaz2wsx", "letmein", "iloveyou", "11111111", "00000000",
    "abc123456", "a123456789", "woaini1314",
}
_KEYBOARD_ROWS = ("1234567890", "qwertyuiop", "asdfghjkl", "zxcvbnm", "abcdefghijklmnopqrstuvwxyz")


def _b64(b: bytes) -> str:
    return base64.b64encode(b).decode("ascii")


def hash_password(password: str) -> str:
    salt = secrets.token_bytes(16)
    dk = hashlib.scrypt(password.encode("utf-8"), salt=salt, n=SCRYPT_N, r=SCRYPT_R, p=SCRYPT_P, dklen=32)
    return f"scrypt${SCRYPT_N}${SCRYPT_R}${SCRYPT_P}${_b64(salt)}${_b64(dk)}"


def verify_password(password: str, stored: str) -> bool:
    try:
        algo, n, r, p, salt, dk = stored.split("$")
        if algo != "scrypt":
            return False
        expect = base64.b64decode(dk)
        got = hashlib.scrypt(
            password.encode("utf-8"), salt=base64.b64decode(salt), n=int(n), r=int(r), p=int(p), dklen=len(expect)
        )
    except (ValueError, TypeError):
        return False
    return hmac.compare_digest(got, expect)


# 用户名不存在时也算一次，让响应时间无法用来探测账号是否存在。
_DUMMY_HASH = hash_password(secrets.token_urlsafe(24))


def check_username(username: str) -> str:
    u = (username or "").strip().lower()
    if not 3 <= len(u) <= 32 or not set(u) <= USERNAME_RE_CHARS:
        raise LicenseError("bad_username", "用户名需为 3–32 位小写字母、数字或 _ . -", 422)
    return u


def check_password_strength(password: str, username: str = "") -> None:
    if not isinstance(password, str) or len(password) < MIN_PASSWORD_LEN:
        raise LicenseError("weak_password", f"密码至少 {MIN_PASSWORD_LEN} 位", 422)
    if len(password) > 256:
        raise LicenseError("weak_password", "密码过长", 422)
    low = password.lower()
    if username and username.lower() in low:
        raise LicenseError("weak_password", "密码不能包含用户名", 422)
    if low in _WEAK or any(w in low for w in _WEAK if len(w) >= 8):
        raise LicenseError("weak_password", "密码过于常见，请换一个", 422)
    if len(set(password)) < 6:
        raise LicenseError("weak_password", "密码重复字符过多", 422)
    for row in _KEYBOARD_ROWS:
        for i in range(len(row) - 7):
            seg = row[i : i + 8]
            if seg in low or seg[::-1] in low:
                raise LicenseError("weak_password", "密码不能包含连续键盘序列", 422)


def _token_digest(token: str) -> str:
    # 会话令牌由 secrets.token_urlsafe 生成，必为 ASCII；调用方已用 _plausible_token 过滤
    return hashlib.sha256(token.encode("ascii")).hexdigest()


def _plausible_token(token: str | None) -> bool:
    """浏览器发来的 Cookie 可能是任意内容：非 ASCII 或过长的直接当作无会话，不能抛异常变成 500。"""
    return bool(token) and len(token) <= 128 and token.isascii()


@dataclass
class AdminSession:
    admin_id: int
    username: str
    csrf: str


class AdminAuth:
    def __init__(self, db: Database):
        self.db = db

    # ------------------------------------------------------------ 账号（CLI 用）

    def create_admin(self, username: str, password: str) -> int:
        username = check_username(username)
        check_password_strength(password, username)
        with self.db.write() as conn:
            if conn.execute("SELECT 1 FROM admins WHERE username = ?", (username,)).fetchone():
                raise LicenseError("exists", f"管理员 {username} 已存在", 409)
            cur = conn.execute(
                "INSERT INTO admins(username, pw_hash, created_at) VALUES (?,?,?)",
                (username, hash_password(password), now()),
            )
            audit(conn, "cli", "admin.create", username=username)
            return cur.lastrowid

    def set_password(self, username: str, password: str, actor: str = "cli", ip: str = "") -> None:
        username = check_username(username)
        check_password_strength(password, username)
        with self.db.write() as conn:
            row = conn.execute("SELECT id FROM admins WHERE username = ?", (username,)).fetchone()
            if row is None:
                raise LicenseError("not_found", f"管理员 {username} 不存在", 404)
            conn.execute(
                "UPDATE admins SET pw_hash = ?, failed_count = 0, locked_until = NULL WHERE id = ?",
                (hash_password(password), row["id"]),
            )
            conn.execute("DELETE FROM admin_sessions WHERE admin_id = ?", (row["id"],))
            audit(conn, actor, "admin.password", ip, username=username)

    def set_disabled(self, username: str, disabled: bool) -> None:
        username = check_username(username)
        with self.db.write() as conn:
            row = conn.execute("SELECT id FROM admins WHERE username = ?", (username,)).fetchone()
            if row is None:
                raise LicenseError("not_found", f"管理员 {username} 不存在", 404)
            conn.execute("UPDATE admins SET disabled_at = ? WHERE id = ?", (now() if disabled else None, row["id"]))
            if disabled:
                conn.execute("DELETE FROM admin_sessions WHERE admin_id = ?", (row["id"],))
            audit(conn, "cli", "admin.disable" if disabled else "admin.enable", username=username)

    def unlock(self, username: str) -> None:
        username = check_username(username)
        with self.db.write() as conn:
            n = conn.execute(
                "UPDATE admins SET failed_count = 0, locked_until = NULL WHERE username = ?", (username,)
            ).rowcount
            if n == 0:
                raise LicenseError("not_found", f"管理员 {username} 不存在", 404)
            audit(conn, "cli", "admin.unlock", username=username)

    def list_admins(self) -> list[dict]:
        return [
            dict(r)
            for r in self.db.read().execute(
                "SELECT id, username, created_at, disabled_at, last_login_at, failed_count, locked_until FROM admins ORDER BY id"
            )
        ]

    # ------------------------------------------------------------ 登录与会话

    def login(self, username: str, password: str, ip: str) -> tuple[str, AdminSession]:
        """成功返回 (会话令牌明文, 会话)；令牌只进 Cookie，库里只存摘要。

        失败计数与审计必须先提交再抛错：在事务里抛错会回滚，锁定就永远不会生效。
        """
        fail = LicenseError("login_failed", "用户名或密码错误", 401)
        try:
            username = check_username(username)
        except LicenseError:
            verify_password(password or "", _DUMMY_HASH)
            raise fail from None
        at = now()
        error: LicenseError | None = None
        with self.db.write() as conn:
            row = conn.execute("SELECT * FROM admins WHERE username = ?", (username,)).fetchone()
            if row is None:
                verify_password(password or "", _DUMMY_HASH)
                audit(conn, username, "admin.login.fail", ip, reason="no_user")
                error = fail
            elif row["locked_until"] and row["locked_until"] > at:
                verify_password(password or "", _DUMMY_HASH)
                audit(conn, username, "admin.login.fail", ip, reason="locked")
                error = LicenseError("locked", "登录失败次数过多，账号已临时锁定，请稍后再试", 429,
                                     retry_after=row["locked_until"] - at)
            else:
                ok = verify_password(password or "", row["pw_hash"])
                if not ok or row["disabled_at"] is not None:
                    failed = row["failed_count"] + 1
                    locked = at + LOCK_SECONDS if failed >= LOCK_AFTER else None
                    conn.execute(
                        "UPDATE admins SET failed_count = ?, locked_until = ? WHERE id = ?",
                        (0 if locked else failed, locked, row["id"]),
                    )
                    audit(conn, username, "admin.login.fail", ip, reason="disabled" if ok else "password",
                          locked=bool(locked))
                    error = fail
                else:
                    token = secrets.token_urlsafe(32)
                    csrf = secrets.token_urlsafe(32)
                    conn.execute("DELETE FROM admin_sessions WHERE expires_at <= ? OR last_used_at <= ?",
                                 (at, at - SESSION_IDLE))
                    conn.execute(
                        "INSERT INTO admin_sessions(token_digest, admin_id, csrf, created_at, last_used_at, expires_at, ip) "
                        "VALUES (?,?,?,?,?,?,?)",
                        (_token_digest(token), row["id"], csrf, at, at, at + SESSION_TTL, ip),
                    )
                    conn.execute(
                        "UPDATE admins SET failed_count = 0, locked_until = NULL, last_login_at = ? WHERE id = ?",
                        (at, row["id"]),
                    )
                    audit(conn, username, "admin.login", ip)
        if error is not None:
            raise error
        return token, AdminSession(row["id"], username, csrf)
    def session(self, token: str | None, ip: str) -> AdminSession | None:
        if not _plausible_token(token):
            return None
        at = now()
        dig = _token_digest(token)
        with self.db.write() as conn:
            row = conn.execute(
                "SELECT s.*, a.username, a.disabled_at FROM admin_sessions s JOIN admins a ON a.id = s.admin_id "
                "WHERE s.token_digest = ?",
                (dig,),
            ).fetchone()
            if row is None:
                return None
            expired = row["expires_at"] <= at or row["last_used_at"] <= at - SESSION_IDLE
            if expired or row["disabled_at"] is not None or not hmac.compare_digest(row["ip"], ip):
                conn.execute("DELETE FROM admin_sessions WHERE token_digest = ?", (dig,))
                return None
            conn.execute("UPDATE admin_sessions SET last_used_at = ? WHERE token_digest = ?", (at, dig))
            return AdminSession(row["admin_id"], row["username"], row["csrf"])

    def logout(self, token: str | None) -> None:
        if _plausible_token(token):
            with self.db.write() as conn:
                conn.execute("DELETE FROM admin_sessions WHERE token_digest = ?", (_token_digest(token),))

    def reauth(self, sess: AdminSession, password: str, ip: str) -> None:
        """敏感操作前重新核对密码；错了也计入失败次数（先提交再抛错）。"""
        error: LicenseError | None = None
        with self.db.write() as conn:
            row = conn.execute("SELECT * FROM admins WHERE id = ?", (sess.admin_id,)).fetchone()
            if row is None or row["disabled_at"] is not None:
                error = LicenseError("unauthorized", "请重新登录", 401)
            elif not verify_password(password or "", row["pw_hash"]):
                failed = row["failed_count"] + 1
                if failed >= LOCK_AFTER:
                    conn.execute(
                        "UPDATE admins SET failed_count = 0, locked_until = ? WHERE id = ?",
                        (now() + LOCK_SECONDS, row["id"]),
                    )
                    conn.execute("DELETE FROM admin_sessions WHERE admin_id = ?", (row["id"],))
                else:
                    conn.execute("UPDATE admins SET failed_count = ? WHERE id = ?", (failed, row["id"]))
                audit(conn, sess.username, "admin.reauth.fail", ip)
                error = LicenseError("reauth_failed", "密码错误", 403)
        if error is not None:
            raise error

    def change_password(self, sess: AdminSession, old: str, new: str, ip: str) -> None:
        self.reauth(sess, old, ip)
        self.set_password(sess.username, new, actor=sess.username, ip=ip)
