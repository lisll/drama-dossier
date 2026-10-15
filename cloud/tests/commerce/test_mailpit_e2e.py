"""端到端：真实 SMTP 发信器 → 本机 Mailpit → 从 Mailpit 取出验证码 → 完成注册。

需要开发环境的 Mailpit（cloud/compose.dev.yaml）；设置 COMMERCE_TEST_MAILPIT=http://127.0.0.1:8025 才会运行。
"""

from __future__ import annotations

import json
import os
import time
import urllib.parse
import urllib.request

import pytest
from fastapi.testclient import TestClient

from drama_cloud.commerce.api import create_app
from drama_cloud.commerce.mailer import SmtpMailer

from .conftest import FAST_HASHER, extract_code

MAILPIT = os.environ.get("COMMERCE_TEST_MAILPIT", "").rstrip("/")
pytestmark = pytest.mark.skipif(not MAILPIT, reason="设置 COMMERCE_TEST_MAILPIT 才运行（需要本机 Mailpit）")


def _get(path: str) -> dict:
    with urllib.request.urlopen(MAILPIT + path, timeout=5) as response:
        return json.loads(response.read())


def _wait_for_message(to: str) -> dict:
    query = urllib.parse.quote(f"to:{to}")
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        found = _get(f"/api/v1/search?query={query}")
        if found["messages"]:
            return _get(f"/api/v1/message/{found['messages'][0]['ID']}")
        time.sleep(0.2)
    raise AssertionError(f"10 秒内 Mailpit 没有收到发给 {to} 的邮件")


def test_register_through_real_smtp(commerce_settings, pool, pg):
    email = f"e2e-{os.urandom(4).hex()}@example.test"
    mailer = SmtpMailer(commerce_settings.smtp)
    with TestClient(create_app(commerce_settings, pool=pool, mailer=mailer, hasher=FAST_HASHER)) as client:
        sent = client.post("/api/v1/auth/codes", json={"email": email, "purpose": "register"})
        assert sent.status_code == 202, sent.text
        message = _wait_for_message(email)
        assert message["Subject"] == "【剧备 DramaDossier】注册账号验证码"
        registered = client.post(
            "/api/v1/auth/register",
            json={"email": email, "code": extract_code(message["Text"]), "password": "correct-horse-1",
                  "accept_terms": True},
        )
    assert registered.status_code == 201, registered.text
