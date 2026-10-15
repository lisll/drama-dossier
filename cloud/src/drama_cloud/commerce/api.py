"""账号服务的 HTTP 接口（FastAPI）。

令牌只认 `Authorization: Bearer <访问令牌>` 请求头，不接受网址参数：
网址会被写进访问日志和浏览器历史，令牌放在网址里就等于公开了。
"""

from __future__ import annotations

import logging
from contextlib import asynccontextmanager
from typing import Optional

import psycopg
from argon2 import PasswordHasher
from fastapi import Depends, FastAPI, Header, Request, Response
from fastapi.exceptions import RequestValidationError
from fastapi.responses import JSONResponse
from psycopg_pool import ConnectionPool

from . import ratelimit, security
from .accounts import AccountService
from .config import CommerceSettings, load_settings
from .db import MigrationError, open_pool, pending
from .errors import CommerceError, unauthorized
from .mailer import Mailer, SmtpMailer
from .ratelimit import Rule
from .schemas import (
    ChangePasswordRequest,
    LoginRequest,
    RefreshRequest,
    RegisterRequest,
    ResetPasswordRequest,
    SendCodeRequest,
)
from .sessions import AuthContext

log = logging.getLogger(__name__)

# 限流规则（名称、上限、时间窗秒数）。验证码自己还有 60 秒冷却和 5 次上限，这里再按 IP、邮箱各挡一层。
SEND_CODE_PER_IP = Rule("send_code:ip", 20, 3600)
SEND_CODE_PER_EMAIL = Rule("send_code:email", 10, 3600)
REGISTER_PER_IP = Rule("register:ip", 20, 3600)
LOGIN_PER_IP = Rule("login:ip", 30, 600)
LOGIN_PER_EMAIL = Rule("login:email", 10, 600)
RESET_PER_IP = Rule("reset:ip", 20, 3600)
CHANGE_PASSWORD_PER_USER = Rule("change_password:user", 10, 3600)

_FIELD_MESSAGES = {
    "missing": "缺少这个字段",
    "extra_forbidden": "不认识这个字段",
    "string_type": "应该是字符串",
    "bool_type": "应该是 true 或 false",
    "literal_error": "取值不在允许范围内",
    "json_invalid": "请求体不是合法的 JSON",
    "model_type": "请求体应该是 JSON 对象",
    "model_attributes_type": "请求体应该是 JSON 对象",
}


def _field_errors(error: RequestValidationError) -> list:
    """把校验错误整理成 [{field, message}]。不回显用户提交的值（里面可能有密码）。"""
    fields = []
    for item in error.errors():
        kind = item.get("type", "")
        # JSON 解析失败时 loc 里是出错的字符位置，不是字段名，统一归到 body。
        location = [] if kind == "json_invalid" else [str(part) for part in item.get("loc", ()) if part != "body"]
        if kind == "value_error":
            message = str(item.get("msg", "")).removeprefix("Value error, ")
        else:
            message = _FIELD_MESSAGES.get(kind, "取值不正确")
        fields.append({"field": ".".join(location) or "body", "message": message})
    return fields


