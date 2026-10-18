"""内部接口：给同机房的其他服务用（M5b）。

**为什么需要它**：桌面端的登录状态在 commerce 里（邮箱注册、访问令牌）。
媒体控制面（跑视频任务的那个服务）也要知道「这个请求是谁」，但它自己
没有账号体系。与其让用户登录两次、再人工对齐两套用户，不如让媒体控制面
拿桌面端已经有的访问令牌来问一句「这是谁」。

**安全边界**：

* 只监听内网（部署侧的事），并用共享密钥 `DRAMA_COMMERCE_INTERNAL_SECRET` 做调用方认证。
* **密钥留空时这些路由一律 404**，不是「放行」。理由：留空若等于不鉴权，
  一次配置遗漏就会让任何人都能拿令牌换用户身份。
* 待验证的访问令牌走**请求头**，不进请求体——请求体会被框架记进错误日志，
  令牌不该出现在日志里。
* 只返回 `user_id` / `email`，不返回令牌、密码摘要、设备等任何其他东西。
* 单独的限流桶：调用方是服务而不是人，给得宽一些，但仍要防被打爆。
"""

from __future__ import annotations

import hmac
import logging

from fastapi import APIRouter, Depends, Header, Request

from . import ratelimit
from .deps import client_key, get_conn
from .errors import CommerceError, unauthorized
from .ratelimit import Rule
from .schemas import StrictModel
from pydantic import Field
from typing import Optional

log = logging.getLogger(__name__)

router = APIRouter(prefix="/internal", include_in_schema=False)

#: 内部接口的限流：调用方是服务，按来源放宽到每分钟 6000 次。
INTROSPECT_PER_CALLER = Rule("internal:introspect", 6000, 60)
CREDIT_PER_CALLER = Rule("internal:credits", 6000, 60)

#: 调用方认证用的共享密钥。用自定义头而不是 Authorization，
#: 免得和「用户令牌」的 Bearer 语义混在一起。
SECRET_HEADER = "X-Internal-Secret"
#: 待验证的访问令牌。
TOKEN_HEADER = "X-Access-Token"


@router.post("/introspect")
def introspect(
    request: Request,
    conn=Depends(get_conn),
    internal_secret: str = Header(default="", alias=SECRET_HEADER),
    access_token: str = Header(default="", alias=TOKEN_HEADER),
):
    """用一个访问令牌换取「这是谁」。令牌无效/过期/已吊销都返回 401。"""
    expected = request.app.state.settings.internal_secret
    if not expected:
        # 没配密钥 = 这个接口不存在。绝不能退化成「不鉴权就放行」。
        raise CommerceError(404, "not_found", "没有这个接口")
    # 恒定时间比较：和验签一样认真，避免从响应时间反推密钥。
    if not hmac.compare_digest(internal_secret, expected):
        log.warning("内部接口密钥不正确：path=%s", request.url.path)
        raise unauthorized("内部接口密钥不正确")

    ratelimit.hit(conn, INTROSPECT_PER_CALLER, client_key(request))

    token = (access_token or "").strip()
    if not token:
        raise CommerceError(422, "invalid_request", "缺少访问令牌")

    ctx = request.app.state.service.authenticate(conn, token)
    if ctx is None:
        raise unauthorized()
    row = conn.execute(
        "SELECT id, email FROM users WHERE id = %s::uuid", (ctx.user_id,)
    ).fetchone()
    if row is None:
        # 令牌有效但用户已被注销：当作无效令牌。
        raise unauthorized()
    return {"user_id": str(row["id"]), "email": row["email"]}


# ---------- 积分：给「服务端处理」用（M5b-4） ----------
#
# 媒体控制面要替用户跑视频，扣的必须是**同一本账**（commerce 的 credit_ledger）。
# 两套账本并存会让人不知道看哪个，也说不清「服务端处理扣了 A、报审包扣了 B」。
# 所以这里把预扣/结算/退还开成内部接口，媒体控制面通过它调用。

