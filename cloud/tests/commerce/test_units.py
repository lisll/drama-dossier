"""不需要数据库的单元测试：配置、令牌与验证码摘要、请求校验、SMTP 发信、迁移文件发现。"""

from __future__ import annotations

import dataclasses
import smtplib
import ssl
import threading
import time
from email import message_from_string

import pytest
from pydantic import ValidationError

from drama_cloud.commerce import db, security
from drama_cloud.commerce.accounts import AccountService
from drama_cloud.commerce.config import ConfigError, SmtpSettings, load_settings
from drama_cloud.commerce.mailer import MailError, SmtpMailer
from drama_cloud.commerce.schemas import LoginRequest, RegisterRequest, SendCodeRequest

from .conftest import MemoryMailer

SECRET = "s" * 40
BASE_ENV = {
    "DRAMA_COMMERCE_DSN": "postgresql://u:p@db.internal/x",
    "DRAMA_COMMERCE_CODE_SECRET": SECRET,
    "DRAMA_COMMERCE_SMTP_HOST": "smtp.example.com",
}


# ---------- 配置 ----------


def test_settings_defaults():
    settings = load_settings(BASE_ENV)
    assert settings.smtp.security == "starttls" and settings.smtp.port == 587
    assert settings.smtp.sender == "no-reply@smtp.example.com"
    assert settings.trusted_proxies == 0 and settings.hash_concurrency == 4
    # 密钥和连接串不出现在 repr 里，免得被打进日志。
    text = repr(settings)
    assert SECRET not in text and "u:p@" not in text


@pytest.mark.parametrize(
    "override, fragment",
    [
        ({"DRAMA_COMMERCE_DSN": ""}, "DRAMA_COMMERCE_DSN"),
        ({"DRAMA_COMMERCE_CODE_SECRET": "short"}, "CODE_SECRET"),
        ({"DRAMA_COMMERCE_SMTP_HOST": ""}, "SMTP_HOST"),
        ({"DRAMA_COMMERCE_SMTP_SECURITY": "tls13"}, "SMTP_SECURITY"),
        ({"DRAMA_COMMERCE_SMTP_SECURITY": "plain"}, "只允许连接本机"),
        ({"DRAMA_COMMERCE_SMTP_PORT": "abc"}, "SMTP_PORT"),
        ({"DRAMA_COMMERCE_SMTP_PORT": "70000"}, "SMTP_PORT"),
        ({"DRAMA_COMMERCE_TRUSTED_PROXIES": "9"}, "TRUSTED_PROXIES"),
        ({"DRAMA_COMMERCE_HASH_CONCURRENCY": "0"}, "HASH_CONCURRENCY"),
    ],
)
def test_settings_reject_bad_config(override, fragment):
    with pytest.raises(ConfigError) as caught:
        load_settings({**BASE_ENV, **override})
    message = str(caught.value)
    assert fragment in message
    assert SECRET not in message and "u:p@" not in message


@pytest.mark.parametrize("host", ["127.0.0.1", "localhost", "::1"])
def test_plain_smtp_allowed_only_on_loopback(host):
    env = {**BASE_ENV, "DRAMA_COMMERCE_SMTP_HOST": host, "DRAMA_COMMERCE_SMTP_SECURITY": "PLAIN"}
    settings = load_settings(env)
    assert settings.smtp.security == "plain" and settings.smtp.port == 25


def test_ssl_default_port():
    settings = load_settings({**BASE_ENV, "DRAMA_COMMERCE_SMTP_SECURITY": "ssl"})
    assert settings.smtp.port == 465


# ---------- 令牌与验证码 ----------


def test_code_digest_binds_secret_purpose_and_email():
    base = security.code_digest(SECRET, "register", "a@example.com", "123456")
    assert len(base) == 32
    assert base == security.code_digest(SECRET, "register", "a@example.com", "123456")
    assert base != security.code_digest(SECRET, "reset_password", "a@example.com", "123456")
    assert base != security.code_digest(SECRET, "register", "b@example.com", "123456")
    assert base != security.code_digest("t" * 40, "register", "a@example.com", "123456")
    assert base != security.code_digest(SECRET, "register", "a@example.com", "123457")


