"""任务子系统：状态机、持久化队列、任务服务与 Worker 执行器。"""

from __future__ import annotations

from .queue import ClaimedTask, TaskQueue
from .runner import Worker, default_worker_id
from .state import ALLOWED_TRANSITIONS, TERMINAL_STATES, TaskState, can_transition, transition

__all__ = [
    "ALLOWED_TRANSITIONS",
    "ClaimedTask",
    "TERMINAL_STATES",
    "TaskQueue",
    "TaskState",
    "Worker",
    "can_transition",
    "default_worker_id",
    "transition",
]
