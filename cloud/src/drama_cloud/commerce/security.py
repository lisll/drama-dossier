"""令牌、验证码与隐私键：纯函数，不碰数据库。

- 令牌（访问令牌 / 刷新令牌）是 256 位随机数，数据库只存 SHA-256 摘要。
- 验证码只有 6 位数字，空间太小，普通哈希可以被离线穷举，所以用服务端密钥做 HMAC，
  并把用途和邮箱一起算进去：注册用的验证码不能拿去重设密码，也不能换个邮箱用。
- 限流桶的键同样用 HMAC 处理 IP 和邮箱，数据库里不出现明文。
"""

from __future__ import annotations

import hashlib
import hmac
import secrets

ACCESS_PREFIX = "at_"
REFRESH_PREFIX = "rt_"


def new_access_token() -> str:
    return ACCESS_PREFIX + secrets.token_urlsafe(32)


def new_refresh_token() -> str:
    return REFRESH_PREFIX + secrets.token_urlsafe(32)


def token_digest(token: str) -> bytes:
    return hashlib.sha256(token.encode("utf-8")).digest()


def new_code() -> str:
    return f"{secrets.randbelow(1_000_000):06d}"


def code_digest(secret: str, purpose: str, email: str, code: str) -> bytes:
    message = f"{purpose}\x00{email}\x00{code}".encode("utf-8")
    return hmac.new(secret.encode("utf-8"), message, hashlib.sha256).digest()


def digests_equal(left: bytes, right: bytes) -> bool:
    return hmac.compare_digest(bytes(left), bytes(right))


def privacy_key(secret: str, value: str) -> str:
    """把 IP、邮箱这类标识换成不可逆的短键，只用来当限流桶的名字。"""
    mac = hmac.new(secret.encode("utf-8"), b"rate\x00" + value.encode("utf-8"), hashlib.sha256)
    return mac.hexdigest()[:32]
