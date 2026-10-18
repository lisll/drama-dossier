"""管理后台的页面（M7）。

页面是静态 HTML，挂在 `/admin`；接口是 JSON，挂在 `/api/v1/admin/*`。
两者分开，路由表才读得懂。

发页面的方式与媒体控制面 `api.py` 一致：读文件返回 `HTMLResponse`，
文件缺失时给一句明确提示而不是 500——「装完包没带页面」是个容易排查的部署问题，
不该表现成一个堆栈。
"""

from __future__ import annotations

import logging
from pathlib import Path

from fastapi import APIRouter
from fastapi.responses import HTMLResponse

log = logging.getLogger(__name__)

router = APIRouter()

#: 页面文件位置。按包内路径解析，不依赖启动目录。
PAGE_PATH = Path(__file__).resolve().parent / "web" / "admin.html"


@router.get("/admin", response_class=HTMLResponse, include_in_schema=False)
def admin_page():
    try:
        return HTMLResponse(PAGE_PATH.read_text(encoding="utf-8"))
    except FileNotFoundError:
        log.error("管理后台页面缺失：%s", PAGE_PATH)
        return HTMLResponse(
            "<h1>管理后台页面缺失</h1>"
            "<p>安装包不完整：没有找到 <code>commerce/web/admin.html</code>。"
            "请重新安装，或确认 <code>pyproject.toml</code> 的 package-data 含这一项。</p>",
            status_code=500,
        )