def _guard(request: Request, conn, rule: Rule) -> None:
    """内部接口的公共前置：先验调用方密钥，再限流。"""
    expected = request.app.state.settings.internal_secret
    if not expected:
        raise CommerceError(404, "not_found", "没有这个接口")
    provided = request.headers.get(SECRET_HEADER, "")
    if not hmac.compare_digest(provided, expected):
        log.warning("内部接口密钥不正确：path=%s", request.url.path)
        raise unauthorized("内部接口密钥不正确")
    ratelimit.hit(conn, rule, client_key(request))


class HoldRequest(StrictModel):
    """预扣积分。`ref` 是对账用的引用（例如 `server_task:<task_id>`）。"""

    user_id: str
    amount: int = Field(gt=0)
    ref: str = Field(min_length=1, max_length=200)
    memo: str = Field(default="", max_length=200)


class HoldRefRequest(StrictModel):
    """对一笔已存在的预扣做结算或退还。"""

    ref: str = Field(min_length=1, max_length=200)
    memo: str = Field(default="", max_length=200)


class BalanceRequest(StrictModel):
    user_id: str


@router.post("/credits/balance")
def internal_balance(
    payload: BalanceRequest,
    request: Request,
    conn=Depends(get_conn),
):
    """查余额（媒体控制面在下单前显示价格时用）。"""
    from . import credits

    _guard(request, conn, CREDIT_PER_CALLER)
    return {"balance": credits.balance(conn, payload.user_id)}


class CostRequest(StrictModel):
    """按计价对象与数量算价（服务端处理按集计价）。"""

    subject: str = Field(min_length=1, max_length=32)
    units: int = Field(gt=0)


@router.post("/credits/cost")
def internal_cost(
    payload: CostRequest,
    request: Request,
    conn=Depends(get_conn),
):
    """算价。价格只由服务端决定，客户端不能传。"""
    from . import credits

    _guard(request, conn, CREDIT_PER_CALLER)
    required = credits.cost_for(conn, payload.subject, payload.units)
    return {"subject": payload.subject, "units": payload.units, "required": required}


@router.post("/credits/hold")
def internal_hold(
    payload: HoldRequest,
    request: Request,
    conn=Depends(get_conn),
):
    """预扣积分。余额不足时 402，与对外接口同一个错误码。

    幂等键由 `ref` 派生：同一个 ref 重复调用不会重复扣（媒体控制面在
    「建任务」这一步会重试，必须能安全重放）。
    """
    from . import credits

    _guard(request, conn, CREDIT_PER_CALLER)
    # `hold` 要求在调用方已开的事务里执行：余额检查与写入之间不能有人插进来。
    with conn.transaction():
        key = credits.hold(
            conn, payload.user_id, payload.amount,
            ref=payload.ref, memo=payload.memo or "服务端处理", actor="media-server",
        )
    return {"hold_key": key, "balance": credits.balance(conn, payload.user_id)}


@router.post("/credits/settle")
def internal_settle(
    payload: HoldRefRequest,
    request: Request,
    conn=Depends(get_conn),
):
    """结算：任务成功，预扣转为已消费。幂等。"""
    from . import credits

    _guard(request, conn, CREDIT_PER_CALLER)
    key = credits.hold_key_for(conn, payload.ref)
    if key is None:
        # 没有这笔预扣（例如任务在扣费之前就失败了）：如实返回而不是假装结算了。
        raise CommerceError(404, "hold_not_found", "找不到对应的预扣记录")
    credits.settle(conn, key, actor="media-server")
    return {"settled": True}


@router.post("/credits/release")
def internal_release(
    payload: HoldRefRequest,
    request: Request,
    conn=Depends(get_conn),
):
    """退还：任务失败或被取消，预扣原路退回。幂等。"""
    from . import credits

    _guard(request, conn, CREDIT_PER_CALLER)
    key = credits.hold_key_for(conn, payload.ref)
    if key is None:
        raise CommerceError(404, "hold_not_found", "找不到对应的预扣记录")
    credits.release(conn, key, memo=payload.memo or "任务未完成，已退还", actor="media-server")
    return {"released": True}
