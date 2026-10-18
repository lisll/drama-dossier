"""管理员体系（M7-1）。

管理后台能动钱与价目，所以这几条是硬边界：

* 没有管理员令牌进不去；**用户令牌（at_）也不能当管理员令牌用**；
* 管理员令牌同样不能当用户令牌用（两个方向都测）；
* 停用后立刻踢掉会话，不等 8 小时自然过期；
* 登录失败信息统一，不暴露「这个邮箱存在但被停用了」；
* 审计日志不记管理员邮箱明文，只记域名。
"""

from __future__ import annotations

import pytest
from fastapi.testclient import TestClient

from drama_cloud.commerce.admins import ADMIN_TOKEN_PREFIX, AdminService
from drama_cloud.commerce.api import create_app

from .conftest import FAST_HASHER, auth

ADMIN_PASSWORD = "admin-password-1234"


@pytest.fixture()
def client(commerce_settings, pool, pg, mailer):
    app = create_app(commerce_settings, pool=pool, mailer=mailer, hasher=FAST_HASHER)
    with TestClient(app) as test_client:
        yield test_client


@pytest.fixture()
def admin(client, pg):
    """建一个管理员并登录，返回 (email, token, admin_id)。"""
    service = AdminService(hasher=FAST_HASHER)
    admin_id = service.create(pg, "boss@example.com", ADMIN_PASSWORD)
    response = client.post(
        "/api/v1/admin/login",
        json={"email": "boss@example.com", "password": ADMIN_PASSWORD},
    )
    assert response.status_code == 200, response.text
    return "boss@example.com", response.json()["token"], admin_id


def admin_auth(token: str) -> dict:
    return {"Authorization": f"Bearer {token}"}


def test_login_returns_an_admin_prefixed_token(admin):
    _, token, _ = admin
    assert token.startswith(ADMIN_TOKEN_PREFIX), "管理员令牌前缀必须与用户令牌不同"


def test_me_works_with_the_admin_token(client, admin):
    _, token, admin_id = admin
    body = client.get("/api/v1/admin/me", headers=admin_auth(token)).json()
    assert body["admin"]["id"] == admin_id
    assert body["admin"]["email"] == "boss@example.com"


def test_me_without_a_token_is_401(client):
    assert client.get("/api/v1/admin/me").status_code == 401


def test_me_with_a_wrong_token_is_401(client):
    assert client.get("/api/v1/admin/me", headers=admin_auth("adm_bogus")).status_code == 401


def test_a_user_token_cannot_act_as_an_admin(client, register, pg):
    """最重要的一条：付费用户不等于管理员。"""
    body = register("plain@example.com", "correct-horse-1")
    user_token = body["tokens"]["access_token"]
    response = client.get("/api/v1/admin/me", headers=admin_auth(user_token))
    assert response.status_code == 401, response.text


def test_an_admin_token_cannot_act_as_a_user(client, register, admin):
    """反方向也要挡住：管理员令牌不是用户令牌。"""
    _, admin_token, _ = admin
    response = client.get("/api/v1/credits", headers=admin_auth(admin_token))
    assert response.status_code == 401, response.text


def test_a_wrong_password_is_rejected_without_saying_why(client, admin):
    response = client.post(
        "/api/v1/admin/login",
        json={"email": "boss@example.com", "password": "wrong-password"},
    )
    assert response.status_code == 401
    assert response.json()["message"] == "邮箱或密码不正确"


def test_an_unknown_email_gives_the_same_message(client):
    response = client.post(
        "/api/v1/admin/login",
        json={"email": "nobody@example.com", "password": "whatever-1234"},
    )
    assert response.status_code == 401
    assert response.json()["message"] == "邮箱或密码不正确"


def test_a_short_password_is_refused_at_creation(pg):
    """管理员密码要求比用户高：这个账号能动钱与价目。"""
    service = AdminService(hasher=FAST_HASHER)
    with pytest.raises(Exception) as caught:
        service.create(pg, "short@example.com", "short")
    assert "12" in str(caught.value)


def test_the_same_email_cannot_be_created_twice(pg):
    service = AdminService(hasher=FAST_HASHER)
    service.create(pg, "dup@example.com", ADMIN_PASSWORD)
    with pytest.raises(Exception) as caught:
        service.create(pg, "dup@example.com", ADMIN_PASSWORD)
    assert "已存在" in str(caught.value)


