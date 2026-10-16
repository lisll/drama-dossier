"""报审包清单与处理配方的签名测试向量（signed_vectors_v1.json）：服务端生成，桌面端对拍。

和 license_vectors_v1.json 一样，由本文件的 build_vectors() 确定性生成并提交进仓库；
改了格式或用例时用 UPDATE_SIGNED_VECTORS=1 重新生成。桌面端
crates/dossier-license/tests/signed_vectors.rs 逐条跑 `manifest::verify` / `recipe::verify`，错误码必须一致。

清单和配方都用生产代码签（package_service.encode_manifest / sign_manifest、Signer.sign_envelope），
向量不会和服务端实际发出的格式走样。服务端只签不验，所以 Python 侧核对的是：
每条正常用例的签名只在自己的用途前缀下成立，换成另外两种用途都不成立；授权凭证的验签也不认配方。
"""

from __future__ import annotations

import json
import os
from pathlib import Path

import pytest
from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey

from drama_cloud.commerce import license_token as lt
from drama_cloud.commerce.package_service import MANIFEST_SCHEMA, encode_manifest, sign_manifest

VECTORS_PATH = Path(__file__).with_name("signed_vectors_v1.json")
TEST_SEED = bytes(range(32))
OTHER_SEED = bytes(range(32, 64))
NOW = 1767225600  # 2026-01-01T00:00:00Z
EXP = NOW + 24 * 3600
DEVICE = "a" * 64
OTHER_DEVICE = "b" * 64
USER = "00000000-0000-4000-8000-000000000001"
PACKAGE_ID = "00000000-0000-4000-8000-0000000000bb"
RECIPE_ID = "00000000-0000-4000-8000-0000000000aa"
TASK = "c" * 64
OTHER_TASK = "d" * 64
H1, H2, H3 = "1" * 64, "2" * 64, "3" * 64
COST_PDF = "04_成本配置表/成本配置表_未签署.pdf"
OWNERSHIP_PDF = "05_权属声明/内容权属声明_未签署.pdf"

MANIFEST_ERRORS = {"malformed", "bad_signature", "bad_payload", "wrong_device"}
RECIPE_ERRORS = MANIFEST_ERRORS | {"wrong_task", "expired"}


def _non_canonical(segment: str) -> str:
    """同一段字节换一种写法：最后一个字符的低位填 1（解码器会忽略这几位）。"""
    alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
    assert len(segment) % 4 in (2, 3)
    return segment[:-1] + alphabet[alphabet.index(segment[-1]) | 1]


def _pub(signer) -> str:
    return lt.b64url_encode(signer.public_key)


def _manifest(**overrides) -> dict:
    manifest = {
        "schema": MANIFEST_SCHEMA,
        "packageId": PACKAGE_ID,
        "generatedAt": "2026-01-01T00:00:00Z",
        "userId": USER,
        "deviceId": DEVICE,
        "dramaName": "测试剧",
        "aiGenerated": False,
        "copyEpisodes": False,
        "signature": {"signer": "mock", "placeholder": True, "documents": [COST_PDF, OWNERSHIP_PDF]},
        "files": [
            {"path": "01_剧集信息/剧名.txt", "sizeBytes": 9, "sha256": H1, "source": "server"},
            {"path": COST_PDF, "sizeBytes": 1000, "sha256": H2, "source": "server"},
            {"path": OWNERSHIP_PDF, "sizeBytes": 1000, "sha256": H3, "source": "server"},
            {"path": "02_封面海报/封面.jpg", "sizeBytes": 20, "sha256": H1, "source": "local"},
        ],
        "references": [{"path": "第01集.mp4", "sizeBytes": 30, "sha256": H2}],
    }
    manifest.update(overrides)
    return manifest


def _with_file(path: str, **fields) -> dict:
    """在清单里多加一个文件（用来测路径和哈希规则）。"""
    manifest = _manifest()
    manifest["files"] = manifest["files"] + [
        {"path": path, "sizeBytes": 1, "sha256": H1, "source": "local", **fields}
    ]
    return manifest


def _sig_file(kid: str, sig: str) -> str:
    return json.dumps({"kid": kid, "sig": sig})


