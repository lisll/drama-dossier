"""Ed25519 签名私钥：生成、加载（带权限检查）、kid 计算。"""

from __future__ import annotations

import base64
import hashlib
import os
import stat
from pathlib import Path

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey, Ed25519PublicKey

from .config import ConfigError


def b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


def b64url_decode(text: str) -> bytes:
    """严格解码：只接受无填充、字符集正确、能原样往返的 base64url。"""
    if not isinstance(text, str) or "=" in text:
        raise ValueError("base64url 不应带填充")
    pad = "=" * (-len(text) % 4)
    raw = base64.urlsafe_b64decode(text + pad)
    if b64url(raw) != text:
        raise ValueError("base64url 编码不规范")
    return raw


def public_raw(pub: Ed25519PublicKey) -> bytes:
    return pub.public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)


def key_id(pub_raw: bytes) -> str:
    return hashlib.sha256(pub_raw).hexdigest()[:16]


def check_secret_file_mode(path: Path) -> None:
    """私钥文件不能对其他用户可读、不能对组可写（允许 0600 或 0640 root:服务组）。"""
    if os.name != "posix":
        return
    mode = stat.S_IMODE(path.stat().st_mode)
    if mode & 0o037:
        raise ConfigError(
            f"{path} 权限过宽（{oct(mode)}），请执行 chmod 600 或 640；为安全起见拒绝启动"
        )


def generate_private_key(out: Path) -> Ed25519PrivateKey:
    if out.exists():
        raise ConfigError(f"{out} 已存在，拒绝覆盖（私钥一旦覆盖，已发出的授权全部失效）")
    out.parent.mkdir(parents=True, exist_ok=True)
    key = Ed25519PrivateKey.generate()
    pem = key.private_bytes(
        serialization.Encoding.PEM,
        serialization.PrivateFormat.PKCS8,
        serialization.NoEncryption(),
    )
    fd = os.open(out, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "wb") as fh:
        fh.write(pem)
    return key


def load_private_key(path: Path) -> Ed25519PrivateKey:
    if not path.exists():
        raise ConfigError(f"找不到签名私钥 {path}，请先执行 license-admin keygen")
    check_secret_file_mode(path)
    key = serialization.load_pem_private_key(path.read_bytes(), password=None)
    if not isinstance(key, Ed25519PrivateKey):
        raise ConfigError(f"{path} 不是 Ed25519 私钥")
    return key


def parse_public_keys(raw: str) -> list[Ed25519PublicKey]:
    """逗号分隔的 base64url 公钥列表（用于密钥轮换期间继续认旧凭证）。"""
    keys = []
    for part in raw.split(","):
        part = part.strip()
        if not part:
            continue
        try:
            data = b64url_decode(part)
        except ValueError:
            raise ConfigError(f"无效的公钥：{part!r}") from None
        if len(data) != 32:
            raise ConfigError(f"公钥长度应为 32 字节：{part!r}")
        keys.append(Ed25519PublicKey.from_public_bytes(data))
    return keys
