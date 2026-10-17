"""本地模拟支付（M5a）。

用途：没有商户号也能把「下单 → 支付 → 回调 → 入账」整条链路跑通。

**关键设计：回调走的是和真实渠道完全相同的代码路径。** 唯一不同的是签名算法
（本地 HMAC-SHA256，而不是 RSA2 / AES-GCM 解密）。这样能真正测到回调验签、
金额校验、重复回调去重；如果 Mock 走一条「直接入账」的捷径，这些就白测了。
"""

from __future__ import annotations

import hashlib
import hmac
import json
from datetime import datetime, timezone
from typing import Mapping

from .base import Checkout, CreateOrder, NotConfigured, PaymentEvent, VerifyFailed

#: Mock 回调的签名头名。
SIGNATURE_HEADER = "X-Mock-Signature"


class MockProvider:
    """模拟收银台。

    `secret` 由服务端配置提供（和支付相关的密钥一样，只从环境变量读）。
    `base_url` 用来拼「收银台页面」地址——它指向本服务自己的一个页面，
    页面上有「支付成功 / 支付失败」等按钮。

    `origin` 是本次请求的来源（如 `http://127.0.0.1:8100`）。没配 `public_base_url`
    时用它补全地址，让桌面端拿到的收银台地址是**可直接打开的完整地址**：
    只给相对路径的话，界面里只能显示一段点不开的文本，本地闭环就走不下去。
    """

    channel = "mock"

    def __init__(self, secret: str, base_url: str | None, origin: str | None = None):
        if not secret:
            raise NotConfigured("模拟支付缺少签名密钥（DRAMA_COMMERCE_MOCK_PAY_SECRET）")
        self._secret = secret.encode("utf-8")
        self._base_url = (base_url or origin or "").rstrip("/")

    def create(self, order: CreateOrder) -> Checkout:
        # 收银台页面地址：把订单号带上，页面据此发起「支付」。
        # `_base_url` 是 public_base_url，没有就用本次请求的来源；
        # 两者都没有（只可能发生在脱离 HTTP 的调用里）才退回相对路径。
        path = f"/pay/mock/checkout?out_trade_no={order.out_trade_no}"
        return Checkout(kind="redirect", payload=f"{self._base_url}{path}")

    def _sign(self, body: bytes) -> str:
        return hmac.new(self._secret, body, hashlib.sha256).hexdigest()

    def parse_notify(self, headers: Mapping[str, str], body: bytes) -> PaymentEvent:
        """验签后解析。签名不对就抛 `VerifyFailed`，绝不返回未验证的数据。"""
        provided = ""
        for key, value in headers.items():
            if key.lower() == SIGNATURE_HEADER.lower():
                provided = value.strip()
                break
        if not provided:
            raise VerifyFailed("模拟支付回调缺少签名")
        expected = self._sign(body)
        # 用 compare_digest 防时序侧信道：和真实渠道的验签一样认真。
        if not hmac.compare_digest(provided, expected):
            raise VerifyFailed("模拟支付回调签名不正确")
        try:
            payload = json.loads(body.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise VerifyFailed("模拟支付回调不是合法 JSON") from error
        return _event_from_payload(payload)

    def ack(self, ok: bool) -> tuple[int, bytes]:
        # Mock 沿用「返回 JSON」的形态；真实渠道各有各的要求（支付宝是 `success`）。
        if ok:
            return 200, b'{"ok":true}'
        return 400, b'{"ok":false}'

    def query(self, out_trade_no: str) -> PaymentEvent:
        # Mock 没有「渠道侧」可查：离线环境下无法确认任何状态。
        # 这一点必须如实报错，不能假装「查到未支付」——那会让对账以为真的查过了。
        raise NotConfigured(
            "模拟支付没有渠道侧可查单；请用本地收银台页面完成支付，或接入真实渠道后再用查单"
        )

    def sign(self, payload: dict) -> tuple[bytes, str]:
        """给测试与本地收银台用：把 payload 序列化并签名。"""
        body = json.dumps(payload, ensure_ascii=False, sort_keys=True).encode("utf-8")
        return body, self._sign(body)


def _event_from_payload(payload: dict) -> PaymentEvent:
    """把回调 JSON 变成 `PaymentEvent`。字段缺失或类型不对都算验签失败。"""
    try:
        out_trade_no = str(payload["out_trade_no"])
        channel_trade_no = str(payload["channel_trade_no"])
        amount_fen = int(payload["amount_fen"])
        status = str(payload["status"])
    except (KeyError, TypeError, ValueError) as error:
        raise VerifyFailed("模拟支付回调缺少必要字段") from error
    if status not in ("paid", "closed", "refunded"):
        raise VerifyFailed(f"模拟支付回调的状态不认识：{status}")
    paid_at = None
    raw_paid_at = payload.get("paid_at")
    if raw_paid_at:
        try:
            paid_at = datetime.fromisoformat(str(raw_paid_at).replace("Z", "+00:00"))
        except ValueError as error:
            raise VerifyFailed("模拟支付回调的时间格式不正确") from error
    elif status == "paid":
        # 已支付却没有时间：补成当前时间，但这是异常情况，回调方应当给。
        paid_at = datetime.now(timezone.utc)
    return PaymentEvent(
        out_trade_no=out_trade_no,
        channel_trade_no=channel_trade_no,
        amount_fen=amount_fen,
        status=status,
        paid_at=paid_at,
    )
