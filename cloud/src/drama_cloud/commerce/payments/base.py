"""支付渠道的公共接口（M5a）。

接口按支付宝 / 微信的真实流程设计（M0 调研报告 3.3 节），不为了省事简化：

* 下单返回「怎么去付」（跳转地址或二维码内容）；
* 回调**先验签再取数据**，验签失败直接抛异常，绝不返回没验证过的内容；
* 应答按各渠道要求给（支付宝成功返回字符串 `success`，微信成功返回 200/204）；
* 主动查单，用于「回调丢了」的兜底。

一条硬规则：**任何渠道的实现都不许自己改账本。** 入账只有一条路——
`payments.service.handle_event()`，它负责订单锁、金额校验、幂等入账。
渠道实现只负责「把渠道的报文变成 PaymentEvent」。
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime
from typing import Mapping, Optional, Protocol


class PaymentError(Exception):
    """支付渠道层面的错误。

    `code` 是给客户端看的稳定机器码；`message` 是中文说明。
    """

    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code
        self.message = message


class NotConfigured(PaymentError):
    """渠道没配好（缺商户号、缺密钥、缺对外地址）。

    这一类错误必须显式报出来：静默降级成「假装支付成功」是绝对不能发生的事。
    """

    def __init__(self, message: str):
        super().__init__("payment_not_configured", message)


class VerifyFailed(PaymentError):
    """回调验签或解密失败。绝不入账。"""

    def __init__(self, message: str):
        super().__init__("payment_verify_failed", message)


@dataclass(frozen=True)
class CreateOrder:
    """向渠道下单需要的参数。"""

    #: 我方订单号，全局唯一（回调按它去重）。
    out_trade_no: str
    #: 金额，单位是分。只用整数——这是钱，不走浮点。
    amount_fen: int
    #: 商品标题（用户在收银台能看到）。
    subject: str
    #: 这个订单什么时候失效。
    expire_at: datetime
    #: 回调地址（渠道把支付结果推到这里）。
    notify_url: str


@dataclass(frozen=True)
class Checkout:
    """下单结果：告诉客户端「怎么去付」。"""

    #: `redirect` 跳转到收银台（支付宝电脑网站支付）；`qrcode` 展示二维码内容（微信 Native）。
    kind: str
    #: 跳转地址，或二维码里要放的字符串。
    payload: str


@dataclass(frozen=True)
class PaymentEvent:
    """回调或查单得到的统一结果（已验签 / 已解密）。"""

    out_trade_no: str
    #: 渠道侧订单号（支付宝 `trade_no`、微信 `transaction_id`）。Mock 用自己的。
    channel_trade_no: str
    amount_fen: int
    #: `paid` / `closed` / `refunded`
    status: str
    paid_at: Optional[datetime] = None


class PaymentProvider(Protocol):
    """一个支付渠道。"""

    #: 渠道标识：`mock` / `alipay` / `wechat`。
    channel: str

    def create(self, order: CreateOrder) -> Checkout:
        """下单。配不齐参数时抛 `NotConfigured`，不猜、不降级。"""

    def parse_notify(self, headers: Mapping[str, str], body: bytes) -> PaymentEvent:
        """把渠道回调变成 `PaymentEvent`。验签失败抛 `VerifyFailed`。"""

    def ack(self, ok: bool) -> tuple[int, bytes]:
        """按渠道要求应答回调。

        支付宝成功时正文必须是 `success`；微信成功时返回 200 或 204 且无正文。
        """

    def query(self, out_trade_no: str) -> PaymentEvent:
        """主动查单（回调丢失时的兜底）。"""
