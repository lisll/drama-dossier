"""业务错误：错误码给程序判断，message 直接展示给用户。"""

from __future__ import annotations


class LicenseError(Exception):
    def __init__(self, code: str, message: str, status: int = 400, retry_after: int | None = None):
        super().__init__(message)
        self.code = code
        self.message = message
        self.status = status
        self.retry_after = retry_after

    def to_dict(self) -> dict:
        body = {"error": self.code, "message": self.message}
        if self.retry_after is not None:
            body["retry_after"] = self.retry_after
        return body