def build_manifest_cases(test, other) -> list:
    cases = []

    def case(name, manifest_bytes, signature, *, error=None, trusted=(test,), device=DEVICE):
        entry = {
            "name": name,
            "manifest": manifest_bytes.decode("utf-8"),
            "signature": signature,
            "trusted": [_pub(signer) for signer in trusted],
            "device_id": device,
        }
        if error is None:
            parsed = json.loads(manifest_bytes)
            entry["expect"] = {
                "package_id": parsed["packageId"],
                "placeholder": parsed["signature"]["placeholder"],
                "files": len(parsed["files"]),
                "references": len(parsed["references"]),
            }
        else:
            entry["error"] = error
        cases.append(entry)

    def signed(manifest, signer=test):
        raw = encode_manifest(manifest)
        return raw, sign_manifest(signer, raw)

    good, good_sig = signed(_manifest())
    good_sig_value = json.loads(good_sig)["sig"]
    case("valid", good, good_sig)
    case("valid_second_trusted_key", *signed(_manifest(), other), trusted=(test, other))
    scan = _manifest(signature={"signer": "scan", "placeholder": False, "documents": []})
    case("valid_scan_signer_without_placeholder", *signed(scan))
    case("valid_copy_episodes_no_references", *signed(_manifest(copyEpisodes=True, references=[])))

    # ---- malformed：manifest.sig 本身不对 ----
    case("malformed_sig_not_json", good, "not json", error="malformed")
    case("malformed_sig_missing_kid", good, json.dumps({"sig": good_sig_value}), error="malformed")
    case("malformed_sig_extra_field", good,
         json.dumps({"kid": test.kid, "sig": good_sig_value, "alg": "none"}), error="malformed")
    case("malformed_sig_duplicate_key", good,
         '{"kid": "%s", "sig": "%s", "sig": "%s"}' % (test.kid, good_sig_value, good_sig_value),
         error="malformed")
    case("malformed_sig_bad_charset", good, _sig_file(test.kid, good_sig_value[:-2] + "+/"), error="malformed")
    case("malformed_sig_padded", good, _sig_file(test.kid, good_sig_value + "=="), error="malformed")
    case("malformed_sig_too_short", good, _sig_file(test.kid, good_sig_value[:-4]), error="malformed")
    case("malformed_sig_non_canonical", good, _sig_file(test.kid, _non_canonical(good_sig_value)),
         error="malformed")
    case("malformed_sig_empty", good, _sig_file(test.kid, ""), error="malformed")
    # ---- bad_signature：签名验不过，或者签名不是给清单用的 ----
    case("bad_signature_tampered_title", good.replace("测试剧".encode(), "别的剧".encode()), good_sig,
         error="bad_signature")
    case("bad_signature_trailing_newline", good + b"\n", good_sig, error="bad_signature")
    swapped = encode_manifest(_manifest(deviceId=OTHER_DEVICE))
    case("bad_signature_device_swapped_after_signing", swapped, good_sig, error="bad_signature",
         device=OTHER_DEVICE)
    case("bad_signature_untrusted_key", *signed(_manifest(), other), error="bad_signature")
    case("bad_signature_no_trusted_keys", good, good_sig, error="bad_signature", trusted=())
    case("bad_signature_kid_mismatch", good, _sig_file(other.kid, good_sig_value), error="bad_signature",
         trusted=(test, other))
    license_sig = test.sign_bytes(good).split(".")[1]
    case("bad_signature_license_purpose_reused", good, _sig_file(test.kid, license_sig), error="bad_signature")
    recipe_sig = test.sign_detached("recipe", good)
    case("bad_signature_recipe_purpose_reused", good, _sig_file(test.kid, recipe_sig), error="bad_signature")
    case("bad_signature_flipped_bit", good,
         _sig_file(test.kid, lt.b64url_encode(bytes([lt.b64url_decode(good_sig_value)[0] ^ 1])
                                              + lt.b64url_decode(good_sig_value)[1:])),
         error="bad_signature")

    # ---- bad_payload：签名没问题，内容不认 ----
    def raw_case(name, raw: bytes, device=DEVICE):
        case(name, raw, sign_manifest(test, raw), error="bad_payload", device=device)

    def payload_case(name, manifest, device=DEVICE):
        raw_case(name, encode_manifest(manifest), device)

    payload_case("bad_payload_old_schema", _manifest(schema="drama-dossier/package@1"))
    no_schema = _manifest()
    del no_schema["schema"]
    payload_case("bad_payload_schema_missing", no_schema)
    no_user = _manifest()
    del no_user["userId"]
    payload_case("bad_payload_field_missing", no_user)
    payload_case("bad_payload_unknown_field", _manifest(extra=True))
    payload_case("bad_payload_empty_package_id", _manifest(packageId=""))
    # 前面塞一个 deviceId：宽松的解析器会取后一个（或前一个），严格解析必须整份拒绝。
    duplicate = encode_manifest(_manifest()).replace(b"{", b'{\n  "deviceId": "%s",' % OTHER_DEVICE.encode(), 1)
    raw_case("bad_payload_duplicate_device_key", duplicate)
    raw_case("bad_payload_not_json", b"not json")
    raw_case("bad_payload_top_level_array", b"[]")
    for label, path in [
        ("parent_dir", "../evil.txt"),
        ("nested_parent_dir", "06_分集视频/../../evil.txt"),
        ("absolute", "/etc/passwd"),
        ("backslash", "06_分集视频\\evil.txt"),
        ("drive_letter", "C:/evil.txt"),
        ("empty_segment", "06_分集视频//a.txt"),
        ("dot_segment", "./a.txt"),
        ("trailing_dot", "06_分集视频./a.txt"),
        ("trailing_space", "06_分集视频 /a.txt"),
        ("control_char", "06_分集视频/a\nb.txt"),
        ("too_long", "a" * 513),
    ]:
        payload_case(f"bad_payload_path_{label}", _with_file(path))
    payload_case("bad_payload_duplicate_path_case_insensitive", _with_file("01_剧集信息/剧名.TXT"))
    payload_case("bad_payload_uppercase_hash", _with_file("06_分集视频/x.mp4", sha256="A" * 64))
    payload_case("bad_payload_short_hash", _with_file("06_分集视频/x.mp4", sha256="1" * 63))
    payload_case("bad_payload_unknown_source", _with_file("06_分集视频/x.mp4", source="remote"))
    payload_case("bad_payload_negative_size", _with_file("06_分集视频/x.mp4", sizeBytes=-1))
    payload_case("bad_payload_reference_with_dir",
                 _manifest(references=[{"path": "子目录/第01集.mp4", "sizeBytes": 30, "sha256": H2}]))
    payload_case("bad_payload_reference_parent_dir",
                 _manifest(references=[{"path": "..", "sizeBytes": 30, "sha256": H2}]))
    payload_case("bad_payload_signed_document_not_in_files",
                 _manifest(signature={"signer": "mock", "placeholder": True, "documents": ["不存在.pdf"]}))
    payload_case("bad_payload_checked_before_wrong_device", _with_file("../evil.txt"), device=OTHER_DEVICE)

    # ---- wrong_device ----
    case("wrong_device", good, good_sig, error="wrong_device", device=OTHER_DEVICE)
    case("wrong_device_case_sensitive", good, good_sig, error="wrong_device", device=DEVICE.upper())
    return cases


