"""管理后台的业务接口（M7-2）。

七个视图：用户、卡密批次、订单、积分流水、设备、公告、功能开关（含版本策略）。

**只读为主**：用户/卡密/订单/流水/设备都是查询；只有公告、开关、版本策略可写。
理由：前面几样是「已经发生的事实」，管理员需要看，但不该在后台随手改——
改用户余额要走 `credits` CLI（有审计与幂等键），改订单状态更不该允许。

**所有查询都分页**：用户数增长起来以后，一次全量查询会把服务打垮。
"""

from __future__ import annotations

import logging
from typing import Optional

from fastapi import APIRouter, Depends, Query
from pydantic import Field

from ..base import StrictModel
from . import errors
from .admins import AdminContext
from .admin_routes import require_admin
from .db import audit
from .deps import get_conn

log = logging.getLogger(__name__)

router = APIRouter(prefix="/api/v1/admin")

#: 一页最多多少行。给上限是为了防「limit=1000000」这种把库拖死的请求。
#: FastAPI 的 `Query(le=MAX_PAGE)` 会在入口就拒掉，不需要在这里再夹一次。
MAX_PAGE = 200


# ---------- 1. 用户 ----------

@router.get("/users")
def list_users(
    keyword: Optional[str] = Query(None, max_length=120),
    limit: int = Query(50, ge=1, le=MAX_PAGE),
    offset: int = Query(0, ge=0),
    ctx: AdminContext = Depends(require_admin),
    conn=Depends(get_conn),
):
    """用户列表：邮箱、注册时间、积分余额、使用权到期、设备数。

    邮箱是个人信息，但管理员本来就要靠它找人，所以这里返回；
    审计日志里仍然只记域名（见 admins.py）。
    """
    params: list = []
    where = "WHERE u.deleted_at IS NULL"
    if keyword:
        where += " AND u.email ILIKE %s"
        params.append(f"%{keyword.strip()}%")
    rows = conn.execute(
        f"""
        SELECT u.id, u.email, u.created_at,
               COALESCE((SELECT SUM(delta) FROM credit_ledger c WHERE c.user_id = u.id), 0) AS balance,
               e.kind AS entitlement_kind, e.expires_at,
               (SELECT count(*) FROM devices d
                 WHERE d.user_id = u.id AND d.unbound_at IS NULL) AS device_count
        FROM users u LEFT JOIN entitlements e ON e.user_id = u.id
        {where}
        ORDER BY u.created_at DESC LIMIT %s OFFSET %s
        """,  # noqa: S608 - where 只有固定片段与一个占位符
        (*params, limit, offset),
    ).fetchall()
    total = conn.execute(
        f"SELECT count(*) AS n FROM users u {where}", tuple(params)
    ).fetchone()["n"]
    return {
        "total": int(total),
        "items": [
            {
                "id": str(row["id"]),
                "email": row["email"],
                "createdAt": row["created_at"].isoformat(),
                "balance": int(row["balance"]),
                "entitlement": row["entitlement_kind"],
                "expiresAt": row["expires_at"].isoformat() if row["expires_at"] else None,
                "deviceCount": int(row["device_count"]),
            }
            for row in rows
        ],
    }


# ---------- 2. 卡密批次 ----------

@router.get("/card-batches")
def list_card_batches(
    limit: int = Query(50, ge=1, le=MAX_PAGE),
    offset: int = Query(0, ge=0),
    ctx: AdminContext = Depends(require_admin),
    conn=Depends(get_conn),
):
    """卡密批次：总量、已兑换、未兑换。

    **不返回卡密明文**：数据库里只有摘要，明文只在导出 CSV 那一刻存在。
    """
    rows = conn.execute(
        """
        SELECT b.id, b.name, b.kind, b.days, b.created_by, b.created_at, b.revoked_at,
               count(c.code_digest) AS total,
               count(c.redeemed_at) AS redeemed
        FROM card_batches b LEFT JOIN cards c ON c.batch_id = b.id
        GROUP BY b.id ORDER BY b.created_at DESC LIMIT %s OFFSET %s
        """,
        (limit, offset),
    ).fetchall()
    return {
        "items": [
            {
                "id": str(row["id"]),
                "name": row["name"],
                "kind": row["kind"],
                "days": row["days"],
                "createdBy": row["created_by"],
                "createdAt": row["created_at"].isoformat(),
                "revoked": row["revoked_at"] is not None,
                "total": int(row["total"]),
                "redeemed": int(row["redeemed"]),
                "remaining": int(row["total"]) - int(row["redeemed"]),
            }
            for row in rows
        ]
    }


