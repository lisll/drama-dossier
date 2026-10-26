"""管理后台接口 /admin/api/* 与静态页面 /admin。

所有接口都要求：IP 在白名单（中间件已拦）、有效会话 Cookie；
写操作额外要求 X-CSRF-Token 与同源 Origin；发卡/改密码还要重输密码。
"""

from __future__ import annotations

import hmac
from typing import Callable
from urllib.parse import urlsplit

from fastapi import Depends, FastAPI, Query, Request
from fastapi.responses import FileResponse, JSONResponse
from pydantic import BaseModel, ConfigDict, Field

from .admin_auth import AdminAuth, AdminSession
from .config import Settings
from .errors import LicenseError
from .plans import PLANS
from .ratelimit import RateLimiter
from .service import LicenseService

COOKIE = "dd_admin"
HOUR = 3600


class _Strict(BaseModel):
    model_config = ConfigDict(extra="forbid", str_max_length=512)


class LoginIn(_Strict):
    username: str = Field(max_length=64)
    password: str = Field(max_length=256)


class PasswordIn(_Strict):
    old: str = Field(max_length=256)
    new: str = Field(max_length=256)


class BatchIn(_Strict):
    plan: str = Field(max_length=16)
    count: int = Field(ge=1, le=10000)
    note: str = Field(default="", max_length=200)
    password: str = Field(max_length=256)


class CardIn(_Strict):
    code: str = Field(max_length=64)


class RevokeMachineIn(_Strict):
    reason: str = Field(min_length=1, max_length=200)