def test_disabling_kills_the_session_immediately(client, pg, admin):
    """停用要立刻生效，不能等 8 小时自然过期。"""
    email, token, _ = admin
    assert client.get("/api/v1/admin/me", headers=admin_auth(token)).status_code == 200

    AdminService(hasher=FAST_HASHER).disable(pg, email)
    assert client.get("/api/v1/admin/me", headers=admin_auth(token)).status_code == 401


def test_a_disabled_admin_cannot_log_in(client, pg):
    service = AdminService(hasher=FAST_HASHER)
    service.create(pg, "gone@example.com", ADMIN_PASSWORD)
    service.disable(pg, "gone@example.com")
    response = client.post(
        "/api/v1/admin/login",
        json={"email": "gone@example.com", "password": ADMIN_PASSWORD},
    )
    # 与「密码不对」同一个说法：不暴露「这个账号存在但被停用了」。
    assert response.status_code == 401
    assert response.json()["message"] == "邮箱或密码不正确"


def test_a_disabled_admin_can_be_restored(client, pg):
    """停用是常规运维动作（人休假、临时收回权限），人回来要能恢复。

    没有这条路径就只能直接改库，而改库会绕过审计。
    """
    service = AdminService(hasher=FAST_HASHER)
    service.create(pg, "back@example.com", ADMIN_PASSWORD)
    service.disable(pg, "back@example.com")
    assert client.post(
        "/api/v1/admin/login",
        json={"email": "back@example.com", "password": ADMIN_PASSWORD},
    ).status_code == 401

    service.enable(pg, "back@example.com")
    response = client.post(
        "/api/v1/admin/login",
        json={"email": "back@example.com", "password": ADMIN_PASSWORD},
    )
    assert response.status_code == 200, response.text

    rows = pg.execute(
        "SELECT action FROM audit_log WHERE action IN ('admin.disable','admin.enable')"
    ).fetchall()
    assert {row["action"] for row in rows} == {"admin.disable", "admin.enable"}


def test_restoring_a_never_disabled_admin_is_a_conflict(pg):
    """区分「不存在」与「本来就没停用」：运维要做的事不同。"""
    service = AdminService(hasher=FAST_HASHER)
    service.create(pg, "active@example.com", ADMIN_PASSWORD)
    with pytest.raises(Exception) as caught:
        service.enable(pg, "active@example.com")
    assert "没有被停用" in str(caught.value)


def test_restoring_a_missing_admin_is_404(pg):
    with pytest.raises(Exception) as caught:
        AdminService(hasher=FAST_HASHER).enable(pg, "nobody@example.com")
    assert "没有这个管理员" in str(caught.value)


def test_logout_revokes_the_token(client, admin):
    _, token, _ = admin
    assert client.post("/api/v1/admin/logout", headers=admin_auth(token)).status_code == 204
    assert client.get("/api/v1/admin/me", headers=admin_auth(token)).status_code == 401


def test_the_audit_log_does_not_record_the_admin_email(pg, admin):
    rows = pg.execute(
        "SELECT detail FROM audit_log WHERE action LIKE 'admin.%%'"
    ).fetchall()
    assert rows, "创建与登录应当留下审计记录"
    for row in rows:
        assert "boss@example.com" not in str(row["detail"]), "审计里不该出现完整管理员邮箱"


def test_login_is_rate_limited(client, admin, pg):
    """管理员登录比用户登录更紧：连续试错要被限流。"""
    from drama_cloud.commerce import ratelimit

    codes = []
    for _ in range(12):
        response = client.post(
            "/api/v1/admin/login",
            json={"email": "boss@example.com", "password": "wrong-password"},
        )
        codes.append(response.status_code)
    assert 429 in codes, f"连续失败应当被限流，实际状态码：{codes}"


def test_the_admin_page_is_served(client):
    response = client.get("/admin")
    # 页面文件在 M7-2 才写；这里只确认路由存在且不是 404。
    assert response.status_code in (200, 500), response.status_code
    if response.status_code == 500:
        assert "页面缺失" in response.text