# ---------- 3. 订单 ----------

@router.get("/orders")
def list_orders(
    status_filter: Optional[str] = Query(None, alias="status", max_length=16),
    limit: int = Query(50, ge=1, le=MAX_PAGE),
    offset: int = Query(0, ge=0),
    ctx: AdminContext = Depends(require_admin),
    conn=Depends(get_conn),
):
    """订单列表。`status` 用来筛「为什么没到账」——最常见的就是 `created`（没付款）。"""
    where = ""
    params: list = []
    if status_filter:
        if status_filter not in ("created", "paid", "closed", "refunded"):
            raise errors.CommerceError(422, "invalid_request", f"不认识的订单状态：{status_filter}")
        where = "WHERE o.status = %s"
        params.append(status_filter)
    rows = conn.execute(
        f"""
        SELECT o.out_trade_no, o.channel, o.amount_fen, o.credits, o.status,
               o.tier_id, o.created_at, o.paid_at, o.expires_at, u.email
        FROM payment_orders o JOIN users u ON u.id = o.user_id
        {where}
        ORDER BY o.created_at DESC LIMIT %s OFFSET %s
        """,  # noqa: S608
        (*params, limit, offset),
    ).fetchall()
    return {
        "items": [
            {
                "outTradeNo": row["out_trade_no"],
                "channel": row["channel"],
                "amountFen": int(row["amount_fen"]),
                "credits": int(row["credits"]),
                "status": row["status"],
                "tierId": row["tier_id"],
                "email": row["email"],
                "createdAt": row["created_at"].isoformat(),
                "paidAt": row["paid_at"].isoformat() if row["paid_at"] else None,
                "expiresAt": row["expires_at"].isoformat(),
            }
            for row in rows
        ]
    }


# ---------- 4. 积分流水 ----------

@router.get("/ledger")
def list_ledger(
    email: Optional[str] = Query(None, max_length=254),
    limit: int = Query(50, ge=1, le=MAX_PAGE),
    offset: int = Query(0, ge=0),
    ctx: AdminContext = Depends(require_admin),
    conn=Depends(get_conn),
):
    """积分流水（`credit_ledger`）。用于核对「这笔钱为什么是这么算的」。"""
    where = ""
    params: list = []
    if email:
        where = "WHERE u.email = %s"
        params.append(email.strip().lower())
    rows = conn.execute(
        f"""
        SELECT c.idempotency_key, c.delta, c.kind, c.ref, c.settled_state, c.memo,
               c.created_at, u.email
        FROM credit_ledger c JOIN users u ON u.id = c.user_id
        {where}
        ORDER BY c.created_at DESC LIMIT %s OFFSET %s
        """,  # noqa: S608
        (*params, limit, offset),
    ).fetchall()
    return {
        "items": [
            {
                "key": row["idempotency_key"],
                "delta": int(row["delta"]),
                "kind": row["kind"],
                "ref": row["ref"],
                "settledState": row["settled_state"],
                "memo": row["memo"],
                "email": row["email"],
                "createdAt": row["created_at"].isoformat(),
            }
            for row in rows
        ]
    }


# ---------- 5. 设备 ----------

@router.get("/devices")
def list_devices(
    email: Optional[str] = Query(None, max_length=254),
    limit: int = Query(50, ge=1, le=MAX_PAGE),
    offset: int = Query(0, ge=0),
    ctx: AdminContext = Depends(require_admin),
    conn=Depends(get_conn),
):
    """设备绑定情况。**不返回完整机器码**：它是设备标识，只给前 12 位便于辨认。"""
    where = ""
    params: list = []
    if email:
        where = "WHERE u.email = %s"
        params.append(email.strip().lower())
    rows = conn.execute(
        f"""
        SELECT d.device_id, d.name, d.platform, d.app_version, d.bound_at,
               d.last_seen_at, d.unbound_at, d.unbind_reason, u.email
        FROM devices d JOIN users u ON u.id = d.user_id
        {where}
        ORDER BY d.bound_at DESC LIMIT %s OFFSET %s
        """,  # noqa: S608
        (*params, limit, offset),
    ).fetchall()
    return {
        "items": [
            {
                "deviceIdPrefix": row["device_id"][:12],
                "name": row["name"],
                "platform": row["platform"],
                "appVersion": row["app_version"],
                "email": row["email"],
                "boundAt": row["bound_at"].isoformat(),
                "lastSeenAt": row["last_seen_at"].isoformat(),
                "unboundAt": row["unbound_at"].isoformat() if row["unbound_at"] else None,
                "unbindReason": row["unbind_reason"],
            }
            for row in rows
        ]
    }