def _recipe(signer, **overrides) -> dict:
    payload = {
        "v": 1,
        "kid": signer.kid,
        "rid": RECIPE_ID,
        "sub": USER,
        "dev": DEVICE,
        "task": TASK,
        "platform": "douyin",
        "strength": "medium",
        "seed": 12345,
        "params": {"platform": {"width": 1080}, "subtle": {"grain": 4}},
        "iat": NOW,
        "exp": EXP,
    }
    payload.update(overrides)
    return payload


def build_recipe_cases(test, other) -> list:
    cases = []

    def case(name, token, *, error=None, trusted=(test,), device=DEVICE, task=TASK, now=NOW, expect=None):
        entry = {
            "name": name,
            "token": token,
            "trusted": [_pub(signer) for signer in trusted],
            "device_id": device,
            "task": task,
            "now": now,
        }
        if error is None:
            entry["expect"] = expect
        else:
            entry["error"] = error
        cases.append(entry)

    def expect_of(payload):
        return {key: payload[key] for key in
                ("kid", "rid", "sub", "dev", "task", "platform", "strength", "seed", "params", "iat", "exp")}

    def sign(payload, signer=test):
        return signer.sign_envelope("recipe", payload)

    base = _recipe(test)
    good = sign(base)
    payload_part, sig_part = good.split(".")
    case("valid", good, expect=expect_of(base))
    case("valid_just_before_expiry", good, now=EXP - 1, expect=expect_of(base))
    by_other = _recipe(other)
    case("valid_second_trusted_key", sign(by_other, other), trusted=(test, other), expect=expect_of(by_other))
    for platform in ("kuaishou", "channels"):
        for strength in ("light", "heavy"):
            payload = _recipe(test, platform=platform, strength=strength)
            case(f"valid_{platform}_{strength}", sign(payload), expect=expect_of(payload))
    extra = _recipe(test, future_field=[1, 2])
    case("valid_unknown_field_ignored", sign(extra), expect=expect_of(base))
    # ---- malformed ----
    case("malformed_no_dot", payload_part + sig_part, error="malformed")
    case("malformed_two_dots", good + "." + sig_part, error="malformed")
    case("malformed_empty_payload", "." + sig_part, error="malformed")
    case("malformed_empty_signature", payload_part + ".", error="malformed")
    case("malformed_padded_payload", payload_part + "=." + sig_part, error="malformed")
    case("malformed_bad_charset_signature", payload_part + "." + sig_part[:-2] + "+/", error="malformed")
    case("malformed_short_signature", payload_part + "." + sig_part[:-4], error="malformed")
    case("malformed_non_canonical_signature", payload_part + "." + _non_canonical(sig_part), error="malformed")
    case("malformed_empty_string", "", error="malformed")

    # ---- bad_signature ----
    tampered = lt.b64url_encode(lt.canonical_payload(_recipe(test, strength="heavy")))
    case("bad_signature_payload_swapped", tampered + "." + sig_part, error="bad_signature")
    case("bad_signature_untrusted_key", sign(_recipe(other), other), error="bad_signature")
    case("bad_signature_no_trusted_keys", good, trusted=(), error="bad_signature")
    raw = lt.canonical_payload(base)
    case("bad_signature_license_purpose_reused", test.sign_bytes(raw), error="bad_signature")
    case("bad_signature_manifest_purpose_reused",
         lt.b64url_encode(raw) + "." + test.sign_detached("package-manifest", raw), error="bad_signature")

    # ---- bad_payload：签名没问题，内容不认 ----
    def payload_case(name, payload, **kwargs):
        case(name, sign(payload), error="bad_payload", **kwargs)

    def raw_payload_case(name, raw_bytes):
        case(name, lt.b64url_encode(raw_bytes) + "." + test.sign_detached("recipe", raw_bytes),
             error="bad_payload")

    payload_case("bad_payload_version_2", _recipe(test, v=2))
    payload_case("bad_payload_version_bool", _recipe(test, v=True))
    payload_case("bad_payload_kid_not_signer", _recipe(test, kid=other.kid))
    payload_case("bad_payload_unknown_platform", _recipe(test, platform="bilibili"))
    payload_case("bad_payload_unknown_strength", _recipe(test, strength="extreme"))
    payload_case("bad_payload_params_not_object", _recipe(test, params=[1]))
    payload_case("bad_payload_seed_string", _recipe(test, seed="12345"))
    payload_case("bad_payload_seed_float", _recipe(test, seed=1.5))
    payload_case("bad_payload_seed_negative", _recipe(test, seed=-1))
    payload_case("bad_payload_seed_too_large", _recipe(test, seed=2**63))
    payload_case("bad_payload_exp_not_after_iat", _recipe(test, exp=NOW))
    payload_case("bad_payload_empty_rid", _recipe(test, rid=""))
    payload_case("bad_payload_task_not_string", _recipe(test, task=1))
    for field in ("rid", "sub", "dev", "task", "params", "iat", "exp", "seed"):
        missing = _recipe(test)
        del missing[field]
        payload_case(f"bad_payload_missing_{field}", missing)
    duplicate = lt.canonical_payload(base).replace(b'"dev":', b'"dev":"%s","dev":' % OTHER_DEVICE.encode(), 1)
    raw_payload_case("bad_payload_duplicate_dev_key", duplicate)
    raw_payload_case("bad_payload_not_json", b"not json")
    raw_payload_case("bad_payload_top_level_array", b"[]")
    payload_case("bad_payload_checked_before_wrong_device", _recipe(test, v=2), device=OTHER_DEVICE)

    # ---- 机器码、任务、有效期（按这个顺序检查） ----
    case("wrong_device", good, device=OTHER_DEVICE, error="wrong_device")
    case("wrong_device_case_sensitive", good, device=DEVICE.upper(), error="wrong_device")
    case("wrong_device_checked_before_task", good, device=OTHER_DEVICE, task=OTHER_TASK, error="wrong_device")
    case("wrong_task", good, task=OTHER_TASK, error="wrong_task")
    case("wrong_task_checked_before_expiry", good, task=OTHER_TASK, now=EXP + 1, error="wrong_task")
    case("expired_at_exact_expiry", good, now=EXP, error="expired")
    case("expired_after_expiry", good, now=EXP + 1, error="expired")
    return cases


