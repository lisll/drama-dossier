"""状态机：合法/非法迁移与失败分类。"""

from __future__ import annotations

import pytest

from drama_cloud.errors import ErrorClass, WorkError
from drama_cloud.tasks.state import (
    ALLOWED_TRANSITIONS,
    TERMINAL_STATES,
    TaskState,
    can_transition,
    failure_target,
    is_terminal,
    transition,
)

from .helpers import make_task


def test_happy_path_is_allowed():
    path = ["PENDING", "QUEUED", "RUNNING", "VERIFYING", "SUCCEEDED"]
    for current, target in zip(path, path[1:]):
        assert can_transition(current, target), f"{current} → {target} 应该允许"


@pytest.mark.parametrize(
    "current,target",
    [
        ("PENDING", "RUNNING"),      # 不能跳过排队
        ("PENDING", "SUCCEEDED"),    # 不能凭空成功
        ("QUEUED", "SUCCEEDED"),
        ("SUCCEEDED", "RUNNING"),    # 终态不接受任何迁移
        ("FAILED", "QUEUED"),
        ("CANCELLED", "RUNNING"),
        ("VERIFYING", "RUNNING"),    # 不允许回退
    ],
)
def test_illegal_transitions_are_rejected(current, target):
    assert not can_transition(current, target)


def test_terminal_states_have_no_outgoing_edges():
    for state in TERMINAL_STATES:
        assert ALLOWED_TRANSITIONS[state] == frozenset()
        assert is_terminal(str(state))


@pytest.mark.parametrize(
    "error_class,attempts,expected",
    [
        (ErrorClass.TRANSIENT_IO, 1, "RETRY_WAITING"),
        (ErrorClass.TRANSIENT_IO, 3, "FAILED"),          # 超过最大次数
        (ErrorClass.INPUT_CORRUPT, 1, "FAILED"),          # 不可重试
        (ErrorClass.QUALITY_GATE_FAILED, 1, "FAILED"),
        (ErrorClass.CANCELLED, 1, "CANCELLED"),
        (ErrorClass.WORKER_LOST, 2, "RETRY_WAITING"),
    ],
)
def test_failure_target(error_class, attempts, expected):
    assert failure_target(error_class, attempts, max_attempts=3) is TaskState(expected)


def test_transition_is_persisted_and_validated(conn, settings, storage, project):
    project_id, _, _ = project
    task = make_task(conn, settings, storage, project_id)
    assert task["state"] == "QUEUED"

    transition(conn, task["id"], TaskState.RUNNING)
    row = conn.execute("SELECT state, updated_at FROM tasks WHERE id = ?", (task["id"],)).fetchone()
    assert row["state"] == "RUNNING"

    with pytest.raises(WorkError) as error:
        transition(conn, task["id"], TaskState.SUCCEEDED)
    assert error.value.error_class is ErrorClass.PARAM_OUT_OF_RANGE


def test_optimistic_guard_rejects_stale_writer(conn, settings, storage, project):
    project_id, _, _ = project
    task = make_task(conn, settings, storage, project_id)
    transition(conn, task["id"], TaskState.RUNNING, allowed_from={str(TaskState.QUEUED)})
    # 第二个写入者仍以为任务是 QUEUED，必须被拒绝。
    with pytest.raises(WorkError) as error:
        transition(conn, task["id"], TaskState.RUNNING, allowed_from={str(TaskState.QUEUED)})
    assert error.value.error_class is ErrorClass.RESOURCE_CONTENTION


def test_transition_on_missing_task(conn):
    with pytest.raises(WorkError):
        transition(conn, "tsk_missing", TaskState.RUNNING)
