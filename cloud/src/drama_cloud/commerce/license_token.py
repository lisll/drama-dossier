"""授权凭证：服务端用 Ed25519 私钥签名，桌面端用内置公钥在本地验签。

格式：`base64url(载荷 JSON) + "." + base64url(签名)`，base64url 不带填充，只认这一种写法。
签名对象是 `SIGNING_CONTEXT + 载荷字节`。固定前缀用来隔离用途：同一把私钥以后签别的东西，
那些签名也不能被挪来冒充授权凭证。

`verify` 是参考实现。桌面端（Rust）的验签必须和它逐条一致，
两边用同一份测试向量对拍：tests/commerce/license_vectors_v1.json。
"""

from __future__ import annotations

import base64
import binascii
import hashlib
import json
import logging
import os
import re
import stat
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Optional

from cryptography.exceptions import InvalidSignature, UnsupportedAlgorithm
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey, Ed25519PublicKey

from .config import ConfigError

log = logging.getLogger(__name__)

SIGNING_CONTEXT = b"drama-dossier/license/v1\n"
PAYLOAD_VERSION = 1
ENTITLEMENT_KINDS = ("none", "timed", "lifetime")
#: 整数字段的取值范围：非负且小于 2^63，和 Rust 的 i64 一致。
_INT_LIMIT = 2**63
_B64URL = re.compile(r"[A-Za-z0-9_-]+")


class LicenseError(ValueError):
    """验签失败。code 是 malformed / bad_signature / bad_payload / wrong_device / expired 之一。"""

    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code


@dataclass(frozen=True)
class License:
    kid: str
    user_id: str
    device_id: str
    entitlement: str
    entitlement_until: Optional[int]
    issued_at: int
    expires_at: int


