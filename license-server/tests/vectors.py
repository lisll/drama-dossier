"""生成与 Rust 客户端共用的测试向量（crates/dossier-cardkey/tests/vectors_v1.json）。

用公开测试种子签名，确定性输出。Python 端测试会核对文件与生成器一致，
防止有人只改了一边的协议。

期望结果码（与 Rust `decide` 的 LockReason 对应）：
  ok / malformed / bad_signature / bad_payload / wrong_machine / clock_rollback / expired
"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

from dd_license.keys import b64url, b64url_decode, key_id, public_raw
from dd_license.token import SIGNING_CONTEXT, Claims, canonical_json, sign

TEST_SEED = bytes(range(32))
OTHER_SEED = bytes(range(32, 64))
VECTOR_PATH = Path(__file__).resolve().parents[2] / "crates" / "dossier-cardkey" / "tests" / "vectors_v1.json"

IAT = 1_800_000_000
DAY = 86400


def _mid(n: int) -> str:
    return hashlib.sha256(f"machine-{n}".encode()).hexdigest()


def _resign(key: Ed25519PrivateKey, payload: bytes) -> str:
    return b64url(payload) + "." + b64url(key.sign(SIGNING_CONTEXT + payload))


def build_vectors() -> dict:
    k = Ed25519PrivateKey.from_private_bytes(TEST_SEED)
    other = Ed25519PrivateKey.from_private_bytes(OTHER_SEED)
    kid = key_id(public_raw(k.public_key()))
    other_kid = key_id(public_raw(other.public_key()))
    M1, M2 = _mid(1), _mid(2)

    def claims(**kw):
        base = dict(kid=kid, lid=7, mid=M1, plan="month", until=IAT + 30 * DAY, iat=IAT, exp=IAT + 7 * DAY)
        base.update(kw)
        return Claims(**base)

    good = sign(k, claims())
    good_payload = json.loads(b64url_decode(good.split(".")[0]))

    def mutated(**changes):
        obj = {**good_payload, **changes}
        return _resign(k, canonical_json(obj))

    p64, s64 = good.split(".")
    sig = bytearray(b64url_decode(s64))
    sig[10] ^= 0x40
    flipped = p64 + "." + b64url(bytes(sig))

    cases = []

    def case(name, token, now, expect, mid=M1, last_seen=0, plan=None, until=None, exp=None):
        c = {"name": name, "token": token, "machine_id": mid, "now": now, "last_seen": last_seen, "expect": expect}
        if expect == "ok":
            c["plan"], c["until"], c["exp"] = plan, until, exp
        cases.append(c)

    ok_kw = dict(plan="month", until=IAT + 30 * DAY, exp=IAT + 7 * DAY)
    case("valid", good, IAT + 3600, "ok", **ok_kw)
    case("valid_at_iat", good, IAT, "ok", **ok_kw)
    case("valid_last_second", good, IAT + 7 * DAY - 1, "ok", **ok_kw)
    case("expired_at_exp", good, IAT + 7 * DAY, "expired")
    case("expired_long_after", good, IAT + 100 * DAY, "expired")
    # 授权到期（until）早于凭证到期（exp）：服务端正常不会签出这种凭证（exp = min(iat+7天, until)），
    # 但客户端仍必须以较早的 until 为准
    early = sign(k, claims(plan="month", until=IAT + 2 * DAY, exp=IAT + 7 * DAY))
    case("until_before_exp_still_valid", early, IAT + 2 * DAY - 1, "ok", plan="month", until=IAT + 2 * DAY, exp=IAT + 7 * DAY)
    case("until_before_exp", early, IAT + 2 * DAY, "expired")
    case("until_zero", sign(k, claims(until=0)), IAT, "expired")
    case("trial_valid", sign(k, claims(plan="trial", until=IAT + 3 * DAY, exp=IAT + 3 * DAY)), IAT + DAY, "ok",
         plan="trial", until=IAT + 3 * DAY, exp=IAT + 3 * DAY)
    case("lifetime_valid", sign(k, claims(plan="lifetime", until=None)), IAT + DAY, "ok",
         plan="lifetime", until=None, exp=IAT + 7 * DAY)
    case("lifetime_still_needs_refresh", sign(k, claims(plan="lifetime", until=None)), IAT + 8 * DAY, "expired")
    case("wrong_machine", good, IAT + 3600, "wrong_machine", mid=M2)
    case("rollback_before_iat", good, IAT - 3600, "clock_rollback")
    case("rollback_within_tolerance", good, IAT - 300, "ok", **ok_kw)
    case("rollback_vs_last_seen", good, IAT + 3600, "clock_rollback", last_seen=IAT + 2 * DAY)
    case("last_seen_within_tolerance", good, IAT + 3600, "ok", last_seen=IAT + 3600 + 500, **ok_kw)
    case("other_key", sign(other, claims(kid=other_kid)), IAT + 3600, "bad_signature")
    case("flipped_signature", flipped, IAT + 3600, "bad_signature")
    case("kid_mismatch", sign(k, claims(kid=other_kid)), IAT + 3600, "bad_payload")
    case("extra_field", mutated(admin=True), IAT + 3600, "bad_payload")
    case("missing_field", _resign(k, canonical_json({x: v for x, v in good_payload.items() if x != "lid"})), IAT + 3600, "bad_payload")
    case("version_2", mutated(v=2), IAT + 3600, "bad_payload")
    case("unknown_plan", mutated(plan="forever"), IAT + 3600, "bad_payload")
    case("lifetime_with_until", mutated(plan="lifetime"), IAT + 3600, "bad_payload")
    case("timed_without_until", mutated(until=None), IAT + 3600, "bad_payload")
    case("exp_not_after_iat", mutated(exp=IAT), IAT, "bad_payload")
    case("bool_as_int", mutated(lid=True), IAT + 3600, "bad_payload")
    case("negative_int", mutated(lid=-1), IAT + 3600, "bad_payload")
    case("float_time", mutated(exp=IAT + 7.5 * DAY), IAT + 3600, "bad_payload")
    case("uppercase_mid", mutated(mid=M1.upper()), IAT + 3600, "bad_payload")
    dup = (
        '{"exp":%d,"iat":%d,"kid":"%s","lid":7,"mid":"%s","plan":"month","plan":"lifetime","until":%d,"v":1}'
        % (IAT + 7 * DAY, IAT, kid, M1, IAT + 30 * DAY)
    ).encode()
    case("duplicate_key", _resign(k, dup), IAT + 3600, "bad_payload")
    case("not_json", _resign(k, b"not json"), IAT + 3600, "bad_payload")
    case("empty", "", IAT, "malformed")
    case("no_dot", "abc", IAT, "malformed")
    case("two_dots", "a.b.c", IAT, "malformed")
    case("padded", p64 + "==." + s64, IAT + 3600, "malformed")
    case("bad_base64_char", p64[:-1] + "*." + s64, IAT + 3600, "malformed")
    case("short_signature", p64 + "." + b64url(b"x" * 63), IAT + 3600, "malformed")
    case("long_signature", p64 + "." + b64url(b"x" * 65), IAT + 3600, "malformed")
    # 非规范 base64url：64 字节签名编码成 86 个字符，最后一个字符只有高 2 位有效。
    # 把低 4 位置 1 后解码结果不变，但不再是唯一编码，两端都必须拒绝。
    alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
    assert len(s64) == 86 and alphabet.index(s64[-1]) & 0x0F == 0
    noncanon = s64[:-1] + alphabet[alphabet.index(s64[-1]) | 0x0F]
    case("noncanonical_base64", p64 + "." + noncanon, IAT + 3600, "malformed")
    case("too_long", "A" * 2049, IAT, "malformed")
    case("version_float", mutated(v=1.0), IAT + 3600, "bad_payload")
    case("int_at_2pow53", mutated(lid=2**53), IAT + 3600, "bad_payload")
    case("int_below_2pow53", mutated(lid=2**53 - 1), IAT + 3600, "ok", **ok_kw)
    case("payload_is_array", _resign(k, b"[1,2,3]"), IAT + 3600, "bad_payload")
    case("short_kid", mutated(kid=kid[:15]), IAT + 3600, "bad_payload")
    case("short_mid", mutated(mid=M1[:63]), IAT + 3600, "bad_payload")
    # 字段齐全、只是 plan 的值含非 ASCII 字节：两端都必须在解析阶段拒绝
    non_ascii = canonical_json(good_payload).replace(b'"plan":"month"', b'"plan":"m\xc3\xb6nth"')
    assert non_ascii != canonical_json(good_payload)
    case("non_ascii_payload", _resign(k, non_ascii), IAT + 3600, "bad_payload")

    return {
        "version": 1,
        "warning": "公开测试密钥，只能用于测试，绝不能写进 PRODUCTION_KEYS",
        "signing_context": SIGNING_CONTEXT.decode(),
        "trusted_public_keys": [b64url(public_raw(k.public_key()))],
        "rollback_tolerance_secs": 600,
        "cases": cases,
    }


def render() -> str:
    return json.dumps(build_vectors(), ensure_ascii=False, indent=2) + "\n"


if __name__ == "__main__":
    VECTOR_PATH.parent.mkdir(parents=True, exist_ok=True)
    VECTOR_PATH.write_text(render(), encoding="utf-8")
    print(f"已写入 {VECTOR_PATH}")