def build_vectors() -> dict:
    test = lt.Signer.from_seed(TEST_SEED)
    other = lt.Signer.from_seed(OTHER_SEED)
    return {
        "format": "drama-dossier package manifest & recipe v1 test vectors",
        "warning": "TEST ONLY：这里的种子是公开的，任何人都能用它签名。正式环境绝不能信任这里的公钥。",
        "contexts": {purpose: context.decode("ascii") for purpose, context in lt.PURPOSE_CONTEXTS.items()},
        "manifest_check_order": ["malformed", "bad_signature", "bad_payload", "wrong_device"],
        "recipe_check_order": ["malformed", "bad_signature", "bad_payload", "wrong_device", "wrong_task", "expired"],
        "manifest_cases": build_manifest_cases(test, other),
        "recipe_cases": build_recipe_cases(test, other),
    }


def _dump(vectors: dict) -> str:
    return json.dumps(vectors, ensure_ascii=False, indent=2, sort_keys=True) + "\n"


def _load() -> dict:
    if not VECTORS_PATH.exists():
        return {"manifest_cases": [], "recipe_cases": []}
    return json.loads(VECTORS_PATH.read_text(encoding="utf-8"))


def test_vectors_file_matches_generator():
    expected = _dump(build_vectors())
    if os.environ.get("UPDATE_SIGNED_VECTORS") == "1":
        VECTORS_PATH.write_text(expected, encoding="utf-8")
    assert VECTORS_PATH.exists(), "测试向量文件不存在，用 UPDATE_SIGNED_VECTORS=1 生成"
    assert VECTORS_PATH.read_text(encoding="utf-8") == expected, (
        "测试向量和生成器对不上；改了格式就用 UPDATE_SIGNED_VECTORS=1 重新生成，并同步桌面端"
    )


