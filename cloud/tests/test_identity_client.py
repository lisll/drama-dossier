"""`identity.introspect` 的分支覆盖（不经过 HTTP 服务，直接测这个函数）。

为什么要单独测：这个函数把「三种结果」分辨清楚（有效 / 无效 / 问不到），
上层所有鉴权都靠它这个判断。用 httpx 的打桩把每条分支都走一遍，
包括「commerce 返回了乱七八糟的东西」这种契约被破坏的情况。
"""

from __future__ import annotations

import httpx
import pytest

from drama_cloud import identity


class FakeResponse:
    def __init__(self, status_code: int, payload=None):
        self.status_code = status_code
        self._payload = payload

    def json(self):
        if self._payload is None:
            raise ValueError("no json")
        return self._payload


def patch_post(monkeypatch, outcome):
    """把 httpx.post 换成给定结果。`outcome` 可以是异常或 FakeResponse。"""
    calls = []

    def _post(url, headers=None, timeout=None):
        calls.append({"url": url, "headers": headers or {}, "timeout": timeout})
        if isinstance(outcome, Exception):
            raise outcome
        return outcome

    monkeypatch.setattr(identity.httpx, "post", _post)
    return calls


def test_a_valid_token_returns_the_identity(monkeypatch):
    patch_post(monkeypatch, FakeResponse(200, {"user_id": "u-1", "email": "a@example.com"}))
    result = identity.introspect("http://commerce", "secret", "token")
    assert result == identity.Identity(user_id="u-1", email="a@example.com")


def test_the_token_goes_in_a_header_not_the_body(monkeypatch):
    calls = patch_post(monkeypatch, FakeResponse(200, {"user_id": "u", "email": "a@example.com"}))
    identity.introspect("http://commerce", "secret", "token-value")
    headers = calls[0]["headers"]
    assert headers[identity.TOKEN_HEADER] == "token-value"
    assert headers[identity.SECRET_HEADER] == "secret"
    assert calls[0]["url"].endswith("/internal/introspect")


def test_401_means_the_token_is_invalid(monkeypatch):
    patch_post(monkeypatch, FakeResponse(401))
    assert identity.introspect("http://commerce", "secret", "token") is None


def test_404_means_the_route_is_off(monkeypatch):
    """commerce 没配内部密钥时会回 404：对本站来说令牌就是不可用。"""
    patch_post(monkeypatch, FakeResponse(404))
    assert identity.introspect("http://commerce", "secret", "token") is None


@pytest.mark.parametrize("status", [500, 502, 503])
def test_5xx_raises_unavailable(monkeypatch, status):
    patch_post(monkeypatch, FakeResponse(status))
    with pytest.raises(identity.IntrospectUnavailable):
        identity.introspect("http://commerce", "secret", "token")


def test_a_network_error_raises_unavailable(monkeypatch):
    patch_post(monkeypatch, httpx.ConnectError("refused"))
    with pytest.raises(identity.IntrospectUnavailable):
        identity.introspect("http://commerce", "secret", "token")


def test_a_timeout_raises_unavailable(monkeypatch):
    patch_post(monkeypatch, httpx.ReadTimeout("slow"))
    with pytest.raises(identity.IntrospectUnavailable):
        identity.introspect("http://commerce", "secret", "token")


def test_an_unexpected_status_raises_unavailable(monkeypatch):
    """3xx/2xx 但不符合约定：按不可用处理，不猜。"""
    patch_post(monkeypatch, FakeResponse(204))
    with pytest.raises(identity.IntrospectUnavailable):
        identity.introspect("http://commerce", "secret", "token")


@pytest.mark.parametrize(
    "payload",
    [
        {},                                    # 缺字段
        {"user_id": "u"},                      # 缺 email
        {"email": "a@example.com"},            # 缺 user_id
        {"user_id": "", "email": "a@example.com"},  # 空 user_id
        {"user_id": "u", "email": ""},         # 空 email
        ["not", "a", "dict"],                  # 类型不对
    ],
)
def test_a_malformed_payload_raises_unavailable(monkeypatch, payload):
    patch_post(monkeypatch, FakeResponse(200, payload))
    with pytest.raises(identity.IntrospectUnavailable):
        identity.introspect("http://commerce", "secret", "token")


def test_non_json_raises_unavailable(monkeypatch):
    patch_post(monkeypatch, FakeResponse(200, None))
    with pytest.raises(identity.IntrospectUnavailable):
        identity.introspect("http://commerce", "secret", "token")


def test_missing_configuration_raises_unavailable(monkeypatch):
    """没配地址或密钥时直接报不可用，不能悄悄放行。"""
    patch_post(monkeypatch, FakeResponse(200, {"user_id": "u", "email": "a@example.com"}))
    with pytest.raises(identity.IntrospectUnavailable):
        identity.introspect("", "secret", "token")
    with pytest.raises(identity.IntrospectUnavailable):
        identity.introspect("http://commerce", "", "token")
