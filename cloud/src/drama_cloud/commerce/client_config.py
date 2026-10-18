"""桌面端的公开配置接口（M7-3）。

`GET /api/v1/client/config` 返回公告、功能开关、版本策略。

**为什么公开（不需要登录）**：桌面端启动时就要知道「要不要强制更新」，
那时可能还没登录。所以这个接口里**绝不能有用户数据**——只有公告正文、
开关布尔值、版本号与下载地址。

**拉不到怎么办**：桌面端有安全默认值（见 `src/logic.js` 的 `defaultClientConfig`）：
空公告、开关全开、不强制更新。理由是三条都朝「不把人挡在门外」的方向降级——
服务端挂了不该让所有客户端变砖。
"""

from __future__ import annotations

from fastapi import APIRouter, Depends, Request

from . import ratelimit
from .deps import client_key, get_conn
from .ratelimit import Rule

router = APIRouter(prefix="/api/v1")

#: 公开接口的限流。桌面端每次启动拉一次，正常不会多；给得宽些但仍要防刷。
CONFIG_PER_IP = Rule("client:config", 600, 3600)


@router.get("/client/config")
def client_config(request: Request, conn=Depends(get_conn)):
    """公告 + 功能开关 + 版本策略。公开，不含任何用户数据。"""
    ratelimit.hit(conn, CONFIG_PER_IP, client_key(request))

    announcements = conn.execute(
        """
        SELECT id, title, body, level
        FROM announcements
        WHERE active
          AND (starts_at IS NULL OR starts_at <= now())
          AND (ends_at IS NULL OR ends_at > now())
        ORDER BY sort_order, created_at DESC
        LIMIT 20
        """
    ).fetchall()

    flags = conn.execute("SELECT key, enabled FROM feature_flags ORDER BY key").fetchall()

    policy = conn.execute(
        "SELECT min_supported, latest, download_url, download_sha256, download_sig, notes"
        " FROM version_policy WHERE id = 1"
    ).fetchone()

    return {
        "announcements": [
            {"id": str(row["id"]), "title": row["title"], "body": row["body"], "level": row["level"]}
            for row in announcements
        ],
        "flags": {row["key"]: bool(row["enabled"]) for row in flags},
        "version": {
            "minSupported": policy["min_supported"] if policy else "0.0.0",
            "latest": policy["latest"] if policy else "0.0.0",
            "downloadUrl": policy["download_url"] if policy else "",
            "downloadSha256": policy["download_sha256"] if policy else "",
            "downloadSig": policy["download_sig"] if policy else "",
            "notes": policy["notes"] if policy else "",
        },
    }