def test_tokens_and_codes_shape():
    access, refresh = security.new_access_token(), security.new_refresh_token()
    assert access.startswith("at_") and refresh.startswith("rt_") and len(access) > 40
    assert security.token_digest(access) != security.token_digest(refresh)
    samples = [security.new_code() for _ in range(200)]
    assert all(len(code) == 6 and code.isdigit() for code in samples)
    assert len(set(samples)) > 150


def test_privacy_key_hides_raw_value():
    key = security.privacy_key(SECRET, "203.0.113.7")
    assert "203.0.113.7" not in key and len(key) == 32
    assert key == security.privacy_key(SECRET, "203.0.113.7")
    assert key != security.privacy_key(SECRET, "203.0.113.8")


# ---------- 密码哈希并发上限 ----------


class SlowHasher:
    """记录同时有几个哈希在跑的假哈希器。"""

    def __init__(self):
        self.lock = threading.Lock()
        self.active = 0
        self.peak = 0

    def _work(self):
        with self.lock:
            self.active += 1
            self.peak = max(self.peak, self.active)
        time.sleep(0.1)
        with self.lock:
            self.active -= 1

    def hash(self, password):
        self._work()
        return "$fake$" + password

    def verify(self, password_hash, password):
        self._work()
        return password_hash == "$fake$" + password


def test_password_hashing_concurrency_is_bounded():
    settings = dataclasses.replace(load_settings(BASE_ENV), hash_concurrency=2)
    hasher = SlowHasher()
    service = AccountService(settings, MemoryMailer(), hasher)
    hasher.peak = 0  # 去掉构造时那次哈希
    workers = [threading.Thread(target=service._hash, args=("pw",)) for _ in range(3)]
    workers += [threading.Thread(target=service._verify, args=("$fake$pw", "pw")) for _ in range(3)]
    for worker in workers:
        worker.start()
    for worker in workers:
        worker.join()
    assert hasher.peak == 2


# ---------- 请求校验 ----------


def test_email_is_normalized():
    assert SendCodeRequest(email="  User@Example.COM ", purpose="register").email == "user@example.com"


@pytest.mark.parametrize(
    "email", ["no-at-sign", "a@b", "a@@b.com", "a b@c.com", "用户@例子.中国", "x" * 250 + "@a.com"]
)
def test_bad_emails_rejected(email):
    with pytest.raises(ValidationError):
        SendCodeRequest(email=email, purpose="register")


def test_unknown_purpose_rejected():
    with pytest.raises(ValidationError):
        SendCodeRequest(email="a@example.com", purpose="login")


GOOD_REGISTER = {"email": "a@example.com", "code": "012345", "password": "longenough", "accept_terms": True}


@pytest.mark.parametrize(
    "override",
    [
        {"accept_terms": False},
        {"accept_terms": "true"},
        {"accept_terms": 1},
        {"password": "short"},
        {"password": "x" * 129},
        {"code": "12345"},
        {"code": "12345a"},
        {"code": "１２３４５６"},
        {"invite_code": "abc"},
    ],
)
def test_register_request_rejects(override):
    with pytest.raises(ValidationError):
        RegisterRequest(**{**GOOD_REGISTER, **override})


def test_register_request_accepts_and_trims_code():
    request = RegisterRequest(**{**GOOD_REGISTER, "code": " 012345 "})
    assert request.code == "012345" and request.accept_terms is True


def test_login_has_no_minimum_length_but_caps_input():
    assert LoginRequest(email="a@example.com", password="old").password == "old"
    for password in ("", "x" * 129):
        with pytest.raises(ValidationError):
            LoginRequest(email="a@example.com", password=password)


# ---------- SMTP 发信 ----------


