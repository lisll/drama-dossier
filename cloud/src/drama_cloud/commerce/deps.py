"""路由共用的依赖：数据库连接、限流用的来源标识、登录校验。

令牌只认 `Authorization: Bearer <访问令牌>` 请求头，不接受网址参数：
网址会被写进访问日志和浏览器历史，令牌放在网址里就等于公开了。
"""

from __future__ import annotations

from typing import Optional

from fastapi import Depends, Header, Request

from . import security
from .errors import unauthorized
from .sessions import AuthContext


def get_conn(request: Request):
    with request.app.state.pool.connection() as conn:
        yield conn


def client_key(request: Request) -> str:
    """来源 IP 的 HMAC 摘要（限流桶用，不存原始 IP）。"""
    settings = request.app.state.settings
    host = request.client.host if request.client else "unknown"
    if settings.trusted_proxies:
        # 每层可信代理都会在 X-Forwarded-For 末尾追加它看到的来源地址。
        hops = [part.strip() for part in request.headers.get("x-forwarded-for", "").split(",")]
        hops = [part for part in hops if part]
        if len(hops) >= settings.trusted_proxies:
            host = hops[-settings.trusted_proxies]
    return security.privacy_key(settings.code_secret, host)


def email_key(request: Request, email: str) -> str:
    return security.privacy_key(request.app.state.settings.code_secret, email)


def require_auth(
    request: Request,
    authorization: Optional[str] = Header(None),
    conn=Depends(get_conn),
) -> AuthContext:
    scheme, _, token = (authorization or "").partition(" ")
    if scheme.lower() != "bearer" or not token.strip():
        raise unauthorized("需要登录：请在 Authorization 请求头里带上 Bearer 访问令牌")
    ctx = request.app.state.service.authenticate(conn, token.strip())
    if ctx is None:
        raise unauthorized()
    return ctx
