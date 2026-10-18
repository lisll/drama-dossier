"""确认流的人工复核队列（分支B）。

对应处理管线的最后一段：

    自动校验失败 → 分支A：自动重试（调 mask / 换后端）→ 仍不通过 → 分支B：人工复核队列
    自动校验仅 warn → 产物照常产出，但同样登记一条待抽查记录

复核者能做两件事：

1. **标记通过 / 标记失败**（`decide`）：只改这条复核记录的状态与审计，不改任务状态。
   自动校验失败的任务没有可交付产物，所以不允许「标记通过」——那种情况应当修正 mask 后重跑；
2. **修正 mask 后重跑**（`rerun`）：把窗口/区域/膨胀等参数覆盖套到原配方上，创建一个**新任务**，
   原复核记录转为 `superseded`。保留原始任务与原始参数，重跑不会污染「这一版到底怎么跑的」证据。
"""

from __future__ import annotations

from typing import List, Optional

from ..config import Settings
from ..db import audit, json_dump, json_load, now_iso
from ..errors import ErrorClass, WorkError
from . import service

#: 复核记录状态。
REVIEW_STATUSES = ("pending", "approved", "rejected", "superseded")
SEVERITIES = ("info", "warn", "high")
#: 自动复核结论对应的复核原因。
REASON_AUTO_FAILED = "auto_verify_failed"
REASON_WARN = "auto_verify_warn"


def new_id() -> str:
    return service.new_id("rev")


def create_review(
    conn,
    task_id: str,
    reason: str,
    severity: str = "warn",
    detail: Optional[dict] = None,
    artifact_id: Optional[str] = None,
    actor: str = "system",
) -> dict:
    """登记一条待复核记录。

    同一个任务 + 同一个原因**只保留一条待处理记录**：重试期间可能反复触发同一个原因，
    重复入队只会让复核者看到一堆一样的条目。
    """
    if severity not in SEVERITIES:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"未知严重度：{severity}")
    existing = conn.execute(
        "SELECT * FROM review_queue WHERE task_id = %s AND reason = %s AND status = 'pending'",
        (task_id, reason),
    ).fetchone()
    if existing is not None:
        return _row_to_dict(existing)

    review_id = new_id()
    conn.execute(
        """
        INSERT INTO review_queue (id, task_id, artifact_id, reason, severity, status,
                                  detail_json, claimed_by, created_at)
        VALUES (%s,%s,%s,%s,%s, 'pending', %s, NULL, %s)
        """,
        (review_id, task_id, artifact_id, reason, severity,
         json_dump(detail or {}), now_iso()),
    )
    audit(conn, actor, "review.enqueue", review_id,
          {"task_id": task_id, "reason": reason, "severity": severity})
    return get_review(conn, review_id)


def _row_to_dict(row) -> dict:
    return {
        "id": row["id"],
        "task_id": row["task_id"],
        "artifact_id": row["artifact_id"],
        "reason": row["reason"],
        "severity": row["severity"],
        "status": row["status"],
        "detail": json_load(row["detail_json"], {}) or {},
        "claimed_by": row["claimed_by"],
        "created_at": row["created_at"],
        "decided_by": row["decided_by"],
        "decided_at": row["decided_at"],
        "decision_note": row["decision_note"],
    }


def get_review(conn, review_id: str) -> dict:
    row = conn.execute("SELECT * FROM review_queue WHERE id = %s", (review_id,)).fetchone()
    if row is None:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"复核记录不存在：{review_id}")
    return _row_to_dict(row)


def list_reviews(
    conn,
    status: Optional[str] = None,
    task_id: Optional[str] = None,
    limit: int = 100,
    owner_id: Optional[str] = None,
) -> List[dict]:
    """列复核记录。

    `owner_id` 传了就只返回这个用户项目下的记录；不传表示不过滤（内部调用）。
    复核记录里带着任务 id 与失败细节，跨用户看到就等于泄露别人的处理内容。
    """
    clauses, values = [], []
    if status:
        if status not in REVIEW_STATUSES:
            raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"未知复核状态：{status}")
        clauses.append("r.status = %s")
        values.append(status)
    if task_id:
        clauses.append("r.task_id = %s")
        values.append(task_id)
    if owner_id is not None:
        clauses.append("p.owner_id = %s")
        values.append(owner_id)
    where = (" WHERE " + " AND ".join(clauses)) if clauses else ""
    rows = conn.execute(
        "SELECT r.* FROM review_queue r"
        " JOIN tasks t ON t.id = r.task_id"
        " JOIN projects p ON p.id = t.project_id"
        f"{where} ORDER BY r.created_at DESC LIMIT %s",  # noqa: S608
        (*values, int(limit)),
    ).fetchall()
    return [_row_to_dict(row) for row in rows]


def ensure_review_access(conn, review_id: str, owner_id: str) -> None:
    """确认这条复核记录属于 `owner_id`，否则按「不存在」处理。"""
    row = conn.execute(
        "SELECT p.owner_id FROM review_queue r"
        " JOIN tasks t ON t.id = r.task_id"
        " JOIN projects p ON p.id = t.project_id"
        " WHERE r.id = %s",
        (review_id,),
    ).fetchone()
    if row is None or str(row["owner_id"]) != owner_id:
        raise WorkError(ErrorClass.NOT_FOUND, f"复核记录不存在：{review_id}")


def pending_for_task(conn, task_id: str) -> List[dict]:
    return list_reviews(conn, status="pending", task_id=task_id)