class FakeSMTP:
    instances: list = []

    def __init__(self, host, port, timeout=None, context=None):
        self.host, self.port, self.timeout, self.context = host, port, timeout, context
        self.calls: list = []
        self.tls_context = None
        self.messages: list = []
        FakeSMTP.instances.append(self)

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.calls.append("quit")
        return False

    def starttls(self, context=None):
        self.calls.append("starttls")
        self.tls_context = context

    def login(self, username, password):
        self.calls.append(("login", username, password))

    def send_message(self, message):
        self.calls.append("send")
        self.messages.append(message)


@pytest.fixture()
def fake_smtp(monkeypatch):
    FakeSMTP.instances = []
    monkeypatch.setattr(smtplib, "SMTP", FakeSMTP)
    monkeypatch.setattr(smtplib, "SMTP_SSL", FakeSMTP)
    return FakeSMTP


def test_starttls_login_and_utf8_message(fake_smtp):
    settings = SmtpSettings(
        host="smtp.example.com", port=587, security="starttls",
        sender="no-reply@example.com", username="mailer", password="smtp-pass",
    )
    SmtpMailer(settings).send("u@example.com", "【剧备】注册账号验证码", "你的验证码是：012345")
    client = fake_smtp.instances[0]
    assert (client.host, client.port) == ("smtp.example.com", 587)
    assert client.calls == ["starttls", ("login", "mailer", "smtp-pass"), "send", "quit"]
    assert isinstance(client.tls_context, ssl.SSLContext)
    message = client.messages[0]
    assert message["To"] == "u@example.com" and message["Subject"] == "【剧备】注册账号验证码"
    assert message["Content-Transfer-Encoding"] == "base64"
    assert message.get_content().strip() == "你的验证码是：012345"
    raw = message.as_string()
    assert "=?utf-8?b?" in raw.lower()  # 中文标题按 RFC 2047 编码
    assert message_from_string(raw)["To"] == "u@example.com"


def test_ssl_mode_verifies_certificates(fake_smtp):
    settings = SmtpSettings(host="smtp.example.com", port=465, security="ssl", sender="n@example.com")
    SmtpMailer(settings).send("u@example.com", "标题", "正文")
    client = fake_smtp.instances[0]
    assert client.context.verify_mode == ssl.CERT_REQUIRED and client.context.check_hostname
    assert client.calls == ["send", "quit"]


def test_plain_mode_skips_tls_and_login(fake_smtp):
    settings = SmtpSettings(host="127.0.0.1", port=1025, security="plain", sender="n@example.com")
    SmtpMailer(settings).send("u@example.com", "标题", "正文")
    assert fake_smtp.instances[0].calls == ["send", "quit"]


def test_smtp_failure_becomes_mail_error_without_details(monkeypatch):
    def refuse(*args, **kwargs):
        raise ConnectionRefusedError("connection refused by smtp.example.com")

    monkeypatch.setattr(smtplib, "SMTP", refuse)
    settings = SmtpSettings(
        host="smtp.example.com", port=587, security="starttls",
        sender="n@example.com", username="mailer", password="smtp-pass",
    )
    with pytest.raises(MailError) as caught:
        SmtpMailer(settings).send("victim@example.com", "标题", "验证码是：987654")
    message = str(caught.value)
    assert "ConnectionRefusedError" in message
    for secret in ("victim@example.com", "987654", "smtp-pass"):
        assert secret not in message


# ---------- 迁移文件 ----------


def test_bundled_migrations_are_discovered():
    migrations = db.discover()
    assert [item.version for item in migrations] == sorted(item.version for item in migrations)
    assert migrations[0].name == "0001_accounts.sql" and len(migrations[0].checksum) == 64


def test_bad_migration_name_rejected(tmp_path):
    (tmp_path / "1_bad.sql").write_text("SELECT 1;", encoding="utf-8")
    with pytest.raises(db.MigrationError, match="文件名"):
        db.discover(tmp_path)


def test_duplicate_migration_version_rejected(tmp_path):
    (tmp_path / "0001_a.sql").write_text("SELECT 1;", encoding="utf-8")
    (tmp_path / "0001_b.sql").write_text("SELECT 2;", encoding="utf-8")
    with pytest.raises(db.MigrationError, match="重复"):
        db.discover(tmp_path)