def test_vectors_cover_every_error_code():
    doc = _load()
    manifest_codes = {case.get("error") for case in doc["manifest_cases"]}
    recipe_codes = {case.get("error") for case in doc["recipe_cases"]}
    assert MANIFEST_ERRORS | {None} == manifest_codes
    assert RECIPE_ERRORS | {None} == recipe_codes
    for kind in ("manifest_cases", "recipe_cases"):
        names = [case["name"] for case in doc[kind]]
        assert len(names) == len(set(names)), f"{kind} 里有重名用例"


def _signing_key(trusted: list, kid: str) -> str:
    """在受信公钥里找出实际签名的那一把（按 kid 对应，不按位置猜）。"""
    [key] = [pub for pub in trusted if lt.key_id(lt.b64url_decode(pub)) == kid]
    return key


def _verifies(public_b64: str, purpose: str, payload: bytes, signature_b64: str) -> bool:
    try:
        Ed25519PublicKey.from_public_bytes(lt.b64url_decode(public_b64)).verify(
            lt.b64url_decode(signature_b64), lt.PURPOSE_CONTEXTS[purpose] + payload
        )
    except InvalidSignature:
        return False
    return True


def _valid_manifests():
    return [case for case in _load()["manifest_cases"] if "expect" in case]


def _valid_recipes():
    return [case for case in _load()["recipe_cases"] if "expect" in case]


@pytest.mark.parametrize("case", _valid_manifests(), ids=lambda case: case["name"])
def test_manifest_signature_only_valid_for_its_purpose(case):
    payload = case["manifest"].encode("utf-8")
    signature = json.loads(case["signature"])
    sig = signature["sig"]
    key = _signing_key(case["trusted"], signature["kid"])
    assert _verifies(key, "package-manifest", payload, sig)
    assert not _verifies(key, "license", payload, sig)
    assert not _verifies(key, "recipe", payload, sig)


@pytest.mark.parametrize("case", _valid_recipes(), ids=lambda case: case["name"])
def test_recipe_signature_only_valid_for_its_purpose(case):
    payload_part, sig = case["token"].split(".")
    payload = lt.b64url_decode(payload_part)
    key = _signing_key(case["trusted"], case["expect"]["kid"])
    assert _verifies(key, "recipe", payload, sig)
    assert not _verifies(key, "license", payload, sig)
    assert not _verifies(key, "package-manifest", payload, sig)
    # 授权凭证的验签器也不认配方：格式一样（载荷.签名），靠用途前缀挡住。
    with pytest.raises(lt.LicenseError) as caught:
        lt.verify(case["token"], [lt.b64url_decode(k) for k in case["trusted"]], case["device_id"], case["now"])
    assert caught.value.code == "bad_signature"
