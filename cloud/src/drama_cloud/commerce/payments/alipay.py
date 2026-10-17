"""支付宝（M5a 只做占位实现，5a-7 才接真实沙箱）。

真实流程按 M0 调研报告 3.1 节：

* 电脑网站支付 `alipay.trade.page.pay`：拼参数 → 签名 → 拼 URL → 跳转收银台；
* 异步通知：去掉 `sign`、`sign_type`，其余参数按参数名升序拼串，用**支付宝公钥**做 RSA2 验签；
  处理成功后正文必须回 `success`（否则支付宝会一直重投）；
* 查单 `alipay.trade.query`；
* 沙箱网关：`https://openapi-sandbox.dl.alipaydev.com/gateway.do`（旧文档还有
  `openapi.alipaydev.com` 的写法，对接时以官方为准）。

现在没配凭证时一律抛 `NotConfigured`。**不实现一个假的「总是成功」的验签**——
那比不做更危险。
"""

from __future__ import annotations

from typing import Mapping

from ..config import CommerceSettings
from .base import Checkout, CreateOrder, NotConfigured, PaymentEvent

#: 沙箱网关（M0 核对过的地址）。生产网关是 `https://openapi.alipay.com/gateway.do`。
SANDBOX_GATEWAY = "https://openapi-sandbox.dl.alipaydev.com/gateway.do"
PRODUCTION_GATEWAY = "https://openapi.alipay.com/gateway.do"


class AlipayProvider:
    """占位实现：接口按真实流程留好，但还没接真凭证。"""

    channel = "alipay"

    def __init__(self, app_id: str, private_key: str, alipay_public_key: str, gateway: str):
        self._app_id = app_id
        self._private_key = private_key
        self._alipay_public_key = alipay_public_key
        self._gateway = gateway

    @classmethod
    def from_settings(cls, settings: CommerceSettings) -> "AlipayProvider":
        missing = [
            name
            for name, value in (
                ("DRAMA_COMMERCE_ALIPAY_APP_ID", settings.alipay_app_id),
                ("DRAMA_COMMERCE_ALIPAY_PRIVATE_KEY", settings.alipay_private_key),
                ("DRAMA_COMMERCE_ALIPAY_PUBLIC_KEY", settings.alipay_public_key),
            )
            if not value
        ]
        if missing:
            raise NotConfigured("支付宝未配置：" + "、".join(missing))
        return cls(
            app_id=settings.alipay_app_id,
            private_key=settings.alipay_private_key,
            alipay_public_key=settings.alipay_public_key,
            gateway=settings.alipay_gateway or SANDBOX_GATEWAY,
        )

    def create(self, order: CreateOrder) -> Checkout:
        # 真实实现要做：拼 biz_content → 参数签名 → 拼成 GET URL。
        # 现在凭证齐了也不做——签名算法没实现就发出去，只会得到一个看不懂的渠道错误。
        raise NotConfigured(
            "支付宝下单还没实现（M5a-7 接沙箱时补）。现在请用渠道 mock 走完整链路"
        )

    def parse_notify(self, headers: Mapping[str, str], body: bytes) -> PaymentEvent:
        raise NotConfigured("支付宝回调验签还没实现（M5a-7 补）")

    def ack(self, ok: bool) -> tuple[int, bytes]:
        # 支付宝的约定：成功时正文是字符串 `success`，否则它会持续重投。
        return (200, b"success") if ok else (400, b"fail")

    def query(self, out_trade_no: str) -> PaymentEvent:
        raise NotConfigured("支付宝查单还没实现（M5a-7 补）")
