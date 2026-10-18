"""向 commerce 预扣 / 结算 / 退还积分（M5b-4）。

**为什么扣费要走 commerce**：报审包、本机处理版本扣的都是 commerce 的积分账本，
「服务端处理」再另起一本账，用户会看到两个余额、对不上。所以这里只做一件事：
把「扣多少、成功还是失败」翻译成 commerce 的内部接口调用。

**三条硬规则**：

1. 预扣在**建任务之前**：没扣到钱不该占队列资源；
2. 结算只发生在任务**成功**时；
3. 失败 / 取消一律退还。

**关于「问不到 commerce」**：预扣阶段直接让请求失败（503），绝不放行
「先跑着、回头再扣」——那会变成免费算力。

**关于结算/退还失败**：不吞掉，但也不把已经跑成功的任务判成失败。
做法是把失败**显式返回**给调用方，由调用方记进任务结果与审计，
运维能查得到。静默吞掉会让钱悄悄消失。
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from typing import Optional

import httpx

from .errors import ErrorClass, WorkError

log = logging.getLogger(__name__)

#: 调用超时（秒）。扣费是同步动作，宁可让用户重试也不要挂很久。
CREDIT_TIMEOUT_SECONDS = 8.0

SECRET_HEADER = "X-Internal-Secret"


class CreditsUnavailable(Exception):
    """问不到 commerce，或 commerce 回了意外结果。调用方应当返回 503。"""


class InsufficientCredits(Exception):
    """余额不足。带着「需要多少、当前多少」，界面要能直接显示。"""

    def __init__(self, required: int, balance: int, message: str):
        super().__init__(message)
        self.required = required
        self.balance = balance
        self.message = message


@dataclass(frozen=True)
class CreditOutcome:
    """结算/退还的结果。`ok=False` 表示这一步没成功，需要人工对账。"""

    ok: bool
    detail: str = ""

    def __bool__(self) -> bool:
        return self.ok


def is_configured(base_url: Optional[str], internal_secret: Optional[str]) -> bool:
    """两个都配了才算接上了计费。

    单独抽成函数是为了能直接测：`CreditClient` 在测试里常被打桩，
    这条判定不该因为打桩就测不到。
    """
    return bool((base_url or "").strip() and (internal_secret or "").strip())


class CreditClient:
    """commerce 积分账本的瘦客户端。"""

    def __init__(self, base_url: Optional[str], internal_secret: Optional[str]):
        self._base = (base_url or "").rstrip("/")
        self._secret = internal_secret or ""

    @property
    def configured(self) -> bool:
        return is_configured(self._base, self._secret)

    def _post(self, path: str, payload: dict) -> dict:
        if not self.configured:
            raise CreditsUnavailable(
                "没有配置 commerce 地址或内部密钥（DRAMA_CLOUD_COMMERCE_URL / "
                "DRAMA_CLOUD_INTERNAL_SECRET），无法扣费"
            )
        try:
            response = httpx.post(
                f"{self._base}{path}",
                json=payload,
                headers={SECRET_HEADER: self._secret},
                timeout=CREDIT_TIMEOUT_SECONDS,
            )
        except httpx.HTTPError as error:
            log.warning("无法连接 commerce 计费：%s", type(error).__name__)
            raise CreditsUnavailable("账号服务暂时不可用，无法扣费") from error

        if response.status_code == 402:
            body = response.json() if response.content else {}
            raise InsufficientCredits(
                required=int(body.get("required") or 0),
                balance=int(body.get("balance") or 0),
                message=str(body.get("message") or "积分不足"),
            )
        if response.status_code == 404:
            # 内部接口没开（没配 INTERNAL_SECRET）或找不到预扣记录。
            raise CreditsUnavailable("账号服务的计费接口不可用")
        if response.status_code >= 400:
            log.error("commerce 计费返回 %s", response.status_code)
            raise CreditsUnavailable("账号服务返回了意外结果")
        try:
            return response.json()
        except ValueError as error:
            raise CreditsUnavailable("账号服务返回了意外结果") from error

    # ---------- 三个动作 ----------

    def balance(self, user_id: str) -> int:
        return int(self._post("/internal/credits/balance", {"user_id": user_id})["balance"])

    def cost(self, subject: str, units: int) -> int:
        """按服务端价目表算价。价格只由服务端决定。"""
        return int(self._post("/internal/credits/cost", {"subject": subject, "units": units})["required"])

    def hold(self, user_id: str, amount: int, ref: str, memo: str = "") -> str:
        """预扣。余额不足抛 `InsufficientCredits`，问不到抛 `CreditsUnavailable`。

        返回 hold_key。同一个 `ref` 重放不会重复扣（commerce 那边幂等）。
        """
        if amount <= 0:
            raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "预扣数量必须大于 0")
        body = self._post(
            "/internal/credits/hold",
            {"user_id": user_id, "amount": amount, "ref": ref, "memo": memo},
        )
        return str(body["hold_key"])

    def settle(self, ref: str) -> CreditOutcome:
        """结算。**不抛异常**：返回 `CreditOutcome`，调用方据它记审计。

        为什么不抛：此时任务已经跑完了。把「钱没结算成功」变成任务失败，
        用户会以为视频没做出来；真实情况是视频好了、账没对上。
        两种情况都要让人知道，但要用不同的方式表达。
        """
        try:
            self._post("/internal/credits/settle", {"ref": ref})
        except CreditsUnavailable as error:
            log.error("任务 %s 结算失败，需要人工对账：%s", ref, error)
            return CreditOutcome(ok=False, detail=str(error))
        return CreditOutcome(ok=True)

    def release(self, ref: str, memo: str = "") -> CreditOutcome:
        """退还。同样返回结果而不抛，理由同上（视频没做成，但钱要退得掉）。"""
        try:
            self._post("/internal/credits/release", {"ref": ref, "memo": memo})
        except CreditsUnavailable as error:
            log.error("任务 %s 退还失败，需要人工对账：%s", ref, error)
            return CreditOutcome(ok=False, detail=str(error))
        return CreditOutcome(ok=True)
