"""FastAPI 应用装配：客户端接口 /v1/*、后台 /admin、统一安全响应头。"""

from __future__ import annotations

import ipaddress
import logging
from pathlib import Path

from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse, Response
from pydantic import BaseModel, ConfigDict, Field
from starlette.exceptions import HTTPException as StarletteHTTPException
from fastapi.exceptions import RequestValidationError

from .admin_auth import AdminAuth
from .config import Settings
from .db import Database
from .errors import LicenseError
from .ratelimit import RateLimiter
from .service import LicenseService

log = logging.getLogger("dd_license")

MAX_BODY = 4096
HOUR = 3600


class _Strict(BaseModel):
    model_config = ConfigDict(extra="forbid", str_max_length=2048)


class ActivateIn(_Strict):
    card: str = Field(max_length=64)
    machine_id: str = Field(min_length=64, max_length=64)
    app_version: str = Field(default="", max_length=32)


class RefreshIn(_Strict):
    token: str = Field(max_length=2048)
    machine_id: str = Field(min_length=64, max_length=64)
    app_version: str = Field(default="", max_length=32)


def client_ip(request: Request, settings: Settings) -> str:
    """真实客户端 IP。只有直连方是本机反向代理时才采信 X-Forwarded-For。"""
    peer = request.client.host if request.client else ""
    if settings.trust_proxy:
        try:
            if ipaddress.ip_address(peer).is_loopback:
                fwd = request.headers.get("x-forwarded-for", "")
                if fwd:
                    # 取最右一个：它是本机 Caddy 亲手追加的，左边的都可能被客户端伪造
                    candidate = fwd.split(",")[-1].strip()
                    ipaddress.ip_address(candidate)
                    return candidate
        except ValueError:
            pass
    return peer


SECURITY_HEADERS = {
    "X-Content-Type-Options": "nosniff",
    "Referrer-Policy": "no-referrer",
    "X-Frame-Options": "DENY",
    "Strict-Transport-Security": "max-age=31536000; includeSubDomains",
    "Cross-Origin-Opener-Policy": "same-origin",
    "Cross-Origin-Resource-Policy": "same-origin",
    "Permissions-Policy": "camera=(), microphone=(), geolocation=()",
}
ADMIN_CSP = (
    "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; "
    "connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'; object-src 'none'"
)


def create_app(settings: Settings, service: LicenseService, auth: AdminAuth, limiter: RateLimiter | None = None) -> FastAPI:
    app = FastAPI(docs_url=None, redoc_url=None, openapi_url=None, debug=False)
    limiter = limiter or RateLimiter()
    app.state.settings = settings
    app.state.service = service
    app.state.auth = auth
    app.state.limiter = limiter

    @app.middleware("http")
    async def guard(request: Request, call_next):
        path = request.url.path
        # 后台可见性：白名单之外一律 404（连登录页都看不到）
        if path == "/admin" or path.startswith("/admin/"):
            if not admin_ip_allowed(request):
                return _not_found()
        length = request.headers.get("content-length")
        if length is not None:
            try:
                if int(length) > MAX_BODY:
                    return JSONResponse({"error": "too_large", "message": "请求体过大"}, status_code=413)
            except ValueError:
                return JSONResponse({"error": "bad_request", "message": "请求头无效"}, status_code=400)
        elif request.method in ("POST", "PUT", "PATCH") and request.headers.get("transfer-encoding"):
            return JSONResponse({"error": "too_large", "message": "不支持分块上传"}, status_code=411)
        response = await call_next(request)
        for k, v in SECURITY_HEADERS.items():
            response.headers.setdefault(k, v)
        if path == "/admin" or path.startswith("/admin/"):
            response.headers["Content-Security-Policy"] = ADMIN_CSP
        if path.startswith("/v1/") or path.startswith("/admin/api/"):
            response.headers["Cache-Control"] = "no-store"
        if "server" in response.headers:
            del response.headers["server"]
        return response

    def admin_ip_allowed(request: Request) -> bool:
        if not settings.admin_enabled:
            return False
        if settings.admin_allow_any_ip:
            return True
        try:
            ip = ipaddress.ip_address(client_ip(request, settings))
        except ValueError:
            return False
        return any(ip in net for net in settings.admin_allow_ips)

    app.state.admin_ip_allowed = admin_ip_allowed

    @app.exception_handler(LicenseError)
    async def _license_error(_: Request, exc: LicenseError):
        headers = {"Retry-After": str(exc.retry_after)} if exc.retry_after else None
        return JSONResponse(exc.to_dict(), status_code=exc.status, headers=headers)

    @app.exception_handler(RequestValidationError)
    async def _validation_error(_: Request, exc: RequestValidationError):
        return JSONResponse({"error": "bad_request", "message": "请求参数不正确"}, status_code=422)

    @app.exception_handler(StarletteHTTPException)
    async def _http_error(_: Request, exc: StarletteHTTPException):
        if exc.status_code == 404:
            return _not_found()
        return JSONResponse({"error": "http_error", "message": "请求无法处理"}, status_code=exc.status_code)

    @app.exception_handler(Exception)
    async def _unexpected(request: Request, exc: Exception):
        log.exception("未处理的异常：%s %s", request.method, request.url.path)
        return JSONResponse({"error": "internal", "message": "服务器内部错误，请稍后再试"}, status_code=500)

    # ------------------------------------------------------------ 客户端接口

    @app.post("/v1/activate")
    def activate(body: ActivateIn, request: Request):
        ip = client_ip(request, settings)
        limiter.hit(f"act:ip:{ip}", 30, HOUR)
        limiter.hit(f"act:mid:{body.machine_id}", 10, HOUR)
        issued = service.activate(body.card, body.machine_id, body.app_version, ip)
        return issued.to_dict()

    @app.post("/v1/refresh")
    def refresh(body: RefreshIn, request: Request):
        ip = client_ip(request, settings)
        limiter.hit(f"ref:ip:{ip}", 600, HOUR)
        limiter.hit(f"ref:mid:{body.machine_id}", 60, HOUR)
        issued = service.refresh(body.token, body.machine_id, body.app_version, ip)
        d = issued.to_dict()
        d.pop("already_redeemed", None)
        return d

    @app.get("/v1/health")
    def health():
        return {"ok": True, "kid": service.kid}

    from .admin_api import register_admin  # 避免循环导入

    register_admin(app, settings, service, auth, limiter, client_ip)
    return app


def _not_found() -> Response:
    return JSONResponse({"error": "not_found", "message": "Not Found"}, status_code=404)


WEB_DIR = Path(__file__).parent / "web"
