import json

import pytest
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

from dd_license.keys import b64url, b64url_decode, key_id, public_raw
from dd_license.token import SIGNING_CONTEXT, Claims, TokenError, canonical_json, parse_payload, sign, verify

from .conftest import OTHER_SEED, TEST_SEED, mid


def _claims(key, **kw):
    base = dict(kid=key_id(public_raw(key.public_key())), lid=1, mid=mid(1), plan="month", until=2000, iat=1000, exp=1500)
    base.update(kw)
    return Claims(**base)


@pytest.fixture
def k():
    return Ed25519PrivateKey.from_private_bytes(TEST_SEED)


def test_round_trip(k):
    t = sign(k, _claims(k))
    c = verify(t, [k.public_key()])
    assert c.mid == mid(1) and c.plan == "month" and c.exp == 1500


def test_payload_is_canonical(k):
    t = sign(k, _claims(k))
    payload = b64url_decode(t.split(".")[0])
    assert payload == canonical_json(json.loads(payload))
    assert payload.startswith(b'{"exp":')


def test_wrong_key_rejected(k):
    other = Ed25519PrivateKey.from_private_bytes(OTHER_SEED)
    t = sign(k, _claims(k))
    with pytest.raises(TokenError):
        verify(t, [other.public_key()])


def test_key_rotation_second_key_accepted(k):
    other = Ed25519PrivateKey.from_private_bytes(OTHER_SEED)
    t = sign(other, _claims(other))
    assert verify(t, [k.public_key(), other.public_key()]).kid == key_id(public_raw(other.public_key()))


def test_kid_must_match_signing_key(k):
    other = Ed25519PrivateKey.from_private_bytes(OTHER_SEED)
    t = sign(k, _claims(k, kid=key_id(public_raw(other.public_key()))))
    with pytest.raises(TokenError, match="kid"):
        verify(t, [k.public_key()])


def _resign(k, obj_bytes):
    return b64url(obj_bytes) + "." + b64url(k.sign(SIGNING_CONTEXT + obj_bytes))


@pytest.mark.parametrize(
    "mutate",
    [
        lambda o: {**o, "extra": 1},
        lambda o: {k: v for k, v in o.items() if k != "exp"},
        lambda o: {**o, "v": 2},
        lambda o: {**o, "plan": "forever"},
        lambda o: {**o, "exp": o["iat"]},
        lambda o: {**o, "lid": True},
        lambda o: {**o, "lid": -1},
        lambda o: {**o, "mid": "ABC"},
        lambda o: {**o, "plan": "lifetime"},  # until 不为 null
        lambda o: {**o, "until": None},  # 非永久却没有 until
    ],
)
def test_bad_payloads_rejected_even_if_signed(k, mutate):
    good = json.loads(b64url_decode(sign(k, _claims(k)).split(".")[0]))
    t = _resign(k, canonical_json(mutate(good)))
    with pytest.raises(TokenError):
        verify(t, [k.public_key()])


def test_duplicate_keys_rejected(k):
    kid = key_id(public_raw(k.public_key()))
    raw = (
        '{"exp":1500,"iat":1000,"kid":"%s","lid":1,"mid":"%s","plan":"month","plan":"lifetime","until":2000,"v":1}'
        % (kid, mid(1))
    ).encode()
    with pytest.raises(TokenError, match="重复"):
        verify(_resign(k, raw), [k.public_key()])


@pytest.mark.parametrize("bad", ["", "a", "a.b.c", "abc.def", "=.=", "x" * 3000])
def test_malformed(k, bad):
    with pytest.raises(TokenError):
        verify(bad, [k.public_key()])


def test_padded_base64_rejected(k):
    t = sign(k, _claims(k))
    p, s = t.split(".")
    with pytest.raises(TokenError):
        verify(p + "==." + s, [k.public_key()])


def test_tampered_signature(k):
    t = sign(k, _claims(k))
    p, s = t.split(".")
    sig = bytearray(b64url_decode(s))
    sig[0] ^= 1
    with pytest.raises(TokenError, match="签名"):
        verify(p + "." + b64url(bytes(sig)), [k.public_key()])


@pytest.mark.parametrize("v", [1.0, True, "1", 2])
def test_version_must_be_integer_one(v):
    obj = {"exp": 2, "iat": 1, "kid": "0" * 16, "lid": 1, "mid": "a" * 64, "plan": "lifetime", "until": None, "v": v}
    with pytest.raises(TokenError):
        parse_payload(canonical_json(obj))
