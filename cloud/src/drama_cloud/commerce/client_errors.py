"""客户端错误上报（M7-6）。

总方案的要求写得很具体：「只报错误码、版本和系统信息，不带邮箱、文件路径、
视频内容；用户可以在设置里关掉。」

这里的实现有两个刻意的选择：

1. **用白名单而不是黑名单**：只接受 `code` / `app_version` / `platform` / `location`
   四个字段，多出来的一律丢弃。黑名单（「不含邮箱、不含路径」）漏一个字段就是泄露，
   白名单漏了只是少收集一点信息。
2. **计数而不是逐条存**：同一个错误一天可能上报成百上千次，逐条存会把表撑爆、
   也让真正要看的东西沉在噪音里。所以同一组 (code, version, platform, location)
   只保留一行 + `occurrences` 计数。

**默认关闭**：桌面端不上报，除非用户在设置里主动打开。
"""

from __future__ import annotations

import logging
from typing import Optional

from fastapi import APIRouter, Depends, Request
from pydantic import Field

from ..base import StrictModel
from . import ratelimit
from .admins import AdminContext
from .admin_routes import require_admin
from .deps import client_key, get_conn
from .ratelimit import Rule

log = logging.getLogger(__name__)

router = APIRouter(prefix="/api/v1")

#: 上报接口的限流。桌面端只应在出错时上报，正常不会多。
ERRORS_PER_IP = Rule("client:errors", 300, 3600)

#: 允许上报的字段。多一个都不收——白名单是这里唯一的安全边界。
ALLOWED_FIELDS = ("code", "app_version", "platform", "location")
ALLOWED_PLATFORMS = ("windows", "macos", "linux", "unknown")


class ClientErrorIn(StrictModel):
    """一条错误上报。

    故意**不含**任何自由文本字段：错误消息本身可能带文件路径或剧名，
    收它就等于把「不带路径」这条要求作废。要看细节请让用户截图。
    """

    code: str = Field(min_length=1, max_length=64)
    app_version: str = Field(min_length=1, max_length=32)
    platform: str = Field(default="unknown", max_length=16)
    location: str = Field(default="", max_length=64)


@router.post("/client/errors", status_code=202)
def report_client_error(
    payload: ClientErrorIn,
    request: Request,
    conn=Depends(get_conn),
):
    """收一条错误上报。同一个错误只累加计数，不逐条存。"""
    ratelimit.hit(conn, ERRORS_PER_IP, client_key(request))

    platform = payload.platform if payload.platform in ALLOWED_PLATFORMS else "unknown"
    # `location` 是「出错的位置」（模块名，例如 upload / credits），
    # 不是路径。这里再挡一次：带斜杠或反斜杠的一律按空处理——
    # 万一客户端把路径塞进来了，也不会进库。
    location = payload.location.strip()
    if "/" in location or "\\" in location:
        location = ""

    conn.execute(
        """
        INSERT INTO client_errors (code, app_version, platform, location)
        VALUES (%s, %s, %s, %s)
        ON CONFLICT (code, app_version, platform, location)
        DO UPDATE SET occurrences = client_errors.occurrences + 1, last_seen = now()
        """,
        (payload.code.strip(), payload.app_version.strip(), platform, location),
    )
    # 日志里也不记 payload：错误码与版本够定位，多记无益。
    log.info("客户端错误上报：code=%s platform=%s", payload.code, platform)
    return {"accepted": True}


# ---------- 后台查看（挂在 /api/v1/admin 下，与其它视图一致） ----------

admin_router = APIRouter(prefix="/api/v1/admin")


@admin_router.get("/client-errors")
def list_client_errors(
    limit: int = 100,
    ctx: AdminContext = Depends(require_admin),
    conn=Depends(get_conn),
):
    """按「最近出现」倒序列出客户端错误。次数多的排前面更有用，
    但这里先按时间：最近冒出来的新问题通常更紧急。"""
    rows = conn.execute(
        "SELECT code, app_version, platform, location, occurrences, first_seen, last_seen"
        " FROM client_errors ORDER BY last_seen DESC LIMIT %s",
        (min(max(limit, 1), 200),),
    ).fetchall()
    return {
        "items": [
            {
                "code": row["code"],
                "appVersion": row["app_version"],
                "platform": row["platform"],
                "location": row["location"],
                "occurrences": int(row["occurrences"]),
                "firstSeen": row["first_seen"].isoformat(),
                "lastSeen": row["last_seen"].isoformat(),
            }
            for row in rows
        ]
    }
