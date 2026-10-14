"""持久化任务队列（设计文档 §7.2）。

文档建议 Celery + Redis；本机没有 Redis。但文档同时要求「关键任务状态仍以数据库为准」，
因此这里直接在 SQLite 上实现同一套语义，概念一一对应：

| 文档概念 | 本实现 |
| --- | --- |
| 队列 | `tasks.state IN ('QUEUED','RETRY_WAITING')` + `next_run_at` |
| 优先级 | `tasks.priority`，数值越小越先执行（同 nice 值语义） |
| 抢占 | `claim()` 在 `BEGIN IMMEDIATE` 事务里选中并标记，天然只被一个 Worker 抢到 |
| 租约 | `lease_owner` / `lease_expires_at` |
| 心跳 | `heartbeat()` 续租 |
| 失联回收 | `reclaim_expired()` 把过期租约的任务放回 RETRY_WAITING |
| 指数退避 | `errors.backoff_delay()` 写进 `next_run_at` |

换回 Redis 只需要替换这个类；`service.py` 与 `runner.py` 的调用点不变。
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from typing import List, Optional, Sequence

from ..config import Settings
from ..db import json_dump, json_load, now_iso, transaction
from ..errors import ErrorClass, backoff_delay
from .state import CLAIMABLE_STATES, TaskState, failure_target, transition

#: 一次 claim 事务里最多检查多少个候选任务（用于按能力过滤）。
CANDIDATE_BATCH = 25


def _iso_after(seconds: float) -> str:
    moment = datetime.now(timezone.utc) + timedelta(seconds=max(0.0, seconds))
    return moment.replace(microsecond=0).isoformat()


@dataclass
class ClaimedTask:
    """被抢到的任务：队列层只回传 Worker 执行需要的最小信息。"""

    task_id: str
    attempt: int
    priority: int
    recipe_id: str
    recipe_version: int
    required_capabilities: List[str]

    @property
    def required(self) -> set:
        return set(self.required_capabilities)


class TaskQueue:
    def __init__(self, settings: Settings):
        self.settings = settings

    # ---------- 入队 ----------

    def enqueue(self, conn, task_id: str, delay_seconds: float = 0.0) -> str:
        """把 PENDING 任务放进队列（或按退避时间延迟可执行）。"""
        next_run = _iso_after(delay_seconds) if delay_seconds > 0 else None
        state = transition(
            conn,
            task_id,
            TaskState.QUEUED,
            allowed_from=None,
            fields={"next_run_at": next_run, "lease_owner": None, "lease_expires_at": None},
        )
        return state

    def schedule_retry(self, conn, task_id: str, error_class: ErrorClass, attempt: int,
                       detail: str = "", max_attempts: int = 3) -> str:
        """按失败分类决定：重排队（指数退避）还是直接失败。"""
        target = failure_target(error_class, attempt, max_attempts)
        if target is TaskState.RETRY_WAITING:
            delay = backoff_delay(attempt)
            return transition(
                conn,
                task_id,
                target,
                fields={
                    "error_class": str(error_class),
                    "error_message": detail,
                    "next_run_at": _iso_after(delay),
                    "lease_owner": None,
                    "lease_expires_at": None,
                },
            )
        fields = {
            "error_class": str(error_class),
            "error_message": detail,
            "lease_owner": None,
            "lease_expires_at": None,
        }
        if target is TaskState.FAILED:
            fields["finished_at"] = now_iso()
        return transition(conn, task_id, target, fields=fields)

    # ---------- 抢占与心跳 ----------

    def claim(self, conn, worker_id: str, capabilities: Sequence[str]) -> Optional[ClaimedTask]:
        """原子抢占一个任务；没有可执行任务时返回 None。

        能力过滤在事务内完成：配方要求的能力 Worker 没有，就跳过这个任务，
        留给具备该能力的 Worker（而不是把一个任务永久卡死或错误地判失败）。
        """
        caps = {str(c) for c in capabilities}
        now = now_iso()
        with transaction(conn):
            self.reclaim_expired(conn)
            rows = conn.execute(
                """
                SELECT id, priority, attempts, required_capabilities, recipe_id, recipe_version
                  FROM tasks
                 WHERE state IN (?, ?)
                   AND (next_run_at IS NULL OR next_run_at <= ?)
                 ORDER BY priority ASC, created_at ASC
                 LIMIT ?
                """,
                (*[str(s) for s in CLAIMABLE_STATES], now, CANDIDATE_BATCH),
            ).fetchall()

            for row in rows:
                required = set(json_load(row["required_capabilities"], []) or [])
                if not required <= caps:
                    continue
                attempt = int(row["attempts"]) + 1
                # 乐观并发：只有仍处于可抢占状态才真的抢到。
                updated = conn.execute(
                    """
                    UPDATE tasks
                       SET state = ?, attempts = ?, lease_owner = ?, lease_expires_at = ?,
                           started_at = COALESCE(started_at, ?), updated_at = ?
                     WHERE id = ? AND state IN (?, ?)
                    """,
                    (
                        str(TaskState.RUNNING),
                        attempt,
                        worker_id,
                        _iso_after(self.settings.lease_seconds),
                        now,
                        now,
                        row["id"],
                        str(CLAIMABLE_STATES[0]),
                        str(CLAIMABLE_STATES[1]),
                    ),
                ).rowcount
                if not updated:
                    continue
                conn.execute(
                    """
                    INSERT OR REPLACE INTO task_attempts
                        (task_id, attempt, worker_id, started_at, finished_at, error_class, error_message)
                    VALUES (?, ?, ?, ?, NULL, NULL, NULL)
                    """,
                    (row["id"], attempt, worker_id, now),
                )
                return ClaimedTask(
                    task_id=row["id"],
                    attempt=attempt,
                    priority=int(row["priority"]),
                    recipe_id=row["recipe_id"],
                    recipe_version=int(row["recipe_version"]),
                    required_capabilities=sorted(required),
                )
        return None

    def heartbeat(self, conn, task_id: str, worker_id: str) -> bool:
        """续租。返回 False 表示租约已易主（Worker 应该停下来）。"""
        now = now_iso()
        changed = conn.execute(
            """
            UPDATE tasks SET lease_expires_at = ?, updated_at = ?
             WHERE id = ? AND lease_owner = ? AND state IN (?, ?)
            """,
            (
                _iso_after(self.settings.lease_seconds),
                now,
                task_id,
                worker_id,
                str(TaskState.RUNNING),
                str(TaskState.VERIFYING),
            ),
        ).rowcount
        return bool(changed)

    def lease_valid(self, conn, task_id: str, worker_id: str) -> bool:
        row = conn.execute(
            "SELECT lease_owner, lease_expires_at FROM tasks WHERE id = ?", (task_id,)
        ).fetchone()
        if row is None or row["lease_owner"] != worker_id:
            return False
        expires = row["lease_expires_at"]
        return bool(expires) and expires > now_iso()

    def reclaim_expired(self, conn) -> int:
        """回收过期租约：Worker 被 kill 后任务不会永久停在 RUNNING。"""
        now = now_iso()
        rows = conn.execute(
            """
            SELECT id, attempts, error_class FROM tasks
             WHERE state IN (?, ?) AND lease_expires_at IS NOT NULL AND lease_expires_at < ?
            """,
            (str(TaskState.RUNNING), str(TaskState.VERIFYING), now),
        ).fetchall()
        recovered = 0
        for row in rows:
            attempts = int(row["attempts"])
            target = failure_target(
                ErrorClass.WORKER_LOST, attempts, max_attempts=3
            )
            fields = {
                "error_class": str(ErrorClass.WORKER_LOST),
                "error_message": "租约过期，Worker 失联，任务已重新排队",
                "lease_owner": None,
                "lease_expires_at": None,
            }
            if target is TaskState.RETRY_WAITING:
                fields["next_run_at"] = _iso_after(backoff_delay(attempts))
            else:
                fields["finished_at"] = now_iso()
            transition(conn, row["id"], target, allowed_from=None, fields=fields)
            conn.execute(
                "UPDATE task_attempts SET finished_at = ?, error_class = ?, error_message = ? "
                "WHERE task_id = ? AND attempt = ? AND finished_at IS NULL",
                (now, str(ErrorClass.WORKER_LOST), "租约过期", row["id"], attempts),
            )
            recovered += 1
        return recovered

    # ---------- 查询 ----------

    def stats(self, conn) -> dict:
        rows = conn.execute(
            "SELECT state, COUNT(*) AS n FROM tasks GROUP BY state ORDER BY state"
        ).fetchall()
        return {row["state"]: int(row["n"]) for row in rows}

    def depth(self, conn) -> int:
        row = conn.execute(
            "SELECT COUNT(*) AS n FROM tasks WHERE state IN (?, ?)",
            (str(TaskState.QUEUED), str(TaskState.RETRY_WAITING)),
        ).fetchone()
        return int(row["n"])


def set_progress(conn, task_id: str, payload: dict) -> None:
    """把阶段进度写回任务（API 查询用），不改变状态。"""
    conn.execute(
        "UPDATE tasks SET progress_json = ?, updated_at = ? WHERE id = ?",
        (json_dump(payload), now_iso(), task_id),
    )
