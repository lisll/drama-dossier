"""向 commerce 询问「这个访问令牌是谁」（M5b）。

媒体控制面没有自己的账号体系，令牌校验要交给 commerce。这里只管一件小事：
发一次 HTTP 请求，把三种结果分清楚——

* 有效 → 返回 `(user_id, email)`；
* 无效（401/404）→ 返回 `None`，调用方据此给客户端 401；
* **问不到**（网络错、超时、commerce 挂了、5xx）→ 抛 `IntrospectUnavailable`。

第三种必须单独区分：把「问不到」当成「令牌无效」会让人以为要重新登录，
而把「问不到」当成「有效」等于没鉴权。**两者都不能做**，只能如实报服务不可用。
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from typing import Optional

import httpx

log = logging.getLogger(__name__)

#: 内省请求超时（秒）。这是登录链路上的同步调用，不能拖太久；
#: 但也不能太短，否则 commerce 稍一抖动就把所有人都挡在门外。
INTROSPECT_TIMEOUT_SECONDS = 5.0

SECRET_HEADER = "X-Internal-Secret"
TOKEN_HEADER = "X-Access-Token"


class IntrospectUnavailable(Exception):
    """问不到 commerce（网络、超时、5xx）。调用方应当返回 503，不是 401。"""


@dataclass(frozen=True)
class Identity:
    user_id: str
    email: str


def introspect(base_url: str, internal_secret: str, access_token: str) -> Optional[Identity]:
    """校验访问令牌。

    返回 `Identity` 表示有效；`None` 表示 commerce 明确说这个令牌无效。
    问不到时抛 `IntrospectUnavailable`。
    """
    if not base_url:
        raise IntrospectUnavailable("没有配置 commerce 地址（DRAMA_CLOUD_COMMERCE_URL）")
    if not internal_secret:
        raise IntrospectUnavailable("没有配置内部接口密钥（DRAMA_CLOUD_INTERNAL_SECRET）")

    url = base_url.rstrip("/") + "/internal/introspect"
    try:
        response = httpx.post(
            url,
            headers={SECRET_HEADER: internal_secret, TOKEN_HEADER: access_token},
            timeout=INTROSPECT_TIMEOUT_SECONDS,
        )
    except httpx.HTTPError as error:
        # 只记异常类型：URL 里没有敏感信息，但异常链可能带请求头，不打全量。
        log.warning("无法连接 commerce 做令牌内省：%s", type(error).__name__)
        raise IntrospectUnavailable("账号服务暂时不可用") from error

    if response.status_code in (401, 404):
        # 401：令牌无效/过期/吊销；404：commerce 没开这个接口（没配内部密钥）。
        # 两者都不该让用户「进去」，但 404 其实是部署问题，要留痕。
        if response.status_code == 404:
            log.error("commerce 没有开启 /internal/introspect（未配置 INTERNAL_SECRET？）")
        return None
    if response.status_code >= 500:
        log.warning("commerce 内省返回 %s", response.status_code)
        raise IntrospectUnavailable("账号服务暂时不可用")
    if response.status_code != 200:
        # 其余状态码属于契约不符，按不可用处理，不猜。
        log.error("commerce 内省返回了意外的状态码 %s", response.status_code)
        raise IntrospectUnavailable("账号服务返回了意外结果")

    try:
        payload = response.json()
        user_id = str(payload["user_id"])
        email = str(payload["email"])
    except (ValueError, KeyError, TypeError) as error:
        log.error("commerce 内省返回的报文不符合约定")
        raise IntrospectUnavailable("账号服务返回了意外结果") from error
    if not user_id or not email:
        raise IntrospectUnavailable("账号服务返回了空的用户标识")
    return Identity(user_id=user_id, email=email)