# ---------- 6. 公告（可写） ----------

class AnnouncementIn(StrictModel):
    title: str = Field(min_length=1, max_length=120)
    body: str = Field(min_length=1, max_length=2000)
    level: str = Field(default="info", pattern="^(info|warn)$")
    sort_order: int = Field(default=100, ge=0, le=10000)
    active: bool = True
    #: 生效窗口（可选）。留空表示不限制；都填时要求起点早于终点。
    #: 表里本来就有这两列、客户端配置也按窗口过滤，接口不收等于那条能力用不上。
    starts_at: Optional[str] = Field(default=None, max_length=40)
    ends_at: Optional[str] = Field(default=None, max_length=40)


def _parse_window(payload: AnnouncementIn) -> tuple:
    """把可选的起止时间解析成时间戳；格式不对就 422。

    不做时区猜测：要求带时区（例如 `2026-10-18T00:00:00+08:00`），
    不带时区的时间在部署跨时区时会变成「公告提前或延后几小时生效」。
    """
    from datetime import datetime

    parsed = []
    for label, value in (("starts_at", payload.starts_at), ("ends_at", payload.ends_at)):
        if not value:
            parsed.append(None)
            continue
        try:
            moment = datetime.fromisoformat(value.replace("Z", "+00:00"))
        except ValueError as error:
            raise errors.CommerceError(
                422, "invalid_request", f"{label} 不是合法时间：{value}"
            ) from error
        if moment.tzinfo is None:
            raise errors.CommerceError(
                422, "invalid_request", f"{label} 必须带时区（例如 +08:00 或 Z）"
            )
        parsed.append(moment)
    start, end = parsed
    if start and end and start >= end:
        raise errors.CommerceError(422, "invalid_request", "公告的起始时间要早于结束时间")
    return start, end


@router.get("/announcements")
def list_announcements(
    ctx: AdminContext = Depends(require_admin),
    conn=Depends(get_conn),
):
    rows = conn.execute(
        "SELECT id, title, body, level, sort_order, active, starts_at, ends_at, updated_at"
        " FROM announcements ORDER BY sort_order, created_at DESC"
    ).fetchall()
    return {
        "items": [
            {
                "id": str(row["id"]),
                "title": row["title"],
                "body": row["body"],
                "level": row["level"],
                "sortOrder": int(row["sort_order"]),
                "active": bool(row["active"]),
                "startsAt": row["starts_at"].isoformat() if row["starts_at"] else None,
                "endsAt": row["ends_at"].isoformat() if row["ends_at"] else None,
                "updatedAt": row["updated_at"].isoformat(),
            }
            for row in rows
        ]
    }


@router.post("/announcements", status_code=201)
def create_announcement(
    payload: AnnouncementIn,
    ctx: AdminContext = Depends(require_admin),
    conn=Depends(get_conn),
):
    starts_at, ends_at = _parse_window(payload)
    with conn.transaction():
        row = conn.execute(
            "INSERT INTO announcements (title, body, level, sort_order, active, starts_at, ends_at)"
            " VALUES (%s,%s,%s,%s,%s,%s,%s) RETURNING id",
            (payload.title.strip(), payload.body.strip(), payload.level,
             int(payload.sort_order), payload.active, starts_at, ends_at),
        ).fetchone()
        audit(conn, ctx.admin_id, "admin.announcement.create", str(row["id"]))
    return {"id": str(row["id"])}


@router.patch("/announcements/{announcement_id}")
def update_announcement(
    announcement_id: str,
    payload: AnnouncementIn,
    ctx: AdminContext = Depends(require_admin),
    conn=Depends(get_conn),
):
    starts_at, ends_at = _parse_window(payload)
    with conn.transaction():
        row = conn.execute(
            "UPDATE announcements SET title = %s, body = %s, level = %s,"
            " sort_order = %s, active = %s, starts_at = %s, ends_at = %s, updated_at = now()"
            " WHERE id = %s::uuid RETURNING id",
            (payload.title.strip(), payload.body.strip(), payload.level,
             int(payload.sort_order), payload.active, starts_at, ends_at, announcement_id),
        ).fetchone()
        if row is None:
            raise errors.CommerceError(404, "announcement_not_found", "找不到这条公告")
        audit(conn, ctx.admin_id, "admin.announcement.update", announcement_id)
    return {"id": announcement_id}


