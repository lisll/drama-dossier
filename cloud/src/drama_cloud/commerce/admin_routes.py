"""管理后台的接口（M7）。

分两类：

* **认证**：`/api/v1/admin/login` 等。不要登录，靠密码。
* **业务**：`/api/v1/admin/*`。要管理员令牌，**不是**用户令牌——
  两种令牌前缀不同（`adm_` vs `at_`），交叉使用一律拒绝。

页面本身在 `/admin`（见 `admin_page.py`），与接口分开：页面是静态文件，
接口是 JSON，混在一起会让路由表难读。
"""

from __future__ import annotations

import logging

from fastapi import APIRouter, Depends, Header, Request

from ..base import StrictModel
from . import ratelimit
from .admins import AdminContext, AdminService
from .deps import client_key, email_key, get_conn
from .errors import unauthorized
from .ratelimit import Rule
from pydantic import Field

log = logging.getLogger(__name__)

router = APIRouter(prefix="/api/v1/admin")

#: 管理员登录限流：按来源 IP 与按邮箱两个桶，与用户登录同一套机制。
#: 管理员账号能动钱与价目，所以比用户登录更紧。
ADMIN_LOGIN_PER_IP = Rule("admin:login:ip", 20, 600)
ADMIN_LOGIN_PER_EMAIL = Rule("admin:login:email", 8, 600)


def admin_service(request: Request) -> AdminService:
    service = getattr(request.app.state, "admins", None)
    if service is None:
        service = AdminService()
        request.app.state.admins = service
    return service


def require_admin(
    request: Request,
    conn=Depends(get_conn),
    authorization: str = Header(default=""),
) -> AdminContext:
    """管理员认证。只认 `Authorization: Bearer adm_...`。

    用户令牌（`at_`）在这里一律拒绝：管理后台和用户接口是两条不同的授权路径，
    混用等于让「任何付费用户」变成「管理员」。
    """
    scheme, _, token = (authorization or "").partition(" ")
    if scheme.lower() != "bearer" or not token.strip():
        raise unauthorized("需要管理员登录：请在 Authorization 请求头里带上 Bearer 管理员令牌")
    ctx = admin_service(request).authenticate(conn, token.strip())
    if ctx is None:
        raise unauthorized("管理员登录已失效，请重新登录")
    return ctx


class AdminLoginRequest(StrictModel):
    email: str = Field(min_length=3, max_length=254)
    password: str = Field(min_length=1, max_length=128)


@router.post("/login")
def admin_login(
    payload: AdminLoginRequest,
    request: Request,
    conn=Depends(get_conn),
):
    """管理员登录。失败信息统一为「邮箱或密码不正确」。"""
    ratelimit.hit(conn, ADMIN_LOGIN_PER_IP, client_key(request))
    ratelimit.hit(conn, ADMIN_LOGIN_PER_EMAIL, email_key(request, payload.email))
    return admin_service(request).login(conn, payload.email, payload.password)


@router.post("/logout", status_code=204)
def admin_logout(
    request: Request,
    ctx: AdminContext = Depends(require_admin),
    conn=Depends(get_conn),
):
    admin_service(request).logout(conn, ctx)
    return None


@router.get("/me")
def admin_me(ctx: AdminContext = Depends(require_admin)):
    """当前管理员是谁。界面用它判断令牌还有没有效。"""
    return {"admin": {"id": ctx.admin_id, "email": ctx.email}}
