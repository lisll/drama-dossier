"""密钥轮换（M7-5）。

总方案给的完成标准是「轮换后旧凭证在过渡期内仍然可用，过渡期结束后被拒绝」。
这条测试直接验这件事。

**只用 Python 侧验签**：Rust 与 Python 的用途前缀是否一致，由
`test_signing_purposes.py` 对拍；这里只管「两把私钥 / 两把公钥」的语义。
"""

from __future__ import annotations

import base64
import json

import pytest
from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey, Ed25519PrivateKey

from drama_cloud.commerce import license_token
from drama_cloud.commerce.license_token import Signer, b64url_encode  # noqa: F401

SEED_OLD = bytes(range(32))
SEED_NEW = bytes(range(32, 64))
RELEASE_CONTEXT = license_token.PURPOSE_CONTEXTS["release"]


def verify_with(public_key: bytes, purpose: str, payload: bytes, signature_b64: str) -> bool:
    """用一把公钥验一段字节；验不过返回 False（不抛）。"""
    try:
        signature = base64.urlsafe_b64decode(signature_b64 + "===")
        key = Ed25519PublicKey.from_public_bytes(public_key)
        key.verify(signature, license_token.PURPOSE_CONTEXTS[purpose] + payload)
        return True
    except (InvalidSignature, ValueError):
        return False


def signer_with_secondary() -> Signer:
    primary = Ed25519PrivateKey.from_private_bytes(SEED_OLD)
    secondary = Ed25519PrivateKey.from_private_bytes(SEED_NEW)
    return Signer(primary, secondary)


def test_a_signer_reports_both_public_keys():
    signer = signer_with_secondary()
    assert signer.public_key != signer.secondary_public_key
    assert signer.kid and signer.secondary_kid
    assert signer.kid != signer.secondary_kid


def test_the_primary_key_signs_by_default():
    signer = signer_with_secondary()
    payload = b"hello"
    signature = signer.sign_detached("release", payload)
    assert verify_with(signer.public_key, "release", payload, signature), "主公钥应当能验"
    assert not verify_with(
        signer.secondary_public_key, "release", payload, signature
    ), "次公钥不该能验主私钥的签名"


def test_the_secondary_key_can_sign_when_asked():
    """过渡期的机制：可以让次私钥去签，好让只认旧公钥的老客户端仍能验证。"""
    signer = signer_with_secondary()
    payload = b"hello"
    signature = signer.sign_detached("release", payload, with_secondary=True)
    assert verify_with(signer.secondary_public_key, "release", payload, signature)
    assert not verify_with(signer.public_key, "release", payload, signature)


def test_both_keys_verify_when_both_public_keys_are_trusted():
    """过渡期的真实形态：客户端同时信任两把公钥，所以两把签的都能验。"""
    signer = signer_with_secondary()
    payload = b"payload"
    from_primary = signer.sign_detached("release", payload)
    from_secondary = signer.sign_detached("release", payload, with_secondary=True)
    assert verify_with(signer.public_key, "release", payload, from_primary)
    assert verify_with(signer.secondary_public_key, "release", payload, from_secondary)


def test_a_retired_key_no_longer_verifies():
    """过渡期结束：客户端不再信任旧公钥，旧私钥签的东西就验不过了。"""
    signer = signer_with_secondary()
    payload = b"payload"
    old_signature = signer.sign_detached("release", payload)
    # 只用新公钥验旧私钥的签名 → 失败。
    assert not verify_with(signer.secondary_public_key, "release", payload, old_signature)
    # 而新私钥的签名仍然能验。
    new_signature = signer.sign_detached("release", payload, with_secondary=True)
    assert verify_with(signer.secondary_public_key, "release", payload, new_signature)


def test_asking_for_the_secondary_without_one_is_an_error():
    """不能悄悄退回主私钥：那会让人以为「签了新钥」，其实签的还是旧钥。"""
    signer = Signer.from_seed(SEED_OLD)
    with pytest.raises(Exception) as caught:
        signer.sign_detached("release", b"x", with_secondary=True)
    assert "次私钥" in str(caught.value)


def test_purpose_isolation_still_holds_across_keys():
    """即使两把公钥都信任，升级包的签名也不能冒充授权凭证。"""
    signer = signer_with_secondary()
    payload = b"payload"
    release_sig = signer.sign_detached("release", payload)
    assert verify_with(signer.public_key, "release", payload, release_sig)
    # 拿 release 的签名去当 license 验，必须失败。
    assert not verify_with(signer.public_key, "license", payload, release_sig)


def test_the_secondary_path_is_loaded_from_a_file(tmp_path):
    """真实用法：两把私钥各一个 PEM 文件。"""
    from drama_cloud.commerce.license_token import generate_key_file

    primary = tmp_path / "primary.pem"
    secondary = tmp_path / "secondary.pem"
    generate_key_file(str(primary))
    generate_key_file(str(secondary))

    signer = Signer.from_pem_file(primary, secondary_path=secondary)
    assert signer.secondary_public_key is not None
    assert signer.kid != signer.secondary_kid

    # 不传次私钥时只有主私钥。
    alone = Signer.from_pem_file(primary)
    assert alone.secondary_public_key is None


def test_the_two_keys_are_really_different(tmp_path):
    """两把密钥真的不同——否则「轮换」什么也没换。"""
    from drama_cloud.commerce.license_token import generate_key_file

    primary = tmp_path / "a.pem"
    secondary = tmp_path / "b.pem"
    generate_key_file(str(primary))
    generate_key_file(str(secondary))
    signer = Signer.from_pem_file(primary, secondary_path=secondary)
    assert signer.public_key != signer.secondary_public_key


def test_a_bad_secondary_file_is_reported_clearly(tmp_path):
    """次私钥文件不存在时要报清楚是哪一项配置，而不是笼统的「加载失败」。"""
    from drama_cloud.commerce.config import ConfigError
    from drama_cloud.commerce.license_token import generate_key_file

    primary = tmp_path / "ok.pem"
    generate_key_file(str(primary))
    with pytest.raises(ConfigError) as caught:
        Signer.from_pem_file(primary, secondary_path=tmp_path / "missing.pem")
    assert "missing.pem" in str(caught.value)
