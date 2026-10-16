"""账号与授权服务的 HTTP 接口（FastAPI）：应用工厂、统一的错误响应、健康检查。

路由按业务拆在 auth_routes（账号）和 license_routes（卡密、授权、设备）里。
令牌只认 `Authorization: Bearer <访问令牌>` 请求头，不接受网址参数（见 deps.require_auth）。
"""

from __future__ import annotations

import logging
from contextlib import asynccontextmanager
from typing import Optional

import psycopg
from argon2 import PasswordHasher
from fastapi import Depends, FastAPI, Request
from fastapi.exceptions import RequestValidationError
from fastapi.responses import JSONResponse
from psycopg_pool import ConnectionPool

from . import auth_routes, license_routes, package_routes
from .accounts import AccountService
from .body_limit import BodyLimitMiddleware
from .config import CommerceSettings, load_settings
from .db import MigrationError, open_pool, pending
from .deps import get_conn
from .documents.pdf import load_font
from .errors import CommerceError
from .license_service import LicenseService
from .license_token import Signer
from .mailer import Mailer, SmtpMailer
from .package_service import PackageService
from .recipes import RecipeService

log = logging.getLogger(__name__)

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
    # 先加载签名私钥：私钥有问题就在打开连接池之前报错。
    signer = Signer.from_pem_file(settings.license_key_file)
    # 报审包 PDF 的中文字体：找不到就拒绝启动，不生成一份方块字的 PDF。
    font = load_font(settings.pdf_font)
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

    @asynccontextmanager
    async def lifespan(_app):
        try:
            yield
        finally:
            if owns_pool:
                pool.close()

    app = FastAPI(title="剧备账号与授权服务", version="0.2.0", lifespan=lifespan)
    app.state.settings = settings
    app.state.pool = pool
    app.state.service = AccountService(settings, mailer or SmtpMailer(settings.smtp), hasher)
    app.state.licenses = LicenseService(settings, signer)
    app.state.packages = PackageService(settings, signer, font)
    app.state.recipes = RecipeService(signer)
    log.info("授权签名公钥 kid=%s", signer.kid)

    @app.exception_handler(CommerceError)
    async def _commerce_error(_request: Request, error: CommerceError):
        headers = {"Retry-After": str(error.retry_after)} if error.retry_after else None
        return JSONResponse(
            status_code=error.status,
            content={**error.extra, "error": error.code, "message": error.message},
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

    app.include_router(auth_routes.router)
    app.include_router(license_routes.router)
    app.include_router(package_routes.router)
    # 最外层：在路由、认证、JSON 解析之前就挡住超大请求体。
    app.add_middleware(BodyLimitMiddleware)
    return app
