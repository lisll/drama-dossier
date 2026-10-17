"""微信支付（M5a 只做占位实现）。

真实流程按 M0 调研报告 3.2 节：

* Native 下单 `POST /v3/pay/transactions/native`，返回 `code_url`（有效期 2 小时）；
* 支付成功回调：报文用 APIv3 密钥按 `AEAD_AES_256_GCM` 解密；验签串是
  「时间戳\\n随机串\\n报文主体\\n」，用平台证书或微信支付公钥做 SHA256-with-RSA 验签；
  要在 5 秒内应答；
* 查单 `GET /v3/pay/transactions/out-trade-no/{out_trade_no}`。

**这一期只能模拟**：个人申请不了商户号（准入条件明确要求个体工商户 / 企业等主体），
而且 APIv3 没有官方沙箱。所以这里只有接口，且没配凭证时一律抛 `NotConfigured`。
"""

from __future__ import annotations

from typing import Mapping

from ..config import CommerceSettings
from .base import Checkout, CreateOrder, NotConfigured, PaymentEvent

#: 微信支付的 API 根地址。
API_BASE = "https://api.mch.weixin.qq.com"


class WechatProvider:
    """占位实现：接口按真实流程留好，但个人主体无法接入。"""

    channel = "wechat"

    def __init__(self, mch_id: str, api_v3_key: str, private_key: str, serial_no: str):
        self._mch_id = mch_id
        self._api_v3_key = api_v3_key
        self._private_key = private_key
        self._serial_no = serial_no

    @classmethod
    def from_settings(cls, settings: CommerceSettings) -> "WechatProvider":
        missing = [
            name
            for name, value in (
                ("DRAMA_COMMERCE_WECHAT_MCH_ID", settings.wechat_mch_id),
                ("DRAMA_COMMERCE_WECHAT_API_V3_KEY", settings.wechat_api_v3_key),
                ("DRAMA_COMMERCE_WECHAT_PRIVATE_KEY", settings.wechat_private_key),
                ("DRAMA_COMMERCE_WECHAT_SERIAL_NO", settings.wechat_serial_no),
            )
            if not value
        ]
        if missing:
            raise NotConfigured(
                "微信支付未配置：" + "、".join(missing)
                + "。个人主体申请不了商户号，本期只能用渠道 mock"
            )
        return cls(
            mch_id=settings.wechat_mch_id,
            api_v3_key=settings.wechat_api_v3_key,
            private_key=settings.wechat_private_key,
            serial_no=settings.wechat_serial_no,
        )

    def create(self, order: CreateOrder) -> Checkout:
        raise NotConfigured("微信支付下单还没实现（个人主体无法接入，仅留接口）")

    def parse_notify(self, headers: Mapping[str, str], body: bytes) -> PaymentEvent:
        raise NotConfigured("微信支付回调解密与验签还没实现（仅留接口）")

    def ack(self, ok: bool) -> tuple[int, bytes]:
        # 微信的约定：成功返回 200 或 204，且正文为空。
        return (204 if ok else 400), b""

    def query(self, out_trade_no: str) -> PaymentEvent:
        raise NotConfigured("微信支付查单还没实现（仅留接口）")
