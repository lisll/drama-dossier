"""M5b：媒体控制面的认证改为「内省 commerce 令牌」。

这份测试钉的是**三种结果必须分开**，因为混错任何一种都是安全问题：

* 令牌有效 → 200，且角色由本地白名单决定；
* 令牌无效 → 401（用户该重新登录）；
* 问不到 commerce → 503（服务暂时不可用）。

最容易出的错是把「问不到」当成「无效」或「有效」：
前者让所有人在 commerce 抖一下时被迫重新登录，后者等于没鉴权。
"""

from __future__ import annotations

import pytest
from fastapi.testclient import TestClient

from drama_cloud import identity
from drama_cloud.api import create_app


@pytest.fixture()
def client(settings, conn, fake_commerce):
    return TestClient(create_app(settings))


def auth(token: str) -> dict:
    return {"Authorization": f"Bearer {token}"}


def test_no_authorization_header_is_401(client):
    assert client.get("/api/v1/auth/whoami").status_code == 401


def test_a_token_commerce_does_not_know_is_401(client, fake_commerce):
    assert client.get("/api/v1/auth/whoami", headers=auth("unknown")).status_code == 401


def test_a_valid_commerce_token_is_accepted(client, conn, fake_commerce):
    token = fake_commerce.user(conn, "someone@example.com", "operator")
    response = client.get("/api/v1/auth/whoami", headers=auth(token))
    assert response.status_code == 200, response.text
    assert response.json()["role"] == "operator"


def test_when_commerce_is_unreachable_we_answer_503_not_401(client, conn, fake_commerce):
    """问不到账号服务时返回 503：既不放行，也不谎称令牌失效。"""
    token = fake_commerce.user(conn, "someone@example.com", "operator")
    fake_commerce.unavailable = True

    response = client.get("/api/v1/auth/whoami", headers=auth(token))
    assert response.status_code == 503, response.text
    assert response.json()["detail"]["error"] == "service_unavailable"


def test_admin_comes_from_the_local_whitelist(client, conn, fake_commerce):
    """commerce 那边没有角色概念，管理员只由本地白名单决定。"""
    operator = fake_commerce.user(conn, "plain@example.com", "operator")
    admin = fake_commerce.user(conn, "boss@example.com", "admin")

    assert client.get("/api/v1/auth/whoami", headers=auth(operator)).json()["role"] == "operator"
    assert client.get("/api/v1/auth/whoami", headers=auth(admin)).json()["role"] == "admin"


def test_role_is_not_taken_from_commerce(client, conn, fake_commerce):
    """同一封邮件在白名单里时才是 admin：证明角色不来自 commerce 的返回值。

    假 commerce 只返回 user_id 与 email，没有任何角色字段；
    这里再确认一次「不在白名单 = operator」，防止将来有人给 Identity 加个 role 字段
    就直接拿来用。
    """
    token = fake_commerce.register("nobody@example.com")[1]
    response = client.get("/api/v1/auth/whoami", headers=auth(token))
    assert response.status_code == 200
    assert response.json()["role"] == "operator"


def test_the_shadow_user_row_is_created_for_foreign_keys(client, conn, fake_commerce):
    """身份来自 commerce，但本地必须有一行 users，否则建项目会撞外键。"""
    email = "shadow@example.com"
    token = fake_commerce.user(conn, email, "operator")
    response = client.get("/api/v1/auth/whoami", headers=auth(token))
    assert response.status_code == 200

    row = conn.execute(
        "SELECT id, name, role, token_hash FROM users WHERE name = %s", (email,)
    ).fetchone()
    assert row is not None, "认证后应当建好影子用户行"
    assert row["role"] == "operator"
    # 影子行不能拿来登录：它的 token_hash 不是任何真实令牌的摘要。
    assert row["token_hash"].startswith("commerce:")


def test_promoting_to_admin_updates_the_shadow_row(client, conn, fake_commerce):
    """把邮箱加进白名单后，影子行的 role 也要跟着变，否则权限判断会不一致。"""
    email = "promoted@example.com"
    token = fake_commerce.user(conn, email, "operator")
    client.get("/api/v1/auth/whoami", headers=auth(token))

    from drama_cloud.auth import add_admin

    add_admin(conn, email, actor="test")
    response = client.get("/api/v1/auth/whoami", headers=auth(token))
    assert response.json()["role"] == "admin"
    row = conn.execute("SELECT role FROM users WHERE name = %s", (email,)).fetchone()
    assert row["role"] == "admin"
