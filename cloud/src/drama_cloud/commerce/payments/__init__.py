"""支付渠道的注册与查找（M5a）。

一个渠道要能被用，必须**同时**满足两件事：代码里有实现，配置里有凭证。
缺任何一样都显式报 `NotConfigured`，绝不降级成「假装成功」。
"""

from __future__ import annotations

from typing import Dict, Optional

from ..config import CommerceSettings
from .base import Checkout, CreateOrder, NotConfigured, PaymentError, PaymentEvent, PaymentProvider, VerifyFailed
from .mock import SIGNATURE_HEADER, MockProvider

#: 目前支持的渠道。
CHANNELS = ("mock", "alipay", "wechat")


def provider_for(settings: CommerceSettings, channel: str, origin: Optional[str] = None) -> PaymentProvider:
    """按渠道名取一个可用的 provider。没配好就抛 `NotConfigured`。

    `origin` 是本次请求的来源，只有 Mock 渠道会用它补全收银台地址（见 `MockProvider`）。
    """
    if channel == "mock":
        return MockProvider(settings.mock_pay_secret, settings.public_base_url, origin)
    if channel == "alipay":
        # 5a-7 才接真实沙箱；现在只有占位实现，配置不全时明确报错。
        from .alipay import AlipayProvider

        return AlipayProvider.from_settings(settings)
    if channel == "wechat":
        from .wechat import WechatProvider

        return WechatProvider.from_settings(settings)
    raise NotConfigured(f"不认识的支付渠道：{channel}")


def notify_url_for(settings: CommerceSettings, channel: str) -> str:
    """拼回调地址。

    没有对外地址时：Mock 用相对路径（浏览器同源即可），真实渠道明确报错——
    渠道根本推不到我们的内网服务，拼一个假地址只会让人以为配好了。
    """
    path = f"/api/v1/payments/{channel}/notify"
    base = (settings.public_base_url or "").rstrip("/")
    if base:
        return f"{base}{path}"
    if channel == "mock":
        return path
    raise NotConfigured(
        "没有配置对外地址（DRAMA_COMMERCE_PUBLIC_BASE_URL），支付渠道无法把结果推回来"
    )


__all__ = [
    "CHANNELS",
    "Checkout",
    "CreateOrder",
    "NotConfigured",
    "PaymentError",
    "PaymentEvent",
    "PaymentProvider",
    "SIGNATURE_HEADER",
    "VerifyFailed",
    "notify_url_for",
    "provider_for",
]
