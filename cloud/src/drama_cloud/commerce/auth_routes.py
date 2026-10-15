"""账号接口：验证码、注册、登录、令牌刷新、退出、改密码、当前账号、导出个人数据、注销。"""

from __future__ import annotations

from fastapi import APIRouter, Depends, Request, Response

from . import entitlements, ratelimit
from .deps import client_key, email_key, get_conn, require_auth
from .ratelimit import Rule
from .schemas import (
    ChangePasswordRequest,
    DeleteAccountRequest,
    LoginRequest,
    RefreshRequest,
    RegisterRequest,
    ResetPasswordRequest,
    SendCodeRequest,
)
from .sessions import AuthContext

# 限流规则（名称、上限、时间窗秒数）。验证码自己还有 60 秒冷却和 5 次上限，这里再按 IP、邮箱各挡一层。
SEND_CODE_PER_IP = Rule("send_code:ip", 20, 3600)
SEND_CODE_PER_EMAIL = Rule("send_code:email", 10, 3600)
REGISTER_PER_IP = Rule("register:ip", 20, 3600)
LOGIN_PER_IP = Rule("login:ip", 30, 600)
LOGIN_PER_EMAIL = Rule("login:email", 10, 600)
RESET_PER_IP = Rule("reset:ip", 20, 3600)
CHANGE_PASSWORD_PER_USER = Rule("change_password:user", 10, 3600)
# 注销要校验密码：限住次数，拿着偷来的访问令牌也没法靠它猜密码。
DELETE_PER_USER = Rule("delete_account:user", 5, 3600)
EXPORT_PER_USER = Rule("export:user", 10, 3600)

router = APIRouter(prefix="/api/v1")


def _service(request: Request):
    return request.app.state.service


@router.post("/auth/codes", status_code=202)
def send_code(payload: SendCodeRequest, request: Request, conn=Depends(get_conn)):
    ratelimit.hit(conn, SEND_CODE_PER_IP, client_key(request))
    ratelimit.hit(conn, SEND_CODE_PER_EMAIL, email_key(request, payload.email))
    return _service(request).send_code(conn, payload.email, payload.purpose)


@router.post("/auth/register", status_code=201)
def register(payload: RegisterRequest, request: Request, conn=Depends(get_conn)):
    ratelimit.hit(conn, REGISTER_PER_IP, client_key(request))
    return _service(request).register(conn, payload.email, payload.code, payload.password)


@router.post("/auth/login")
def login(payload: LoginRequest, request: Request, conn=Depends(get_conn)):
    ratelimit.hit(conn, LOGIN_PER_IP, client_key(request))
    ratelimit.hit(conn, LOGIN_PER_EMAIL, email_key(request, payload.email))
    return _service(request).login(conn, payload.email, payload.password)


@router.post("/auth/refresh")
def refresh(payload: RefreshRequest, request: Request, conn=Depends(get_conn)):
    return _service(request).refresh(conn, payload.refresh_token)


@router.post("/auth/logout", status_code=204)
def logout(request: Request, ctx: AuthContext = Depends(require_auth), conn=Depends(get_conn)):
    _service(request).logout(conn, ctx)
    return Response(status_code=204)


@router.post("/auth/password/reset", status_code=204)
def reset_password(payload: ResetPasswordRequest, request: Request, conn=Depends(get_conn)):
    ratelimit.hit(conn, RESET_PER_IP, client_key(request))
    _service(request).reset_password(conn, payload.email, payload.code, payload.new_password)
    return Response(status_code=204)


@router.post("/auth/password/change", status_code=204)
def change_password(
    payload: ChangePasswordRequest,
    request: Request,
    ctx: AuthContext = Depends(require_auth),
    conn=Depends(get_conn),
):
    ratelimit.hit(conn, CHANGE_PASSWORD_PER_USER, ctx.user_id)
    _service(request).change_password(conn, ctx, payload.current_password, payload.new_password)
    return Response(status_code=204)


@router.get("/me")
def me(request: Request, ctx: AuthContext = Depends(require_auth), conn=Depends(get_conn)):
    body = _service(request).me(conn, ctx)
    body["entitlement"] = entitlements.current(conn, ctx.user_id).view()
    return body


@router.get("/me/export")
def export_me(request: Request, ctx: AuthContext = Depends(require_auth), conn=Depends(get_conn)):
    ratelimit.hit(conn, EXPORT_PER_USER, ctx.user_id)
    return _service(request).export(conn, ctx)


@router.post("/me/delete", status_code=204)
def delete_me(
    payload: DeleteAccountRequest,
    request: Request,
    ctx: AuthContext = Depends(require_auth),
    conn=Depends(get_conn),
):
    ratelimit.hit(conn, DELETE_PER_USER, ctx.user_id)
    _service(request).delete_account(conn, ctx, payload.password)
    return Response(status_code=204)
