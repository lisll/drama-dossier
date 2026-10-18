"""内部接口 `/internal/introspect`（M5b）。

媒体控制面靠它把「桌面端的访问令牌」换成「这是谁」。所以这里每一条
都关系到「别人能不能冒充某个用户」，失败路径必须逐个钉住：

* 没配密钥 → 404（不能退化成不鉴权）
* 密钥错 → 401
* 令牌无效 / 过期 / 已吊销 / 用户已注销 → 401
* 正常 → 只返回 user_id 与 email
"""

from __future__ import annotations

import dataclasses

import pytest
from fastapi.testclient import TestClient

from drama_cloud.commerce.api import create_app

from .conftest import FAST_HASHER, auth

PASSWORD = "correct-horse-1"
SECRET = "internal-secret-for-tests-0123456789"
SECRET_HEADER = "X-Internal-Secret"
TOKEN_HEADER = "X-Access-Token"


@pytest.fixture()
def with_secret(commerce_settings, pool, pg, mailer):
    """一个配了内部密钥的 app。共享的 commerce_settings 没有这一项，所以单独造一个。"""
    settings = dataclasses.replace(commerce_settings, internal_secret=SECRET)
    app = create_app(settings, pool=pool, mailer=mailer, hasher=FAST_HASHER)
    with TestClient(app) as test_client:
        yield test_client


@pytest.fixture()
def without_secret(client):
    """共享的 client 没配密钥：内部接口应当等同于不存在。"""
    return client


def _introspect(app, token: str, secret: str = SECRET):
    return app.post(
        "/internal/introspect",
        headers={SECRET_HEADER: secret, TOKEN_HEADER: token},
    )


def test_introspect_returns_the_user_id_and_email(with_secret, register):
    body = register("introspect@example.com", PASSWORD)
    token = body["tokens"]["access_token"]

    response = _introspect(with_secret, token)
    assert response.status_code == 200, response.text
    payload = response.json()
    assert payload == {"user_id": body["user"]["id"], "email": "introspect@example.com"}


def test_without_a_configured_secret_the_route_does_not_exist(without_secret, register):
    """留空密钥必须是 404，不是「谁都能调」。"""
    body = register("nosecret@example.com", PASSWORD)
    response = _introspect(without_secret, body["tokens"]["access_token"])
    assert response.status_code == 404, response.text


def test_a_wrong_secret_is_rejected(with_secret, register):
    body = register("wrongsecret@example.com", PASSWORD)
    response = _introspect(with_secret, body["tokens"]["access_token"], secret="wrong")
    assert response.status_code == 401, response.text


def test_a_missing_secret_header_is_rejected(with_secret, register):
    body = register("nosecretkey@example.com", PASSWORD)
    token = body["tokens"]["access_token"]
    response = with_secret.post("/internal/introspect", headers={TOKEN_HEADER: token})
    assert response.status_code == 401, response.text


def test_a_bad_token_is_rejected(with_secret):
    response = _introspect(with_secret, "not-a-real-token")
    assert response.status_code == 401, response.text


def test_a_missing_token_is_rejected(with_secret):
    response = with_secret.post("/internal/introspect", headers={SECRET_HEADER: SECRET})
    # 缺令牌是请求不完整，给 422；和「令牌不对」区分开，方便调用方排查。
    assert response.status_code == 422, response.text


def test_a_revoked_token_is_rejected(with_secret, register):
    """退出登录后令牌立即失效，内省也必须跟着拒绝。"""
    body = register("revoked@example.com", PASSWORD)
    token = body["tokens"]["access_token"]
    logout = with_secret.post("/api/v1/auth/logout", headers=auth(token))
    assert logout.status_code == 204, logout.text

    response = _introspect(with_secret, token)
    assert response.status_code == 401, response.text


def test_a_deleted_account_is_rejected(with_secret, register):
    """注销账号后，哪怕令牌还没到期，也不能再换出身份。"""
    body = register("deleted@example.com", PASSWORD)
    token = body["tokens"]["access_token"]
    deleted = with_secret.post(
        "/api/v1/me/delete", headers=auth(token), json={"password": PASSWORD}
    )
    assert deleted.status_code == 204, deleted.text

    response = _introspect(with_secret, token)
    assert response.status_code == 401, response.text


def test_introspect_does_not_leak_anything_else(with_secret, register):
    """只给 user_id 与 email：不该出现令牌、密码摘要、设备等。"""
    body = register("leak@example.com", PASSWORD)
    token = body["tokens"]["access_token"]
    payload = _introspect(with_secret, token).json()
    assert set(payload) == {"user_id", "email"}
    text = _introspect(with_secret, token).text
    assert token not in text
    assert "password" not in text.lower()
