"""授权凭证：`base64url(payload) "." base64url(signature)`。

签名对象 = 用途前缀 + payload 原始字节。客户端直接对收到的字节验签，
所以这里的序列化方式（sort_keys / 紧凑 / ASCII）就是协议本身，不能随意改。
"""

from __future__ import annotations

import json
from dataclasses import dataclass

from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey, Ed25519PublicKey

from .keys import b64url, b64url_decode, key_id, public_raw

SIGNING_CONTEXT = b"drama-dossier/license/v1\n"
PAYLOAD_FIELDS = ("v", "kid", "lid", "mid", "plan", "until", "iat", "exp")
PLAN_CODES = ("trial", "month", "quarter", "year", "lifetime")
MAX_TOKEN_LEN = 2048


class TokenError(ValueError):
    pass


@dataclass(frozen=True)
class Claims:
    kid: str
    lid: int
    mid: str
    plan: str
    until: int | None
    iat: int
    exp: int


def canonical_json(obj: dict) -> bytes:
    return json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode("ascii")


def sign(key: Ed25519PrivateKey, claims: Claims) -> str:
    payload = canonical_json(
        {
            "v": 1,
            "kid": claims.kid,
            "lid": claims.lid,
            "mid": claims.mid,
            "plan": claims.plan,
            "until": claims.until,
            "iat": claims.iat,
            "exp": claims.exp,
        }
    )
    sig = key.sign(SIGNING_CONTEXT + payload)
    return b64url(payload) + "." + b64url(sig)


def _reject_duplicates(pairs):
    seen = {}
    for k, v in pairs:
        if k in seen:
            raise TokenError("payload 存在重复字段")
        seen[k] = v
    return seen


def _reject_constant(name):
    raise TokenError(f"payload 含非法数值 {name}")


def _is_int(value) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and 0 <= value < 2**53


def parse_payload(payload: bytes) -> Claims:
    try:
        obj = json.loads(
            payload.decode("ascii"),
            object_pairs_hook=_reject_duplicates,
            parse_constant=_reject_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise TokenError(f"payload 不是合法 JSON：{exc}") from None
    if not isinstance(obj, dict) or set(obj) != set(PAYLOAD_FIELDS):
        raise TokenError("payload 字段不符")
    if obj["v"] != 1 or isinstance(obj["v"], bool):
        raise TokenError("不支持的凭证版本")
    if not (isinstance(obj["kid"], str) and len(obj["kid"]) == 16):
        raise TokenError("kid 格式错误")
    if not (isinstance(obj["mid"], str) and len(obj["mid"]) == 64 and all(c in "0123456789abcdef" for c in obj["mid"])):
        raise TokenError("机器码格式错误")
    if obj["plan"] not in PLAN_CODES:
        raise TokenError("卡种错误")
    for name in ("lid", "iat", "exp"):
        if not _is_int(obj[name]):
            raise TokenError(f"{name} 必须是非负整数")
    if obj["until"] is not None and not _is_int(obj["until"]):
        raise TokenError("until 必须是非负整数或 null")
    if (obj["plan"] == "lifetime") != (obj["until"] is None):
        raise TokenError("永久卡与 until 不一致")
    if obj["exp"] <= obj["iat"]:
        raise TokenError("exp 必须晚于 iat")
    return Claims(obj["kid"], obj["lid"], obj["mid"], obj["plan"], obj["until"], obj["iat"], obj["exp"])


def verify(token: str, trusted: list[Ed25519PublicKey]) -> Claims:
    """验签并解析。**不检查有效期**：是否仍有使用权由调用方按数据库判断。"""
    if not isinstance(token, str) or len(token) > MAX_TOKEN_LEN or token.count(".") != 1:
        raise TokenError("凭证格式错误")
    p64, s64 = token.split(".")
    try:
        payload = b64url_decode(p64)
        sig = b64url_decode(s64)
    except ValueError:
        raise TokenError("凭证编码错误") from None
    if len(sig) != 64:
        raise TokenError("签名长度错误")
    matched = None
    for pub in trusted:
        try:
            pub.verify(sig, SIGNING_CONTEXT + payload)
        except InvalidSignature:
            continue
        matched = pub
        break
    if matched is None:
        raise TokenError("签名无效")
    claims = parse_payload(payload)
    if claims.kid != key_id(public_raw(matched)):
        raise TokenError("kid 与签名公钥不符")
    return claims