def create_app(
    settings: Optional[CommerceSettings] = None,
    *,
    pool: Optional[ConnectionPool] = None,
    mailer: Optional[Mailer] = None,
    hasher: Optional[PasswordHasher] = None,
) -> FastAPI:
    """应用工厂。测试可以注入连接池、发信器和低开销的密码哈希器。"""
    settings = settings or load_settings()
    owns_pool = pool is None
    if pool is None:
        pool = open_pool(settings.dsn, settings.pool_min_size, settings.pool_max_size)
    try:
        with pool.connection() as conn:
            missing = pending(conn)
        if missing:
            raise MigrationError(
                f"数据库还没有迁移到最新，缺少 {', '.join(missing)}。请先运行 drama-commerce migrate"
            )
    except BaseException:
        if owns_pool:
            pool.close()
        raise
    service = AccountService(settings, mailer or SmtpMailer(settings.smtp), hasher)

    @asynccontextmanager
    async def lifespan(_app):
        try:
            yield
        finally:
            if owns_pool:
                pool.close()

    app = FastAPI(title="剧备账号服务", version="0.1.0", lifespan=lifespan)
    app.state.settings = settings
    app.state.service = service

    def get_conn():
        with pool.connection() as conn:
            yield conn

    def client_key(request: Request) -> str:
        host = request.client.host if request.client else "unknown"
        if settings.trusted_proxies:
            # 每层可信代理都会在 X-Forwarded-For 末尾追加它看到的来源地址。
            hops = [part.strip() for part in request.headers.get("x-forwarded-for", "").split(",")]
            hops = [part for part in hops if part]
            if len(hops) >= settings.trusted_proxies:
                host = hops[-settings.trusted_proxies]
        return security.privacy_key(settings.code_secret, host)

    def email_key(email: str) -> str:
        return security.privacy_key(settings.code_secret, email)

    def require_auth(authorization: Optional[str] = Header(None), conn=Depends(get_conn)) -> AuthContext:
        scheme, _, token = (authorization or "").partition(" ")
        if scheme.lower() != "bearer" or not token.strip():
            raise unauthorized("需要登录：请在 Authorization 请求头里带上 Bearer 访问令牌")
        ctx = service.authenticate(conn, token.strip())
        if ctx is None:
            raise unauthorized()
        return ctx

    @app.exception_handler(CommerceError)
    async def _commerce_error(_request: Request, error: CommerceError):
        headers = {"Retry-After": str(error.retry_after)} if error.retry_after else None
        return JSONResponse(
            status_code=error.status,
            content={"error": error.code, "message": error.message},
            headers=headers,
        )

    @app.exception_handler(RequestValidationError)
    async def _validation_error(_request: Request, error: RequestValidationError):
        return JSONResponse(
            status_code=422,
            content={"error": "invalid_request", "message": "请求参数不正确", "fields": _field_errors(error)},
        )

    @app.exception_handler(psycopg.OperationalError)
    async def _database_unavailable(_request: Request, error: psycopg.OperationalError):
        log.error("数据库不可用：%s", type(error).__name__)
        return JSONResponse(
            status_code=503,
            content={"error": "service_unavailable", "message": "服务暂时不可用，请稍后重试"},
        )

    @app.get("/api/v1/health")
    def health(conn=Depends(get_conn)):
        conn.execute("SELECT 1")
        return {"ok": True}

    @app.post("/api/v1/auth/codes", status_code=202)
    def send_code(payload: SendCodeRequest, request: Request, conn=Depends(get_conn)):
        ratelimit.hit(conn, SEND_CODE_PER_IP, client_key(request))
        ratelimit.hit(conn, SEND_CODE_PER_EMAIL, email_key(payload.email))
        return service.send_code(conn, payload.email, payload.purpose)

    @app.post("/api/v1/auth/register", status_code=201)
    def register(payload: RegisterRequest, request: Request, conn=Depends(get_conn)):
        ratelimit.hit(conn, REGISTER_PER_IP, client_key(request))
        return service.register(conn, payload.email, payload.code, payload.password)

    @app.post("/api/v1/auth/login")
    def login(payload: LoginRequest, request: Request, conn=Depends(get_conn)):
        ratelimit.hit(conn, LOGIN_PER_IP, client_key(request))
        ratelimit.hit(conn, LOGIN_PER_EMAIL, email_key(payload.email))
        return service.login(conn, payload.email, payload.password)

    @app.post("/api/v1/auth/refresh")
    def refresh(payload: RefreshRequest, conn=Depends(get_conn)):
        return service.refresh(conn, payload.refresh_token)

    @app.post("/api/v1/auth/logout", status_code=204)
    def logout(ctx: AuthContext = Depends(require_auth), conn=Depends(get_conn)):
        service.logout(conn, ctx)
        return Response(status_code=204)

    @app.post("/api/v1/auth/password/reset", status_code=204)
    def reset_password(payload: ResetPasswordRequest, request: Request, conn=Depends(get_conn)):
        ratelimit.hit(conn, RESET_PER_IP, client_key(request))
        service.reset_password(conn, payload.email, payload.code, payload.new_password)
        return Response(status_code=204)

    @app.post("/api/v1/auth/password/change", status_code=204)
    def change_password(
        payload: ChangePasswordRequest,
        ctx: AuthContext = Depends(require_auth),
        conn=Depends(get_conn),
    ):
        ratelimit.hit(conn, CHANGE_PASSWORD_PER_USER, ctx.user_id)
        service.change_password(conn, ctx, payload.current_password, payload.new_password)
        return Response(status_code=204)

    @app.get("/api/v1/me")
    def me(ctx: AuthContext = Depends(require_auth), conn=Depends(get_conn)):
        return service.me(conn, ctx)

    return app
