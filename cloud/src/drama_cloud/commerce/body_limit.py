"""请求体大小上限（ASGI 中间件）。

字段级的长度、数量限制要等整个请求体读进内存、解析完 JSON 才生效；这里在读的过程中就挡住超大请求，
免得一个几百 MB 的请求把服务的内存撑满。

- 一般接口：1 MiB（账号、卡密、配方的请求体都远小于这个）。
- `POST /api/v1/packages`：1 MiB + 4 份扫描件（每份 base64 之后最多 `SCAN_B64_MAX`）。

先看 `Content-Length`，超了直接拒；没有或谎报 `Content-Length` 时，先收齐请求体再交给应用，收的过程中一超就拒。
"""

from __future__ import annotations

import json

from .schemas import SCAN_B64_MAX

DEFAULT_LIMIT = 1 << 20
#: 扫描件最多 4 份（见 schemas.PackageRequest）。
PACKAGE_LIMIT = DEFAULT_LIMIT + 4 * SCAN_B64_MAX
PACKAGE_PATH = "/api/v1/packages"

_BODY = json.dumps(
    {"error": "payload_too_large", "message": "请求内容太大"}, ensure_ascii=False
).encode("utf-8")


def limit_for(path: str) -> int:
    return PACKAGE_LIMIT if path.rstrip("/") == PACKAGE_PATH else DEFAULT_LIMIT


class BodyLimitMiddleware:
    def __init__(self, app):
        self.app = app

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http":
            await self.app(scope, receive, send)
            return
        limit = limit_for(scope.get("path", ""))
        for name, value in scope.get("headers", ()):
            if name == b"content-length":
                try:
                    declared = int(value)
                except ValueError:
                    declared = limit + 1
                if declared > limit:
                    await _reject(send)
                    return

        # 没有或谎报 Content-Length 时，先在这里把请求体收齐（最多 limit 字节），超了直接回 413，
        # 不进应用：商业接口都是小 JSON，本来就要整体读进内存再解析，提前收齐不多占内存；
        # 也避免「应用读到一半被打断」抛出 ClientDisconnect、在服务日志里留一条错误堆栈。
        chunks: list[bytes] = []
        received = 0
        while True:
            message = await receive()
            if message["type"] == "http.disconnect":
                return
            if message["type"] != "http.request":
                continue
            body = message.get("body", b"")
            received += len(body)
            if received > limit:
                await _reject(send)
                return
            chunks.append(body)
            if not message.get("more_body", False):
                break

        replayed = False

        async def replay_receive():
            nonlocal replayed
            if not replayed:
                replayed = True
                return {"type": "http.request", "body": b"".join(chunks), "more_body": False}
            # 请求体已经交完；之后应用再等消息，只可能是在等客户端断开。
            return await receive()

        await self.app(scope, replay_receive, send)


async def _reject(send):
    await send({
        "type": "http.response.start",
        "status": 413,
        "headers": [(b"content-type", b"application/json; charset=utf-8"),
                    (b"content-length", str(len(_BODY)).encode("ascii"))],
    })
    await send({"type": "http.response.body", "body": _BODY})