@router.delete("/announcements/{announcement_id}", status_code=204)
def delete_announcement(
    announcement_id: str,
    ctx: AdminContext = Depends(require_admin),
    conn=Depends(get_conn),
):
    with conn.transaction():
        row = conn.execute(
            "DELETE FROM announcements WHERE id = %s::uuid RETURNING id", (announcement_id,)
        ).fetchone()
        if row is None:
            raise errors.CommerceError(404, "announcement_not_found", "找不到这条公告")
        audit(conn, ctx.admin_id, "admin.announcement.delete", announcement_id)
    return None


# ---------- 7. 功能开关与版本策略（可写） ----------

class FlagIn(StrictModel):
    enabled: bool
    note: str = Field(default="", max_length=200)


@router.get("/flags")
def list_flags(
    ctx: AdminContext = Depends(require_admin),
    conn=Depends(get_conn),
):
    rows = conn.execute(
        "SELECT key, enabled, note, updated_at FROM feature_flags ORDER BY key"
    ).fetchall()
    return {
        "items": [
            {
                "key": row["key"],
                "enabled": bool(row["enabled"]),
                "note": row["note"],
                "updatedAt": row["updated_at"].isoformat(),
            }
            for row in rows
        ]
    }


@router.put("/flags/{key}")
def set_flag(
    key: str,
    payload: FlagIn,
    ctx: AdminContext = Depends(require_admin),
    conn=Depends(get_conn),
):
    with conn.transaction():
        row = conn.execute(
            "UPDATE feature_flags SET enabled = %s, note = %s, updated_at = now()"
            " WHERE key = %s RETURNING key",
            (payload.enabled, payload.note.strip(), key),
        ).fetchone()
        if row is None:
            # 不认识的开关直接拒绝：客户端只认它认识的那几个，
            # 后台造一个新的只会让两边不一致。
            raise errors.CommerceError(404, "flag_not_found", f"不认识的开关：{key}")
        audit(conn, ctx.admin_id, "admin.flag.set", key, {"enabled": payload.enabled})
    return {"key": key, "enabled": payload.enabled}


class VersionPolicyIn(StrictModel):
    min_supported: str = Field(min_length=1, max_length=32)
    latest: str = Field(min_length=1, max_length=32)
    download_url: str = Field(default="", max_length=500)
    download_sha256: str = Field(default="", pattern="^([0-9a-f]{64})?$")
    download_sig: str = Field(default="", max_length=200)
    notes: str = Field(default="", max_length=2000)


@router.get("/version-policy")
def get_version_policy(
    ctx: AdminContext = Depends(require_admin),
    conn=Depends(get_conn),
):
    row = conn.execute("SELECT * FROM version_policy WHERE id = 1").fetchone()
    return {
        "minSupported": row["min_supported"],
        "latest": row["latest"],
        "downloadUrl": row["download_url"],
        "downloadSha256": row["download_sha256"],
        "downloadSig": row["download_sig"],
        "notes": row["notes"],
        "updatedAt": row["updated_at"].isoformat(),
    }


@router.put("/version-policy")
def set_version_policy(
    payload: VersionPolicyIn,
    ctx: AdminContext = Depends(require_admin),
    conn=Depends(get_conn),
):
    # 版本号只做形状校验（形如 1.2.3），不做语义比较：比较在前端与 Rust 侧各有一份，
    # 这里多写一份只会出现两处规则不一致。
    _check_version(payload.min_supported)
    _check_version(payload.latest)
    with conn.transaction():
        conn.execute(
            "UPDATE version_policy SET min_supported = %s, latest = %s, download_url = %s,"
            " download_sha256 = %s, download_sig = %s, notes = %s, updated_at = now()"
            " WHERE id = 1",
            (payload.min_supported.strip(), payload.latest.strip(), payload.download_url.strip(),
             payload.download_sha256.strip(), payload.download_sig.strip(), payload.notes.strip()),
        )
        audit(conn, ctx.admin_id, "admin.version_policy.set", "1", {
            "min_supported": payload.min_supported, "latest": payload.latest,
        })
    return {"ok": True}


def _check_version(value: str) -> None:
    parts = value.strip().split(".")
    if len(parts) < 2 or not all(part.isdigit() for part in parts):
        raise errors.CommerceError(
            422, "invalid_request", f"版本号要形如 1.2.3：{value!r}"
        )
