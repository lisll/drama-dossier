"""任务状态机（设计文档 §7.1）。

文档要求「状态转换必须由服务端校验，不能由客户端任意覆盖」。因此：

- 状态集合与允许的迁移在这里写死，`tasks` 表还有 CHECK 约束兜底；
- 所有迁移都必须经过 `transition()`，调用方只能声明「想迁到哪」，合法性由这里判定；
- API 层不直接 UPDATE `tasks.state`。

正常路径：

    PENDING ──enqueue──▶ QUEUED ──claim──▶ RUNNING ──进入门禁──▶ VERIFYING ──▶ SUCCEEDED

异常分支：

    RETRY_WAITING  可重试失败或租约过期，等退避结束后回到 QUEUED
    CANCEL_REQUESTED 收到取消请求；Worker 在步骤边界协作退出后进入 CANCELLED
    CANCELLED      终态（取消）；不会留下「半成品被标为成功」
    FAILED         终态（不可重试失败或超过最大重试次数）
"""

from __future__ import annotations

import enum
from typing import Dict, FrozenSet, Iterable, Optional

from ..db import now_iso
from ..errors import ErrorClass, WorkError


class TaskState(str, enum.Enum):
    PENDING = "PENDING"
    QUEUED = "QUEUED"
    RUNNING = "RUNNING"
    VERIFYING = "VERIFYING"
    SUCCEEDED = "SUCCEEDED"
    RETRY_WAITING = "RETRY_WAITING"
    CANCEL_REQUESTED = "CANCEL_REQUESTED"
    CANCELLED = "CANCELLED"
    FAILED = "FAILED"

    def __str__(self) -> str:  # Python 3.9 的 str-Enum 默认会打印 "TaskState.X"
        return self.value


#: 允许的迁移。反向/跳步/越权都会在这里被拒绝。
ALLOWED_TRANSITIONS: Dict[TaskState, FrozenSet[TaskState]] = {
    TaskState.PENDING: frozenset({TaskState.QUEUED, TaskState.CANCELLED, TaskState.FAILED}),
    TaskState.QUEUED: frozenset(
        {TaskState.RUNNING, TaskState.CANCEL_REQUESTED, TaskState.CANCELLED, TaskState.FAILED}
    ),
    TaskState.RUNNING: frozenset(
        {
            TaskState.VERIFYING,
            TaskState.RETRY_WAITING,
            TaskState.FAILED,
            TaskState.CANCEL_REQUESTED,
            TaskState.CANCELLED,
        }
    ),
    TaskState.VERIFYING: frozenset(
        {TaskState.SUCCEEDED, TaskState.RETRY_WAITING, TaskState.FAILED, TaskState.CANCELLED}
    ),
    TaskState.RETRY_WAITING: frozenset(
        {TaskState.QUEUED, TaskState.CANCELLED, TaskState.FAILED}
    ),
    TaskState.CANCEL_REQUESTED: frozenset(
        {TaskState.CANCELLED, TaskState.SUCCEEDED, TaskState.FAILED}
    ),
    TaskState.SUCCEEDED: frozenset(),
    TaskState.CANCELLED: frozenset(),
    TaskState.FAILED: frozenset(),
}

#: 终态：不再有任何迁移。
TERMINAL_STATES = frozenset(
    {TaskState.SUCCEEDED, TaskState.CANCELLED, TaskState.FAILED}
)

#: 可以被 Worker 抢走执行的状态。
CLAIMABLE_STATES = (TaskState.QUEUED, TaskState.RETRY_WAITING)

#: 失败分类 → 迁移目标状态。
def failure_target(error_class: ErrorClass, attempts: int, max_attempts: int) -> TaskState:
    if error_class is ErrorClass.CANCELLED:
        return TaskState.CANCELLED
    if error_class.retryable and attempts < max_attempts:
        return TaskState.RETRY_WAITING
    return TaskState.FAILED


def can_transition(current: str, target: str) -> bool:
    try:
        source, destination = TaskState(current), TaskState(target)
    except ValueError:
        return False
    return destination in ALLOWED_TRANSITIONS[source]


def is_terminal(state: str) -> bool:
    try:
        return TaskState(state) in TERMINAL_STATES
    except ValueError:
        return False


def _fetch_state(conn, task_id: str) -> str:
    row = conn.execute("SELECT state FROM tasks WHERE id = %s", (task_id,)).fetchone()
    if row is None:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"任务不存在：{task_id}")
    return row["state"]


def transition(
    conn,
    task_id: str,
    target: str,
    allowed_from: Optional[Iterable[str]] = None,
    fields: Optional[dict] = None,
    note: str = "",
) -> str:
    """原子地做一次状态迁移并返回新状态。

    `allowed_from` 是乐观并发控制：只要求「当前状态在预期集合里」才迁移，
    避免两个 Worker 同时抢到同一个任务后互相覆盖。
    """
    current = _fetch_state(conn, task_id)
    if allowed_from is not None:
        allowed = {str(s) for s in allowed_from}
        if current not in allowed:
            raise WorkError(
                ErrorClass.RESOURCE_CONTENTION,
                f"任务 {task_id} 当前状态 {current} 不在预期集合 {'/'.join(sorted(allowed))} 内",
            )
    if str(target) != current and not can_transition(current, target):
        raise WorkError(
            ErrorClass.PARAM_OUT_OF_RANGE,
            f"非法状态迁移：{current} → {target}（服务端校验拒绝）",
        )

    assignments = ["state = %s", "updated_at = %s"]
    values = [str(target), now_iso()]
    for key, value in (fields or {}).items():
        assignments.append(f"{key} = %s")
        values.append(value)
    values.append(task_id)
    conn.execute(f"UPDATE tasks SET {', '.join(assignments)} WHERE id = %s", values)
    return str(target)
