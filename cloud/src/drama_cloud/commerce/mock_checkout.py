"""本地模拟收银台（M5a）。

**为什么需要它**：Mock 渠道下单后返回的跳转地址指向 `/pay/mock/checkout`，
没有这个页面，本地就没法把「下单 → 支付 → 回调 → 入账」走完。

**它不是一条「直接入账」的捷径**：页面上的每个按钮都只是把一份回调报文连同
`mock_pay_secret` 算出的签名 POST 到真实回调地址 `/api/v1/payments/mock/notify`。
入账走的仍是 `payments.service.handle_event()` 那条唯一的路。所以在这里能真正
测到验签、金额校验、重复回调去重。

页面不存任何会话状态：订单号在网址里，报文与签名由服务端在渲染时算好放在
`data-` 属性上，点按钮发出去的就是页面上展示的那一份，不含任何客户端拼装。
"""

from __future__ import annotations

import base64
import html
import logging
from datetime import datetime, timedelta, timezone

from fastapi import APIRouter, Depends, Request
from fastapi.responses import HTMLResponse

from .deps import get_conn
from .payments import SIGNATURE_HEADER
from .payments.mock import MockProvider

log = logging.getLogger(__name__)

router = APIRouter()

#: 收银台上的四个按钮：(动作标识, 标题, 一句说明)。
ACTIONS = (
    ("paid", "支付成功", "发出一次「已支付」回调，积分入账"),
    ("paid_again", "重复回调", "发一份与上面**逐字节相同**的报文，验证不会重复入账"),
    ("closed", "支付超时", "发出「已关闭」回调，订单作废、不入账"),
    ("tampered", "伪造金额", "把金额改大一分再签名，验证金额不符时拒绝入账"),
)

#: 页面自身的回调地址。同源，所以只写路径，不依赖 `public_base_url`。
_NOTIFY_PATH = "/api/v1/payments/mock/notify"


def _page(title: str, body: str) -> HTMLResponse:
    """极简页面。动态内容一律先过 `html.escape`。"""
    return HTMLResponse(
        '<!doctype html><html lang="zh-CN"><head><meta charset="utf-8">'
        f"<title>{html.escape(title)}</title>"
        "<style>body{font-family:system-ui,sans-serif;max-width:44rem;margin:3rem auto;"
        "padding:0 1rem;line-height:1.7;color:#222}"
        "code{background:#f4f4f4;padding:.1rem .3rem;border-radius:3px}"
        "button{display:inline-block;margin:.3rem .4rem .3rem 0;padding:.5rem 1rem;"
        "border:1px solid #888;border-radius:6px;background:#fff;font-size:1rem;cursor:pointer;"
        "color:#222}"
        "button.warn{border-color:#b33;color:#b33}"
        ".hint{color:#666;font-size:.9rem}"
        "pre{background:#f7f7f7;padding:.8rem;border-radius:6px;overflow:auto;white-space:pre-wrap}"
        "table{border-collapse:collapse}td{padding:.15rem 1rem .15rem 0;vertical-align:top}"
        "</style></head>"
        f"<body>{body}</body></html>"
    )


def _order(conn, out_trade_no: str):
    return conn.execute(
        "SELECT out_trade_no, amount_fen, credits, status, expires_at"
        " FROM payment_orders WHERE out_trade_no = %s",
        (out_trade_no,),
    ).fetchone()


def _status_text(status: str) -> str:
    return {
        "created": "待支付",
        "paid": "已支付（积分已入账）",
        "closed": "已关闭 / 已超时",
        "refunded": "已退款",
    }.get(status, status)


def _payload(row, action: str) -> dict:
    """按动作生成回调报文。

    `paid_at` 由订单的 `expires_at` 倒推（= 下单时间），是确定值。
    这样「支付成功」和「重复回调」两个按钮发出的是**逐字节相同**的报文，
    才真正测得到幂等；如果这里用 `now()`，每次报文都不一样，就测不到了。
    """
    paid_at = row["expires_at"] - timedelta(hours=2)
    if paid_at.tzinfo is None:
        paid_at = paid_at.replace(tzinfo=timezone.utc)
    amount = int(row["amount_fen"])
    status = "paid"
    if action == "closed":
        status = "closed"
    elif action == "tampered":
        amount += 1  # 差一分就够触发金额校验
    payload = {
        "out_trade_no": row["out_trade_no"],
        "channel_trade_no": f"MOCK-{row['out_trade_no']}",
        "amount_fen": amount,
        "status": status,
    }
    if status == "paid":
        payload["paid_at"] = paid_at.isoformat()
    return payload