def b64url_encode(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode("ascii")


def b64url_decode(text: str) -> bytes:
    """严格解码：解出来再编码一遍必须和原文一模一样，否则拒绝（同一段字节只认一种写法）。"""
    if not _B64URL.fullmatch(text):
        raise LicenseError("malformed", "授权凭证格式不正确")
    try:
        raw = base64.b64decode(text + "=" * (-len(text) % 4), altchars=b"-_", validate=True)
    except (binascii.Error, ValueError) as error:
        raise LicenseError("malformed", "授权凭证格式不正确") from error
    if b64url_encode(raw) != text:
        raise LicenseError("malformed", "授权凭证格式不正确")
    return raw


def key_id(public_key: bytes) -> str:
    return hashlib.sha256(public_key).hexdigest()[:16]


def canonical_payload(payload: dict) -> bytes:
    return json.dumps(payload, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode("ascii")


class Signer:
    """服务端签名器。私钥从文件加载，不进仓库，也不进日志。"""

    def __init__(self, private_key: Ed25519PrivateKey):
        self._private_key = private_key
        self.public_key = private_key.public_key().public_bytes(
            serialization.Encoding.Raw, serialization.PublicFormat.Raw
        )
        self.kid = key_id(self.public_key)

    @classmethod
    def from_seed(cls, seed: bytes) -> "Signer":
        return cls(Ed25519PrivateKey.from_private_bytes(seed))

    @classmethod
    def from_pem_file(cls, path) -> "Signer":
        path = Path(path)
        try:
            data = path.read_bytes()
            mode = path.stat().st_mode
        except OSError as error:
            raise ConfigError(
                f"读不到授权签名私钥 {path}（{type(error).__name__}），可以用 drama-commerce keygen 生成"
            ) from error
        if mode & (stat.S_IRWXG | stat.S_IRWXO):
            log.warning("授权签名私钥文件权限过宽（%o），建议改成 600", stat.S_IMODE(mode))
        try:
            key = serialization.load_pem_private_key(data, password=None)
        except (ValueError, TypeError, UnsupportedAlgorithm) as error:
            raise ConfigError(f"{path} 不是可用的 PEM 私钥（私钥不能加密）") from error
        if not isinstance(key, Ed25519PrivateKey):
            raise ConfigError(f"{path} 不是 Ed25519 私钥")
        return cls(key)

    def sign_bytes(self, payload: bytes) -> str:
        signature = self._private_key.sign(SIGNING_CONTEXT + payload)
        return b64url_encode(payload) + "." + b64url_encode(signature)

    def sign(self, payload: dict) -> str:
        return self.sign_bytes(canonical_payload(payload))

    def __repr__(self) -> str:
        return f"Signer(kid={self.kid!r})"


def generate_key_file(path) -> Signer:
    """生成新私钥写到 path（权限 600）。文件已存在就报错，绝不覆盖。"""
    path = Path(path)
    key = Ed25519PrivateKey.generate()
    pem = key.private_bytes(
        serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()
    )
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    try:
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError as error:
        raise ConfigError(f"{path} 已存在，不会覆盖。请换一个路径") from error
    with os.fdopen(fd, "wb") as handle:
        handle.write(pem)
        handle.flush()
        os.fsync(handle.fileno())
    return Signer(key)


def verify(token: str, trusted_keys: Iterable[bytes], device_id: str, now: int) -> License:
    """本地验签：格式 → 签名 → 载荷 → 机器码 → 有效期，任何一步不过都抛 LicenseError。

    先验签名再解析载荷：没通过签名的内容，一个字节都不信。
    """
    payload_part, dot, signature_part = token.partition(".") if isinstance(token, str) else ("", "", "")
    if not dot or "." in signature_part:
        raise LicenseError("malformed", "授权凭证格式不正确")
    payload = b64url_decode(payload_part)
    signature = b64url_decode(signature_part)
    if len(signature) != 64:
        raise LicenseError("malformed", "授权凭证格式不正确")
    signed_by = None
    for public_key in trusted_keys:
        try:
            Ed25519PublicKey.from_public_bytes(public_key).verify(signature, SIGNING_CONTEXT + payload)
        except InvalidSignature:
            continue
        signed_by = key_id(public_key)
        break
    if signed_by is None:
        raise LicenseError("bad_signature", "授权凭证签名无效")
    license_ = _parse_payload(payload, signed_by)
    if license_.device_id != device_id:
        raise LicenseError("wrong_device", "授权凭证不属于这台电脑")
    if now >= license_.expires_at:
        raise LicenseError("expired", "授权凭证已过期，请联网刷新")
    return license_


def _reject_duplicates(pairs):
    keys = [key for key, _ in pairs]
    if len(keys) != len(set(keys)):
        raise ValueError("duplicate key")
    return dict(pairs)


def _reject_constant(name):
    raise ValueError(f"非法常量 {name}")


def _is_int(value) -> bool:
    # bool 是 int 的子类，要单独排除。
    return type(value) is int and 0 <= value < _INT_LIMIT


def _parse_payload(raw: bytes, signed_by: str) -> License:
    try:
        payload = json.loads(
            raw.decode("utf-8"), object_pairs_hook=_reject_duplicates, parse_constant=_reject_constant
        )
    except ValueError as error:
        raise LicenseError("bad_payload", "授权凭证内容无效") from error
    if not isinstance(payload, dict):
        raise LicenseError("bad_payload", "授权凭证内容无效")
    kind = payload.get("ent")
    until = payload.get("ent_until")
    valid = (
        _is_int(payload.get("v")) and payload["v"] == PAYLOAD_VERSION
        and payload.get("kid") == signed_by
        and isinstance(payload.get("sub"), str) and payload["sub"] != ""
        and isinstance(payload.get("dev"), str) and payload["dev"] != ""
        and kind in ENTITLEMENT_KINDS
        and (_is_int(until) if kind == "timed" else until is None)
        and _is_int(payload.get("iat")) and _is_int(payload.get("exp"))
        and payload["exp"] > payload["iat"]
    )
    if not valid:
        raise LicenseError("bad_payload", "授权凭证内容无效")
    return License(
        kid=payload["kid"],
        user_id=payload["sub"],
        device_id=payload["dev"],
        entitlement=kind,
        entitlement_until=until,
        issued_at=payload["iat"],
        expires_at=payload["exp"],
    )