def register_admin(
    app: FastAPI,
    settings: Settings,
    service: LicenseService,
    auth: AdminAuth,
    limiter: RateLimiter,
    client_ip: Callable[[Request, Settings], str],
) -> None:
    from .api import WEB_DIR

    def ip_of(request: Request) -> str:
        return client_ip(request, settings)

    def same_origin(request: Request) -> bool:
        origin = request.headers.get("origin")
        if not origin:
            return False
        host = request.headers.get("host", "")
        try:
            return urlsplit(origin).netloc == host
        except ValueError:
            return False

    def require_session(request: Request) -> AdminSession:
        sess = auth.session(request.cookies.get(COOKIE), ip_of(request))
        if sess is None:
            raise LicenseError("unauthorized", "请先登录", 401)
        return sess

    def require_write(request: Request, sess: AdminSession = Depends(require_session)) -> AdminSession:
        token = request.headers.get("x-csrf-token", "")
        if not same_origin(request) or not hmac.compare_digest(token, sess.csrf):
            raise LicenseError("csrf", "请求来源校验失败，请刷新页面后重试", 403)
        limiter.hit(f"adm:write:{sess.admin_id}", 120, HOUR)
        return sess

    def set_cookie(resp: JSONResponse, token: str) -> None:
        resp.set_cookie(
            COOKIE, token, max_age=8 * HOUR, httponly=True, secure=settings.cookie_secure,
            samesite="strict", path="/admin",
        )

    # ------------------------------------------------------------ 静态页面

    @app.get("/admin", include_in_schema=False)
    @app.get("/admin/", include_in_schema=False)
    def admin_page():
        return FileResponse(WEB_DIR / "admin.html", media_type="text/html; charset=utf-8")

    @app.get("/admin/static/{name}", include_in_schema=False)
    def admin_static(name: str):
        allowed = {"admin.js": "text/javascript; charset=utf-8", "admin.css": "text/css; charset=utf-8"}
        if name not in allowed:
            raise LicenseError("not_found", "Not Found", 404)
        return FileResponse(WEB_DIR / name, media_type=allowed[name])

    # ------------------------------------------------------------ 认证

    @app.post("/admin/api/login")
    def login(body: LoginIn, request: Request):
        if not same_origin(request):
            raise LicenseError("csrf", "请求来源校验失败", 403)
        ip = ip_of(request)
        limiter.hit(f"adm:login:ip:{ip}", 20, HOUR)
        token, sess = auth.login(body.username, body.password, ip)
        resp = JSONResponse({"username": sess.username, "csrf": sess.csrf})
        set_cookie(resp, token)
        return resp

    @app.post("/admin/api/logout")
    def logout(request: Request, _: AdminSession = Depends(require_write)):
        auth.logout(request.cookies.get(COOKIE))
        resp = JSONResponse({"ok": True})
        resp.delete_cookie(COOKIE, path="/admin")
        return resp

    @app.get("/admin/api/me")
    def me(sess: AdminSession = Depends(require_session)):
        return {"username": sess.username, "csrf": sess.csrf, "allow_any_ip": settings.admin_allow_any_ip}

    @app.post("/admin/api/password")
    def password(body: PasswordIn, request: Request, sess: AdminSession = Depends(require_write)):
        auth.change_password(sess, body.old, body.new, ip_of(request))
        resp = JSONResponse({"ok": True})
        resp.delete_cookie(COOKIE, path="/admin")
        return resp

    # ------------------------------------------------------------ 查询

    @app.get("/admin/api/stats")
    def stats(_: AdminSession = Depends(require_session)):
        return service.stats()

    @app.get("/admin/api/plans")
    def plans(_: AdminSession = Depends(require_session)):
        return [{"code": p.code, "label": p.label, "days": p.days} for p in PLANS.values()]

    @app.get("/admin/api/batches")
    def batches(page: int = Query(1, ge=1, le=100000), _: AdminSession = Depends(require_session)):
        return service.list_batches(page)

    @app.get("/admin/api/machines")
    def machines(
        q: str = Query("", max_length=64),
        status: str = Query("", max_length=16),
        page: int = Query(1, ge=1, le=100000),
        _: AdminSession = Depends(require_session),
    ):
        return service.list_machines(q, status, page)

    @app.get("/admin/api/machines/{machine_id}")
    def machine(machine_id: int, _: AdminSession = Depends(require_session)):
        return service.machine_detail(machine_id)

    @app.get("/admin/api/audit")
    def audit_log(
        action: str = Query("", max_length=64),
        actor: str = Query("", max_length=64),
        page: int = Query(1, ge=1, le=100000),
        _: AdminSession = Depends(require_session),
    ):
        return service.list_audit(action, actor, page)

    # ------------------------------------------------------------ 写操作

    @app.post("/admin/api/batches")
    def create_batch(body: BatchIn, request: Request, sess: AdminSession = Depends(require_write)):
        ip = ip_of(request)
        auth.reauth(sess, body.password, ip)
        batch_id, codes = service.create_batch(body.plan, body.count, body.note, sess.username, ip)
        return {"batch": batch_id, "codes": codes}

    @app.post("/admin/api/batches/{batch_id}/revoke")
    def revoke_batch(batch_id: int, request: Request, sess: AdminSession = Depends(require_write)):
        return {"revoked": service.revoke_batch(batch_id, sess.username, ip_of(request))}

    @app.post("/admin/api/cards/lookup")
    def lookup(body: CardIn, request: Request, sess: AdminSession = Depends(require_write)):
        # 用 POST + CSRF：卡密不进网址和访问日志
        limiter.hit(f"adm:lookup:{sess.admin_id}", 300, HOUR)
        return service.lookup_card(body.code)

    @app.post("/admin/api/cards/revoke")
    def revoke_card(body: CardIn, request: Request, sess: AdminSession = Depends(require_write)):
        service.revoke_card(body.code, sess.username, ip_of(request))
        return {"ok": True}

    @app.post("/admin/api/machines/{machine_id}/revoke")
    def revoke_machine(machine_id: int, body: RevokeMachineIn, request: Request, sess: AdminSession = Depends(require_write)):
        service.revoke_machine(machine_id, body.reason, sess.username, ip_of(request))
        return {"ok": True}

    @app.post("/admin/api/machines/{machine_id}/unrevoke")
    def unrevoke_machine(machine_id: int, request: Request, sess: AdminSession = Depends(require_write)):
        service.unrevoke_machine(machine_id, sess.username, ip_of(request))
        return {"ok": True}
