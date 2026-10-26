import os
import stat

import pytest

from dd_license import cli
from dd_license.config import ConfigError
from dd_license.keys import generate_private_key, load_private_key

from .conftest import make_settings


def test_keygen_refuses_overwrite_and_sets_mode(tmp_path):
    p = tmp_path / "k" / "key.pem"
    generate_private_key(p)
    assert stat.S_IMODE(p.stat().st_mode) == 0o600
    with pytest.raises(ConfigError):
        generate_private_key(p)
    load_private_key(p)


def test_loose_key_permissions_refused(tmp_path):
    p = tmp_path / "key.pem"
    generate_private_key(p)
    os.chmod(p, 0o644)
    with pytest.raises(ConfigError, match="权限过宽"):
        load_private_key(p)


def test_doctor_flags_problems(tmp_path):
    s = make_settings(tmp_path, admin_allow_ips=[], admin_allow_any_ip=True, trust_proxy=False)
    generate_private_key(s.key_file)
    os.chmod(s.key_file, 0o644)
    results = dict((msg, ok) for ok, msg in cli.doctor(s, host="0.0.0.0"))
    failed = [m for m, ok in results.items() if not ok]
    assert any("私钥权限" in m for m in failed)
    assert any("白名单" in m for m in failed)
    assert any("ALLOW_ANY_IP" in m for m in failed)
    assert any("只监听本机" in m for m in failed)
    assert any("TRUST_PROXY" in m for m in failed)


def test_doctor_passes_when_hardened(tmp_path, monkeypatch):
    from dd_license.admin_auth import AdminAuth
    from dd_license.db import Database

    s = make_settings(tmp_path, cookie_secure=True)
    s.db_path = tmp_path / "data" / "x.sqlite3"
    generate_private_key(s.key_file)
    Database(s.db_path).migrate()
    os.chmod(s.db_path.parent, 0o700)
    AdminAuth(Database(s.db_path)).create_admin("boss", "Correct-Horse-Battery-Staple-42")
    monkeypatch.setattr(os, "geteuid", lambda: 1000)
    failed = [msg for ok, msg in cli.doctor(s) if not ok]
    assert failed == []
