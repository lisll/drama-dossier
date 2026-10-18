"""启动时的 TLS 校验（M5b-6）。

重点不是「能用 https 起来」，而是**该失败的时候一定失败**：
「以为开了 TLS、实际是明文」比「明确知道没开」危险得多。
"""

from __future__ import annotations

import pytest

from drama_cloud.tls import TlsConfigError, require_tls_from_env, resolve_tls


@pytest.fixture()
def cert_and_key(tmp_path):
    """两个占位文件。这里只测「文件在不在、能不能读」，不测证书内容。"""
    cert = tmp_path / "server.crt"
    key = tmp_path / "server.key"
    cert.write_text("placeholder cert", encoding="utf-8")
    key.write_text("placeholder key", encoding="utf-8")
    return cert, key


def test_both_given_is_accepted(cert_and_key):
    cert, key = cert_and_key
    config = resolve_tls(str(cert), str(key))
    assert config is not None
    assert config.cert == cert and config.key == key
    assert config.uvicorn_kwargs() == {"ssl_certfile": str(cert), "ssl_keyfile": str(key)}


def test_neither_given_means_plaintext(cert_and_key):
    assert resolve_tls(None, None) is None
    assert resolve_tls("", "") is None
    assert resolve_tls("   ", "  ") is None


def test_only_the_cert_is_an_error(cert_and_key):
    """半开最危险：看起来配了 TLS，其实服务仍在明文跑。"""
    cert, _ = cert_and_key
    with pytest.raises(TlsConfigError) as caught:
        resolve_tls(str(cert), None)
    assert "--tls-key" in str(caught.value)


def test_only_the_key_is_an_error(cert_and_key):
    _, key = cert_and_key
    with pytest.raises(TlsConfigError) as caught:
        resolve_tls(None, str(key))
    assert "--tls-cert" in str(caught.value)


def test_a_missing_cert_file_is_an_error(tmp_path, cert_and_key):
    _, key = cert_and_key
    with pytest.raises(TlsConfigError) as caught:
        resolve_tls(str(tmp_path / "nope.crt"), str(key))
    assert "不存在" in str(caught.value)


def test_a_missing_key_file_is_an_error(tmp_path, cert_and_key):
    cert, _ = cert_and_key
    with pytest.raises(TlsConfigError) as caught:
        resolve_tls(str(cert), str(tmp_path / "nope.key"))
    assert "不存在" in str(caught.value)


def test_a_directory_instead_of_a_file_is_an_error(tmp_path, cert_and_key):
    _, key = cert_and_key
    directory = tmp_path / "adir"
    directory.mkdir()
    with pytest.raises(TlsConfigError) as caught:
        resolve_tls(str(directory), str(key))
    assert "不是文件" in str(caught.value)


def test_required_tls_without_files_is_an_error():
    """明确要求 TLS 却没给凭证：拒绝以明文启动。"""
    with pytest.raises(TlsConfigError) as caught:
        resolve_tls(None, None, required=True)
    assert "拒绝以明文启动" in str(caught.value)


def test_required_tls_with_files_is_fine(cert_and_key):
    cert, key = cert_and_key
    assert resolve_tls(str(cert), str(key), required=True) is not None


@pytest.mark.parametrize("value", ["1", "true", "TRUE", "yes", "on", " On "])
def test_truthy_require_tls_values(value):
    assert require_tls_from_env(value) is True


@pytest.mark.parametrize("value", ["0", "false", "no", "off", "", None])
def test_falsy_require_tls_values(value):
    assert require_tls_from_env(value) is False


def test_an_unrecognised_value_is_an_error():
    """写错成 "ture" 时不能静默当成「没要求」——那会让人以为配好了。"""
    with pytest.raises(TlsConfigError) as caught:
        require_tls_from_env("ture")
    assert "无法识别" in str(caught.value)
