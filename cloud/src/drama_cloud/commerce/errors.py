"""面向客户端的业务错误。

响应体固定为 `{"error": <稳定的机器码>, "message": <给人看的中文说明>}`。
桌面端按 `error` 分支处理，`message` 可以直接展示。
"""

from __future__ import annotations

from typing import Optional


class CommerceError(Exception):
    def __init__(self, status: int, code: str, message: str, retry_after: Optional[int] = None):
        super().__init__(message)
        self.status = status
        self.code = code
        self.message = message
        #: 只有 429 才带：客户端多少秒后可以重试（写进 Retry-After 响应头）。
        self.retry_after = retry_after


def unauthorized(message: str = "登录已失效，请重新登录") -> CommerceError:
    return CommerceError(401, "unauthorized", message)


def rate_limited(retry_after: int, message: str = "操作太频繁，请稍后再试") -> CommerceError:
    return CommerceError(429, "rate_limited", message, retry_after=max(1, int(retry_after)))
