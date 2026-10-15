"""授权凭证：签名、严格验签、密钥文件，以及给桌面端对拍用的测试向量。

测试向量 license_vectors_v1.json 由本文件的 build_vectors() 确定性生成并提交进仓库
（Ed25519 签名是确定性的，同样的种子和载荷永远得到同样的字节）。
改了格式或用例时，用 UPDATE_LICENSE_VECTORS=1 重新生成；平时这里只核对文件没有走样。
"""

from __future__ import annotations

import json
import logging
import os
import stat
from pathlib import Path

import pytest
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

from drama_cloud.commerce import license_token as lt
from drama_cloud.commerce.config import ConfigError

VECTORS_PATH = Path(__file__).with_name("license_vectors_v1.json")
TEST_SEED = bytes(range(32))
OTHER_SEED = bytes(range(32, 64))
NOW = 1767225600  # 2026-01-01T00:00:00Z
EXP = NOW + 72 * 3600
DEVICE = "a" * 64
OTHER_DEVICE = "b" * 64
USER = "00000000-0000-4000-8000-000000000001"


def _payload(signer, **overrides):
    payload = {
        "v": 1,
        "kid": signer.kid,
        "sub": USER,
        "dev": DEVICE,
        "ent": "timed",
        "ent_until": NOW + 30 * 86400,
        "iat": NOW,
        "exp": EXP,
    }
    payload.update(overrides)
    return payload


