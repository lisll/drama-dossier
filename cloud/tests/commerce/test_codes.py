"""邮件验证码：冷却、过期、超次数、一次性、重发后旧码作废、发信失败可立即重试、按邮箱限流。"""

from __future__ import annotations

from drama_cloud.commerce import codes, security
from drama_cloud.commerce.api import SEND_CODE_PER_EMAIL

from .conftest import CODE_SECRET

EMAIL = "user@example.com"


def send(client, email=EMAIL, purpose="register"):
    return client.post("/api/v1/auth/codes", json={"email": email, "purpose": purpose})


def register_with(client, code, email=EMAIL):
    return client.post(
        "/api/v1/auth/register",
        json={"email": email, "code": code, "password": "correct-horse-1", "accept_terms": True},
    )


def age_codes(pg, seconds):
    """把验证码的时间整体往前拨，模拟过了这么多秒。"""
    pg.execute(
        "UPDATE email_codes SET created_at = created_at - make_interval(secs => %s::int),"
        " expires_at = expires_at - make_interval(secs => %s::int)",
        (seconds, seconds),
    )


def wrong(code):
    return f"{(int(code) + 1) % 1_000_000:06d}"


def test_send_code_mails_a_six_digit_code_and_stores_only_hmac(client, mailer, pg):
    response = send(client)
    assert response.status_code == 202
    assert response.json() == {"cooldown_seconds": 60, "expires_in": 600}
    message = mailer.sent[-1]
    assert message["to"] == EMAIL and message["subject"] == "【剧备 DramaDossier】注册账号验证码"
    code = mailer.last_code(EMAIL)
    assert code not in message["subject"]
    row = pg.execute("SELECT code_digest, attempts, purpose FROM email_codes").fetchone()
    assert bytes(row["code_digest"]) == security.code_digest(CODE_SECRET, "register", EMAIL, code)
    assert row["attempts"] == 0 and row["purpose"] == "register"


def test_resend_within_cooldown_is_rejected(client, mailer):
    assert send(client).status_code == 202
    again = send(client)
    assert again.status_code == 429 and again.json()["error"] == "rate_limited"
    assert 1 <= int(again.headers["Retry-After"]) <= 60
    assert "刚发过" in again.json()["message"]
    assert len(mailer.sent) == 1


def test_resend_after_cooldown_invalidates_old_code(client, mailer, pg):
    assert send(client).status_code == 202
    first = mailer.last_code(EMAIL)
    age_codes(pg, 61)
    assert send(client).status_code == 202
    second = mailer.last_code(EMAIL)
    if first != second:
        stale = register_with(client, first)
        assert stale.status_code == 400 and stale.json()["error"] == "code_invalid"
    assert register_with(client, second).status_code == 201


def test_too_many_wrong_attempts_lock_the_code(client, mailer, pg):
    assert send(client).status_code == 202
    code = mailer.last_code(EMAIL)
    for _ in range(codes.MAX_ATTEMPTS):
        response = register_with(client, wrong(code))
        assert response.status_code == 400 and response.json()["error"] == "code_invalid"
    locked = register_with(client, code)
    assert locked.status_code == 400 and locked.json()["error"] == "code_exhausted"
    assert pg.execute("SELECT attempts FROM email_codes").fetchone()["attempts"] == codes.MAX_ATTEMPTS
    assert pg.execute("SELECT count(*) AS n FROM users").fetchone()["n"] == 0


def test_expired_code_rejected(client, mailer, pg):
    assert send(client).status_code == 202
    code = mailer.last_code(EMAIL)
    age_codes(pg, codes.CODE_TTL_SECONDS + 1)
    response = register_with(client, code)
    assert response.status_code == 400 and response.json()["error"] == "code_expired"


def test_reset_code_is_single_use(client, mailer, register):
    register(EMAIL)
    assert send(client, purpose="reset_password").status_code == 202
    code = mailer.last_code(EMAIL)
    body = {"email": EMAIL, "code": code, "new_password": "brand-new-pass-1"}
    assert client.post("/api/v1/auth/password/reset", json=body).status_code == 204
    reused = client.post("/api/v1/auth/password/reset", json={**body, "new_password": "another-pass-2"})
    assert reused.status_code == 400 and reused.json()["error"] == "code_invalid"


def test_code_is_scoped_to_purpose_and_email(pg):
    _, code = codes.issue(pg, CODE_SECRET, EMAIL, "register")
    with pg.transaction():
        assert codes.check(pg, CODE_SECRET, EMAIL, "reset_password", code)[0] is codes.Verdict.INVALID
        assert codes.check(pg, CODE_SECRET, "other@example.com", "register", code)[0] is codes.Verdict.INVALID
        verdict, code_id = codes.check(pg, CODE_SECRET, EMAIL, "register", code)
    assert verdict is codes.Verdict.OK and code_id is not None


def test_mail_failure_frees_the_cooldown(client, mailer, pg):
    mailer.fail = True
    failed = send(client)
    assert failed.status_code == 503 and failed.json()["error"] == "mail_unavailable"
    assert pg.execute("SELECT count(*) AS n FROM email_codes").fetchone()["n"] == 0
    mailer.fail = False
    assert send(client).status_code == 202


def test_send_code_checks_account_existence(client, register):
    register(EMAIL)
    taken = send(client, purpose="register")
    assert taken.status_code == 409 and taken.json()["error"] == "email_taken"
    missing = send(client, email="nobody@example.com", purpose="reset_password")
    assert missing.status_code == 404 and missing.json()["error"] == "email_not_registered"


def test_per_email_hourly_limit(client, pg, fresh_window):
    fresh_window(SEND_CODE_PER_EMAIL.window_seconds)
    for _ in range(SEND_CODE_PER_EMAIL.limit):
        assert send(client).status_code == 202
        age_codes(pg, 61)  # 跳过 60 秒冷却，只测按邮箱的小时上限
    blocked = send(client)
    assert blocked.status_code == 429 and blocked.json()["message"] == "操作太频繁，请稍后再试"
    assert int(blocked.headers["Retry-After"]) >= 1
