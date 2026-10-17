"""积分与支付的接口（M5a）。

分两类：

* **要登录**：余额、流水、充值档位、下单、查单。作者是本人，按 user_id 限定范围。
* **不要登录**：渠道回调。渠道不会带我们的令牌，只能靠**验签**确认来源。
  所以回调路由：先限流（防刷），再验签（验不过直接拒），最后才入账。
"""

from __future__ import annotations

import logging

from fastapi import APIRouter, Depends, Request, Response

from . import credits, invites, ratelimit
from .deps import client_key, get_conn, require_auth
from .errors import CommerceError
from .payments import base as pay_base
from .payments import notify_url_for, provider_for, service as pay_service
from .ratelimit import Rule
from .schemas import CreateOrderRequest
from .sessions import AuthContext

log = logging.getLogger(__name__)

BALANCE_PER_USER = Rule("credits:balance", 240, 3600)
HISTORY_PER_USER = Rule("credits:history", 120, 3600)
ORDER_PER_USER = Rule("payment:order", 30, 3600)
#: 回调按 IP 限流：渠道可能重投，但不会一秒几十次。挡住刷接口的脚本。
NOTIFY_PER_IP = Rule("payment:notify", 300, 3600)

router = APIRouter(prefix="/api/v1")


def _settings(request: Request):
    return request.app.state.settings


def _origin(request: Request) -> str:
    """本次请求的来源地址，例如 `http://127.0.0.1:8100`。

    只给 Mock 渠道拼收银台地址用：它不经过公网，用请求自己的地址最准确。
    真实渠道一律要求显式配置 `public_base_url`，不走这里。
    """
    return str(request.base_url).rstrip("/")


@router.get("/credits")
def get_credits(
    request: Request,
    ctx: AuthContext = Depends(require_auth),
    conn=Depends(get_conn),
):
    """余额 + 最近流水。一次返回，界面上少一次往返。"""
    ratelimit.hit(conn, BALANCE_PER_USER, ctx.user_id)
    return {
        "balance": credits.balance(conn, ctx.user_id),
        "history": credits.history(conn, ctx.user_id, 20),
    }


@router.get("/credits/history")
def get_history(
    request: Request,
    limit: int = 50,
    ctx: AuthContext = Depends(require_auth),
    conn=Depends(get_conn),
):
    ratelimit.hit(conn, HISTORY_PER_USER, ctx.user_id)
    return {"items": credits.history(conn, ctx.user_id, limit)}


@router.get("/invites")
def get_invites(
    request: Request,
    ctx: AuthContext = Depends(require_auth),
    conn=Depends(get_conn),
):
    """我的邀请码、已邀请人数、已得奖励，以及当前的奖励配置。"""
    ratelimit.hit(conn, BALANCE_PER_USER, ctx.user_id)
    return invites.status(conn, ctx.user_id)


@router.get("/payments/tiers")
def get_tiers(
    request: Request,
    ctx: AuthContext = Depends(require_auth),
    conn=Depends(get_conn),
):
    return {"items": pay_service.list_tiers(conn)}


@router.post("/payments/orders")
def create_order(
    payload: CreateOrderRequest,
    request: Request,
    ctx: AuthContext = Depends(require_auth),
    conn=Depends(get_conn),
):
    """下单，返回怎么去付。渠道没配好会返回 503 `payment_not_configured`。"""
    ratelimit.hit(conn, ORDER_PER_USER, ctx.user_id)
    settings = _settings(request)
    try:
        provider = provider_for(settings, payload.channel, _origin(request))
        notify_url = notify_url_for(settings, payload.channel)
    except pay_base.NotConfigured as error:
        raise CommerceError(503, error.code, error.message) from error
    try:
        return pay_service.create_order(
            conn, ctx.user_id, payload.tier_id, payload.channel, provider, notify_url
        )
    except pay_base.NotConfigured as error:
        # 下单阶段渠道报「没配好」：订单已经落库（created），如实告诉用户。
        raise CommerceError(503, error.code, error.message) from error


@router.get("/payments/orders/{out_trade_no}")
def get_order(
    out_trade_no: str,
    request: Request,
    ctx: AuthContext = Depends(require_auth),
    conn=Depends(get_conn),
):
    return pay_service.get_order(conn, ctx.user_id, out_trade_no)


@router.post("/payments/orders/{out_trade_no}/reconcile")
def reconcile_order(
    out_trade_no: str,
    request: Request,
    ctx: AuthContext = Depends(require_auth),
    conn=Depends(get_conn),
):
    """用户点「我已完成支付」时主动查单（回调丢失的兜底）。"""
    ratelimit.hit(conn, ORDER_PER_USER, ctx.user_id)
    settings = _settings(request)
    row = pay_service.get_order(conn, ctx.user_id, out_trade_no)
    try:
        provider = provider_for(settings, row["channel"])
    except pay_base.NotConfigured as error:
        raise CommerceError(503, error.code, error.message) from error
    try:
        return pay_service.reconcile(conn, ctx.user_id, out_trade_no, provider)
    except pay_base.NotConfigured as error:
        # Mock 没有渠道侧可查：如实说清楚，不要假装「查过了但没付」。
        raise CommerceError(503, error.code, error.message) from error


@router.post("/payments/{channel}/notify")
async def payment_notify(
    channel: str,
    request: Request,
    conn=Depends(get_conn),
):
    """渠道回调。**不要登录**：靠验签确认来源。

    任何一步不过都返回失败：渠道据此重投，我们这边不入账。
    """
    ratelimit.hit(conn, NOTIFY_PER_IP, client_key(request))
    settings = _settings(request)
    body = await request.body()
    try:
        provider = provider_for(settings, channel)
    except pay_base.NotConfigured as error:
        raise CommerceError(503, error.code, error.message) from error

    try:
        event = provider.parse_notify(dict(request.headers), body)
    except pay_base.VerifyFailed as error:
        # 验签失败：记日志（不含报文内容，可能含敏感信息），返回失败让渠道重投。
        log.warning("支付回调验签失败：channel=%s code=%s", channel, error.code)
        status, content = provider.ack(False)
        return Response(content=content, status_code=status, media_type="text/plain")
    except pay_base.NotConfigured as error:
        raise CommerceError(503, error.code, error.message) from error

    try:
        result = pay_service.handle_event(conn, event)
    except CommerceError as error:
        # 订单不存在 / 金额不符：都不入账。
        log.warning("支付回调不能入账：channel=%s code=%s", channel, error.code)
        status, content = provider.ack(False)
        return Response(content=content, status_code=status, media_type="text/plain")

    status, content = provider.ack(True)
    return Response(
        content=content,
        status_code=status,
        media_type="text/plain",
        headers={"X-Credited": str(result["credited"])},
    )