def _flip_bit(segment: str) -> str:
    raw = bytearray(lt.b64url_decode(segment))
    raw[len(raw) // 2] ^= 0x01
    return lt.b64url_encode(bytes(raw))


def _non_canonical(segment: str) -> str:
    """同一段字节换一种写法：最后一个字符的低位填 1（解码器会忽略这几位）。"""
    alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
    assert len(segment) % 4 in (2, 3)
    last = alphabet.index(segment[-1])
    return segment[:-1] + alphabet[last | 1]


def _pub(signer) -> str:
    return lt.b64url_encode(signer.public_key)


def build_vectors() -> dict:
    test = lt.Signer.from_seed(TEST_SEED)
    other = lt.Signer.from_seed(OTHER_SEED)
    cases = []

    def case(name, token, *, expect=None, error=None, trusted=(test,), device=DEVICE, now=NOW):
        entry = {
            "name": name,
            "token": token,
            "trusted": [_pub(signer) for signer in trusted],
            "device_id": device,
            "now": now,
        }
        if error is not None:
            entry["error"] = error
        else:
            entry["expect"] = expect
        cases.append(entry)

    def expect_of(payload):
        return {
            "kid": payload["kid"],
            "user_id": payload["sub"],
            "device_id": payload["dev"],
            "entitlement": payload["ent"],
            "entitlement_until": payload.get("ent_until"),
            "issued_at": payload["iat"],
            "expires_at": payload["exp"],
        }

    base = _payload(test)
    good = test.sign(base)
    case("valid_timed", good, expect=expect_of(base))
    case("valid_just_before_expiry", good, expect=expect_of(base), now=EXP - 1)
    for kind in ("lifetime", "none"):
        payload = _payload(test, ent=kind, ent_until=None)
        case(f"valid_{kind}", test.sign(payload), expect=expect_of(payload))
    lifetime_no_until = _payload(test, ent="lifetime")
    del lifetime_no_until["ent_until"]
    case("valid_ent_until_absent_means_null", test.sign(lifetime_no_until),
         expect=expect_of(lifetime_no_until))
    extra = _payload(test, future_field={"anything": [1, 2]})
    case("valid_unknown_field_ignored", test.sign(extra), expect=expect_of(base))
    by_other = _payload(other)
    case("valid_second_trusted_key", other.sign(by_other), expect=expect_of(by_other),
         trusted=(test, other))

    # 伪造：改载荷、换签名、不认识的钥匙、漏了用途前缀
    payload_part, signature_part = good.split(".")
    for name, forged in [
        ("bad_signature_forged_lifetime", dict(base, ent="lifetime", ent_until=None)),
        ("bad_signature_forged_expiry", dict(base, exp=EXP + 365 * 86400)),
        ("bad_signature_forged_device", dict(base, dev=OTHER_DEVICE)),
    ]:
        token = lt.b64url_encode(lt.canonical_payload(forged)) + "." + signature_part
        case(name, token, error="bad_signature", device=forged["dev"])
    case("bad_signature_payload_bit_flipped", _flip_bit(payload_part) + "." + signature_part,
         error="bad_signature")
    case("bad_signature_signature_bit_flipped", payload_part + "." + _flip_bit(signature_part),
         error="bad_signature")
    case("bad_signature_untrusted_key", other.sign(by_other), error="bad_signature")
    case("bad_signature_empty_trust_list", good, error="bad_signature", trusted=())
    raw_key = Ed25519PrivateKey.from_private_bytes(TEST_SEED)
    payload_bytes = lt.canonical_payload(base)
    case("bad_signature_missing_context",
         lt.b64url_encode(payload_bytes) + "." + lt.b64url_encode(raw_key.sign(payload_bytes)),
         error="bad_signature")

    # 格式：只认唯一一种写法
    for name, token in [
        ("malformed_no_dot", payload_part + signature_part),
        ("malformed_two_dots", good + "."),
        ("malformed_empty_payload", "." + signature_part),
        ("malformed_empty_signature", payload_part + "."),
        ("malformed_padding", good + "=="),
        ("malformed_illegal_char", payload_part + "." + signature_part[:-1] + "+"),
        ("malformed_leading_space", " " + good),
        ("malformed_trailing_newline", good + "\n"),
        ("malformed_non_canonical_signature", payload_part + "." + _non_canonical(signature_part)),
        ("malformed_short_signature",
         payload_part + "." + lt.b64url_encode(lt.b64url_decode(signature_part)[:63])),
    ]:
        case(name, token, error="malformed")

    # 载荷：签名是真的，但内容不合规
    canonical = lt.canonical_payload(base)
    missing_dev = {key: value for key, value in base.items() if key != "dev"}
    for name, raw in [
        ("bad_payload_not_json", b"not json"),
        ("bad_payload_not_utf8", b"\xff\xfe"),
        ("bad_payload_not_object", b"[1,2,3]"),
        ("bad_payload_nan", canonical.replace(f'"exp":{EXP}'.encode(), b'"exp":NaN')),
        # NaN 不是合法 JSON：就算出现在不认识的字段里也要拒绝，否则两边解析器结论不一样。
        ("bad_payload_nan_in_unknown_field", b'{"x":NaN,' + canonical[1:]),
        ("bad_payload_duplicate_key", b'{"ent":"lifetime",' + canonical[1:]),
        ("bad_payload_duplicate_unknown_key", b'{"x":1,"x":2,' + canonical[1:]),
        ("bad_payload_missing_dev", lt.canonical_payload(missing_dev)),
    ]:
        case(name, test.sign_bytes(raw), error="bad_payload")
    for name, overrides in [
        ("bad_payload_version_2", {"v": 2}),
        ("bad_payload_version_bool", {"v": True}),
        ("bad_payload_version_float", {"v": 1.0}),
        ("bad_payload_version_string", {"v": "1"}),
        ("bad_payload_kid_mismatch", {"kid": "0" * 16}),
        ("bad_payload_empty_sub", {"sub": ""}),
        ("bad_payload_sub_not_string", {"sub": 1}),
        ("bad_payload_unknown_entitlement", {"ent": "gold"}),
        ("bad_payload_timed_without_until", {"ent_until": None}),
        ("bad_payload_lifetime_with_until", {"ent": "lifetime"}),
        ("bad_payload_exp_not_after_iat", {"exp": NOW}),
        ("bad_payload_negative_iat", {"iat": -1}),
        ("bad_payload_int_overflow", {"exp": 2**63}),
        ("bad_payload_exp_string", {"exp": str(EXP)}),
    ]:
        case(name, test.sign(_payload(test, **overrides)), error="bad_payload")
    max_exp = _payload(test, exp=2**63 - 1)
    case("valid_max_int_exp", test.sign(max_exp), expect=expect_of(max_exp))

    # 机器码与有效期：机器码先于有效期检查
    case("wrong_device", good, error="wrong_device", device=OTHER_DEVICE)
    case("wrong_device_case_sensitive", good, error="wrong_device", device=DEVICE.upper())
    case("wrong_device_checked_before_expiry", good, error="wrong_device", device=OTHER_DEVICE,
         now=EXP + 1)
    case("expired_at_exact_expiry", good, error="expired", now=EXP)
    case("expired_after_expiry", good, error="expired", now=EXP + 1)
    return {
        "format": "drama-dossier license token v1 test vectors",
        "warning": "TEST ONLY：这里的种子是公开的，任何人都能用它签名。正式环境绝不能信任这里的公钥。",
        "check_order": ["malformed", "bad_signature", "bad_payload", "wrong_device", "expired"],
        "signing_context": lt.SIGNING_CONTEXT.decode("ascii"),
        "test_seed_hex": TEST_SEED.hex(),
        "other_seed_hex": OTHER_SEED.hex(),
        "cases": cases,
    }


def _dump(vectors: dict) -> str:
    return json.dumps(vectors, ensure_ascii=False, indent=2, sort_keys=True) + "\n"


def _load_cases():
    if not VECTORS_PATH.exists():
        return []
    return json.loads(VECTORS_PATH.read_text(encoding="utf-8"))["cases"]


def test_vectors_file_matches_generator():
    expected = _dump(build_vectors())
    if os.environ.get("UPDATE_LICENSE_VECTORS") == "1":
        VECTORS_PATH.write_text(expected, encoding="utf-8")
    assert VECTORS_PATH.exists(), "测试向量文件不存在，用 UPDATE_LICENSE_VECTORS=1 生成"
    assert VECTORS_PATH.read_text(encoding="utf-8") == expected, (
        "测试向量和生成器对不上；改了格式就用 UPDATE_LICENSE_VECTORS=1 重新生成，并同步桌面端"
    )


def test_vectors_cover_every_error_code():
    codes = {case.get("error") for case in _load_cases()}
    assert {"malformed", "bad_signature", "bad_payload", "wrong_device", "expired"} <= codes
    assert None in codes  # 也有通过的用例


@pytest.mark.parametrize("case", _load_cases(), ids=lambda case: case["name"])
def test_vector(case):
    trusted = [lt.b64url_decode(key) for key in case["trusted"]]
    if "error" in case:
        with pytest.raises(lt.LicenseError) as caught:
            lt.verify(case["token"], trusted, case["device_id"], case["now"])
        assert caught.value.code == case["error"]
    else:
        result = lt.verify(case["token"], trusted, case["device_id"], case["now"])
        assert {
            "kid": result.kid,
            "user_id": result.user_id,
            "device_id": result.device_id,
            "entitlement": result.entitlement,
            "entitlement_until": result.entitlement_until,
            "issued_at": result.issued_at,
            "expires_at": result.expires_at,
        } == case["expect"]


def test_non_string_token_is_malformed():
    with pytest.raises(lt.LicenseError) as caught:
        lt.verify(None, [], DEVICE, NOW)
    assert caught.value.code == "malformed"


def test_key_file_roundtrip(tmp_path):
    path = tmp_path / "keys" / "license.pem"
    signer = lt.generate_key_file(path)
    assert stat.S_IMODE(path.stat().st_mode) == 0o600
    loaded = lt.Signer.from_pem_file(path)
    assert (loaded.kid, loaded.public_key) == (signer.kid, signer.public_key)
    assert len(signer.public_key) == 32 and len(signer.kid) == 16
    token = loaded.sign(_payload(loaded))
    assert lt.verify(token, [signer.public_key], DEVICE, NOW).kid == signer.kid


def test_generate_refuses_to_overwrite(tmp_path):
    path = tmp_path / "license.pem"
    path.write_bytes(b"keep me")
    with pytest.raises(ConfigError, match="不会覆盖"):
        lt.generate_key_file(path)
    assert path.read_bytes() == b"keep me"


@pytest.mark.parametrize("content", [None, b"not a pem", b""])
def test_bad_key_file_is_config_error(tmp_path, content):
    path = tmp_path / "license.pem"
    if content is not None:
        path.write_bytes(content)
    with pytest.raises(ConfigError, match="keygen|PEM"):
        lt.Signer.from_pem_file(path)


def test_loose_permissions_warn_without_leaking_key(tmp_path, caplog):
    path = tmp_path / "license.pem"
    lt.generate_key_file(path)
    os.chmod(path, 0o644)
    pem = path.read_text()
    with caplog.at_level(logging.WARNING, logger="drama_cloud.commerce.license_token"):
        lt.Signer.from_pem_file(path)
    assert "权限过宽" in caplog.text
    body = "".join(pem.splitlines()[1:-1])
    assert body[:20] not in caplog.text


def test_signer_repr_hides_key():
    signer = lt.Signer.from_seed(TEST_SEED)
    assert TEST_SEED.hex() not in repr(signer) and signer.kid in repr(signer)
