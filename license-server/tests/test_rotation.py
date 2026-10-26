"""计划内密钥轮换：服务端换新私钥后，老用户手里旧私钥签的凭证仍能续签，拿到新私钥签的凭证。"""

import pytest
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

from dd_license import cli
from dd_license.errors import LicenseError
from dd_license.keys import b64url, generate_private_key, public_raw
from dd_license.service import LicenseService
from dd_license.token import verify

from .conftest import OTHER_SEED, make_settings, mid


def test_refresh_old_token_after_rotation(db, key):
    old = LicenseService(db, key)
    code = old.create_batch("year", 1, "", "boss")[1][0]
    old_token = old.activate(code, mid(1)).token

    new_key = Ed25519PrivateKey.from_private_bytes(OTHER_SEED)
    without = LicenseService(db, new_key)
    with pytest.raises(LicenseError):
        without.refresh(old_token, mid(1))

    rotated = LicenseService(db, new_key, extra_trusted=[key.public_key()])
    fresh = rotated.refresh(old_token, mid(1)).token
    assert verify(fresh, [new_key.public_key()]).mid == mid(1)
    with pytest.raises(Exception):
        verify(fresh, [key.public_key()])


def test_build_app_reads_extra_pubkeys(tmp_path, key):
    s = make_settings(tmp_path)
    generate_private_key(s.key_file)
    s.extra_pubkeys = b64url(public_raw(key.public_key()))
    app = cli.build_app(s)
    assert len(app.state.service.trusted) == 2
