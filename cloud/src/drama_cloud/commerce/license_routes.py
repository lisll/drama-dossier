"""授权接口：兑换卡密、绑定电脑并领授权凭证、刷新凭证、设备列表、解绑。"""

from __future__ import annotations

import uuid
from typing import Optional

from fastapi import APIRouter, Depends, Query, Request, Response

from . import cards, ratelimit
from .deps import client_key, get_conn, require_auth
from .ratelimit import Rule
from .schemas import ActivateRequest, LicenseRefreshRequest, RedeemRequest
from .sessions import AuthContext

# 卡密有 80 位随机数，限流不是为了防猜，而是挡住脚本刷接口。
REDEEM_PER_USER = Rule("redeem:user", 20, 3600)
REDEEM_PER_IP = Rule("redeem:ip", 60, 3600)
ACTIVATE_PER_USER = Rule("activate:user", 20, 3600)
LICENSE_REFRESH_PER_USER = Rule("license_refresh:user", 120, 3600)
DEVICES_PER_USER = Rule("devices:user", 60, 3600)

router = APIRouter(prefix="/api/v1")


def _licenses(request: Request):
    return request.app.state.licenses


@router.post("/cards/redeem")
def redeem(
    payload: RedeemRequest,
    request: Request,
    ctx: AuthContext = Depends(require_auth),
    conn=Depends(get_conn),
):
    ratelimit.hit(conn, REDEEM_PER_IP, client_key(request))
    ratelimit.hit(conn, REDEEM_PER_USER, ctx.user_id)
    return cards.redeem(conn, ctx.user_id, payload.code)


@router.post("/license/activate")
def activate(
    payload: ActivateRequest,
    request: Request,
    ctx: AuthContext = Depends(require_auth),
    conn=Depends(get_conn),
):
    ratelimit.hit(conn, ACTIVATE_PER_USER, ctx.user_id)
    return _licenses(request).activate(
        conn, ctx, payload.device_id, payload.name, payload.platform, payload.app_version
    )


@router.post("/license/refresh")
def refresh_license(
    payload: LicenseRefreshRequest,
    request: Request,
    ctx: AuthContext = Depends(require_auth),
    conn=Depends(get_conn),
):
    ratelimit.hit(conn, LICENSE_REFRESH_PER_USER, ctx.user_id)
    return _licenses(request).refresh(conn, ctx, payload.device_id, payload.app_version)


@router.get("/devices")
def list_devices(
    request: Request,
    current_device: Optional[str] = Query(None, pattern=r"^[0-9a-f]{64}$"),
    ctx: AuthContext = Depends(require_auth),
    conn=Depends(get_conn),
):
    ratelimit.hit(conn, DEVICES_PER_USER, ctx.user_id)
    return _licenses(request).list_devices(conn, ctx, current_device)


@router.delete("/devices/{binding_id}", status_code=204)
def unbind_device(
    binding_id: uuid.UUID,
    request: Request,
    ctx: AuthContext = Depends(require_auth),
    conn=Depends(get_conn),
):
    ratelimit.hit(conn, DEVICES_PER_USER, ctx.user_id)
    _licenses(request).unbind(conn, ctx, str(binding_id))
    return Response(status_code=204)