@router.get("/pay/mock/checkout", response_class=HTMLResponse, include_in_schema=False)
def checkout(request: Request, out_trade_no: str = "", conn=Depends(get_conn)):
    """模拟收银台页面。"""
    settings = request.app.state.settings
    # 没配签名密钥时 MockProvider 构造就抛 NotConfigured，由 API 层变成 503：
    # 宁可不显示页面，也不显示一个「点了没用」的假收银台。
    # 页面上只需要签名，用不到 base_url，所以不传来源地址。
    provider = MockProvider(settings.mock_pay_secret, settings.public_base_url)

    if not out_trade_no:
        return _page(
            "模拟收银台",
            "<h1>模拟收银台</h1>"
            "<p>网址里缺少订单号（<code>out_trade_no</code>）。请从客户端的充值入口重新下单。</p>",
        )

    row = _order(conn, out_trade_no)
    if row is None:
        return _page("模拟收银台", "<h1>模拟收银台</h1><p>找不到这笔订单。</p>")

    summary = (
        "<table>"
        f"<tr><td>订单号</td><td><code>{html.escape(row['out_trade_no'])}</code></td></tr>"
        f"<tr><td>金额</td><td>{int(row['amount_fen'])} 分</td></tr>"
        f"<tr><td>本单积分</td><td>{int(row['credits'])}</td></tr>"
        f"<tr><td>订单状态</td><td>{html.escape(_status_text(row['status']))}</td></tr>"
        "</table>"
    )

    blocks = []
    for action, label, hint in ACTIONS:
        body, signature = provider.sign(_payload(row, action))
        # 报文与签名都放进 data- 属性：点按钮发出去的就是页面上这一份。
        # 报文用 base64 存放，免得 HTML 转义（引号变 &quot;）改变字节；
        # 页面上展示的仍是原文，方便肉眼核对。
        encoded = base64.b64encode(body).decode("ascii")
        blocks.append(
            f"<h3>{html.escape(label)}</h3>"
            f"<p class='hint'>{html.escape(hint)}</p>"
            f"<pre>{html.escape(body.decode('utf-8'))}</pre>"
            f"<p class='hint'>签名 <code>{SIGNATURE_HEADER}: {html.escape(signature)}</code></p>"
            f"<button type='button' data-action='{html.escape(action)}'"
            f" data-body='{encoded}'"
            f" data-signature='{html.escape(signature)}'"
            f"{' class=\"warn\"' if action == 'tampered' else ''}>发送这个回调</button>"
        )

    body = (
        "<h1>模拟收银台</h1>"
        "<p class='hint'>这是本地开发用的假收银台，不涉及任何真实资金。"
        "按钮会把上面展示的报文签名后 POST 到真实回调地址，"
        "入账走的是和真实渠道完全相同的代码。</p>"
        f"{summary}"
        "<div id='log' class='hint'>点上面的按钮看结果，然后刷新页面看订单状态。</div>"
        + "".join(blocks)
        + f"""
<script>
const log = document.getElementById("log");
document.querySelectorAll("button[data-action]").forEach((button) => {{
  button.addEventListener("click", async () => {{
    // data-body 是 base64：先还原成字节，再按 UTF-8 解码。直接 atob 会把中文弄乱。
    const bytes = Uint8Array.from(atob(button.dataset.body), (c) => c.charCodeAt(0));
    const body = new TextDecoder("utf-8").decode(bytes);
    const signature = button.dataset.signature;
    log.textContent = "发送中…";
    try {{
      const response = await fetch({_NOTIFY_PATH!r}, {{
        method: "POST",
        headers: {{ "Content-Type": "application/json", "X-Mock-Signature": signature }},
        body,
      }});
      const text = await response.text();
      const credited = response.headers.get("X-Credited");
      log.innerHTML =
        "<strong>" + (response.ok ? "回调被接受" : "回调被拒绝") + "</strong>（HTTP "
        + response.status + "）"
        + (credited === null ? "" : "，本次入账 " + credited + " 积分");
      const pre = document.createElement("pre");
      pre.textContent = text;
      log.appendChild(pre);
    }} catch (error) {{
      log.textContent = "请求失败：" + error;
    }}
  }});
}});
</script>"""
    )
    return _page("模拟收银台", body)