def review_summary(conn, task_id: str) -> Optional[dict]:
    """任务上的复核投影（`get_task` 用）；没有记录时为 None。"""
    rows = list_reviews(conn, task_id=task_id, limit=50)
    if not rows:
        return None
    pending = [row for row in rows if row["status"] == "pending"]
    return {
        "pending": len(pending),
        "total": len(rows),
        "status": "pending" if pending else rows[0]["status"],
        "items": [
            {
                "id": row["id"],
                "reason": row["reason"],
                "severity": row["severity"],
                "status": row["status"],
                "created_at": row["created_at"],
            }
            for row in rows
        ],
        "pending_ids": [row["id"] for row in pending],
    }


def claim(conn, review_id: str, actor: str) -> dict:
    """认领一条待复核记录（避免两个人同时处理同一条）。"""
    row = _fetch(conn, review_id)
    if row["status"] != "pending":
        raise WorkError(
            ErrorClass.PARAM_OUT_OF_RANGE, f"复核记录 {review_id} 已是 {row['status']}，无需认领"
        )
    if row["claimed_by"] and row["claimed_by"] != actor:
        raise WorkError(
            ErrorClass.RESOURCE_CONTENTION,
            f"复核记录 {review_id} 已被 {row['claimed_by']} 认领",
        )
    conn.execute(
        "UPDATE review_queue SET claimed_by = %s WHERE id = %s", (actor, review_id)
    )
    audit(conn, actor, "review.claim", review_id, {})
    return get_review(conn, review_id)


def _fetch(conn, review_id: str):
    row = conn.execute("SELECT * FROM review_queue WHERE id = %s", (review_id,)).fetchone()
    if row is None:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"复核记录不存在：{review_id}")
    return row


def decide(conn, review_id: str, decision: str, note: str = "", actor: str = "system") -> dict:
    """人工裁决：`approve`（放行）/ `reject`（判定不可用，通常随后重跑）。"""
    if decision not in ("approve", "reject"):
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"未知复核决定：{decision}")
    row = _fetch(conn, review_id)
    if row["status"] != "pending":
        raise WorkError(
            ErrorClass.PARAM_OUT_OF_RANGE,
            f"复核记录 {review_id} 已是 {row['status']}，不能重复裁决",
        )
    if decision == "approve" and not row["artifact_id"]:
        raise WorkError(
            ErrorClass.PARAM_OUT_OF_RANGE,
            "这条复核记录没有可交付产物（自动校验失败时不会产出成品）；"
            "请修正 mask 后用重跑接口重新出片",
        )
    status = "approved" if decision == "approve" else "rejected"
    conn.execute(
        "UPDATE review_queue SET status = %s, decided_by = %s, decided_at = %s, "
        "decision_note = %s, claimed_by = COALESCE(claimed_by, %s) WHERE id = %s",
        (status, actor, now_iso(), note, actor, review_id),
    )
    audit(conn, actor, f"review.{decision}", review_id,
          {"task_id": row["task_id"], "note": note})
    return get_review(conn, review_id)


def rerun(
    conn,
    settings: Settings,
    review_id: str,
    user_params: Optional[dict] = None,
    note: str = "",
    actor: str = "system",
    priority: int = 100,
) -> dict:
    """按修正后的参数重跑：创建一个新任务，原复核记录转 `superseded`。

    `user_params` 的形态与创建任务时一致（`{"<步骤 id>": {"参数": 值}}`），因此「修正 mask」
    就是覆盖 `video.inpaint` 步骤的 `regions` / `dilate` / `start` 等已有参数；
    覆盖规则、取值范围仍由 `service.apply_user_params` 与模块参数模型负责，这里不做第二份校验。
    """
    row = _fetch(conn, review_id)
    if row["status"] not in ("pending", "rejected"):
        raise WorkError(
            ErrorClass.PARAM_OUT_OF_RANGE,
            f"复核记录 {review_id} 已是 {row['status']}，不能再发起重跑",
        )
    original = conn.execute(
        "SELECT * FROM tasks WHERE id = %s", (row["task_id"],)
    ).fetchone()
    if original is None:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"原任务不存在：{row['task_id']}")

    merged_params = dict(json_load(original["user_params_json"], {}) or {})
    for step_id, overrides in (user_params or {}).items():
        existing = dict(merged_params.get(step_id) or {})
        if not isinstance(overrides, dict):
            raise WorkError(
                ErrorClass.PARAM_OUT_OF_RANGE, f"步骤 {step_id} 的参数覆盖必须是对象"
            )
        existing.update(overrides)
        merged_params[step_id] = existing

    task = service.create_task(
        conn,
        settings,
        project_id=original["project_id"],
        asset_ids=json_load(original["input_asset_ids"], []) or [],
        recipe_id=original["recipe_id"],
        recipe_version=int(original["recipe_version"]),
        user_params=merged_params or None,
        priority=priority,
        actor=f"{actor}:review-rerun",
    )
    conn.execute(
        "UPDATE review_queue SET status = 'superseded', decided_by = %s, decided_at = %s, "
        "decision_note = %s, claimed_by = COALESCE(claimed_by, %s) WHERE id = %s",
        (
            actor,
            now_iso(),
            note or f"按修正后的参数重跑：{task['id']}",
            actor,
            review_id,
        ),
    )
    audit(conn, actor, "review.rerun", review_id,
          {"task_id": row["task_id"], "new_task_id": task["id"], "user_params": merged_params})
    return {"review": get_review(conn, review_id), "task": task}
