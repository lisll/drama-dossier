"""持久化队列：抢占、能力过滤、心跳、租约回收与指数退避。"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone

from drama_cloud.errors import ErrorClass, backoff_delay
from drama_cloud.tasks.queue import TaskQueue
from drama_cloud.tasks.state import TaskState

from .helpers import make_task

CAPS = ["ffmpeg", "ffprobe", "h264_encode", "subtitles", "video_filters", "audio_filters"]


def test_claim_then_empty(conn, settings, storage, project):
    project_id, _, _ = project
    task = make_task(conn, settings, storage, project_id)
    queue = TaskQueue(settings)
    claimed = queue.claim(conn, "wrk_a", CAPS)
    assert claimed is not None and claimed.task_id == task["id"]
    assert claimed.attempt == 1
    assert queue.claim(conn, "wrk_b", CAPS) is None
    state = conn.execute("SELECT state, lease_owner FROM tasks WHERE id = ?", (task["id"],)).fetchone()
    assert state["state"] == "RUNNING" and state["lease_owner"] == "wrk_a"


def test_attempt_row_is_recorded(conn, settings, storage, project):
    project_id, _, _ = project
    task = make_task(conn, settings, storage, project_id)
    TaskQueue(settings).claim(conn, "wrk_a", CAPS)
    rows = conn.execute("SELECT * FROM task_attempts WHERE task_id = ?", (task["id"],)).fetchall()
    assert len(rows) == 1 and rows[0]["attempt"] == 1 and rows[0]["finished_at"] is None


def test_capability_filter_skips_incapable_worker(conn, settings, storage, project):
    project_id, _, _ = project
    task = make_task(conn, settings, storage, project_id)
    queue = TaskQueue(settings)
    # 只声明 ffprobe：配方要求的 h264_encode 不满足，任务必须留给有能力的 Worker。
    assert queue.claim(conn, "wrk_weak", ["ffprobe"]) is None
    state = conn.execute("SELECT state FROM tasks WHERE id = ?", (task["id"],)).fetchone()
    assert state["state"] == "QUEUED"
    assert queue.claim(conn, "wrk_strong", CAPS) is not None


def test_priority_orders_claims(conn, settings, storage, project):
    project_id, _, _ = project
    low = make_task(conn, settings, storage, project_id, priority=10)
    high = make_task(conn, settings, storage, project_id, priority=1)
    queue = TaskQueue(settings)
    first = queue.claim(conn, "wrk_a", CAPS)
    assert first.task_id == high["id"] or first.task_id == low["id"]
    # 数值越小越先执行
    assert queue.claim(conn, "wrk_a", CAPS).task_id == low["id"]


def test_heartbeat_requires_ownership(conn, settings, storage, project):
    project_id, _, _ = project
    task = make_task(conn, settings, storage, project_id)
    queue = TaskQueue(settings)
    queue.claim(conn, "wrk_a", CAPS)
    assert queue.heartbeat(conn, task["id"], "wrk_a") is True
    assert queue.heartbeat(conn, task["id"], "wrk_b") is False
    assert queue.lease_valid(conn, task["id"], "wrk_a") is True
    assert queue.lease_valid(conn, task["id"], "wrk_b") is False


def _expire_lease(conn, task_id: str) -> None:
    past = (datetime.now(timezone.utc) - timedelta(seconds=60)).replace(microsecond=0).isoformat()
    conn.execute("UPDATE tasks SET lease_expires_at = ? WHERE id = ?", (past, task_id))


def test_expired_lease_is_reclaimed_and_requeued(conn, settings, storage, project):
    project_id, _, _ = project
    task = make_task(conn, settings, storage, project_id)
    queue = TaskQueue(settings)
    queue.claim(conn, "wrk_dead", CAPS)
    _expire_lease(conn, task["id"])

    assert queue.reclaim_expired(conn) == 1
    row = conn.execute(
        "SELECT state, error_class, lease_owner FROM tasks WHERE id = ?", (task["id"],)
    ).fetchone()
    assert row["state"] == str(TaskState.RETRY_WAITING)
    assert row["error_class"] == str(ErrorClass.WORKER_LOST)
    assert row["lease_owner"] is None

    # 退避结束后可以被重新抢占（测试里直接把 next_run_at 清空）。
    conn.execute("UPDATE tasks SET next_run_at = NULL WHERE id = ?", (task["id"],))
    reclaimed = queue.claim(conn, "wrk_b", CAPS)
    assert reclaimed is not None and reclaimed.attempt == 2


def test_expired_lease_after_max_attempts_fails(conn, settings, storage, project):
    project_id, _, _ = project
    task = make_task(conn, settings, storage, project_id)
    conn.execute("UPDATE tasks SET attempts = 3 WHERE id = ?", (task["id"],))
    queue = TaskQueue(settings)
    queue.claim(conn, "wrk_dead", CAPS)  # attempts 变成 4
    _expire_lease(conn, task["id"])
    queue.reclaim_expired(conn)
    row = conn.execute("SELECT state, finished_at FROM tasks WHERE id = ?", (task["id"],)).fetchone()
    assert row["state"] == str(TaskState.FAILED)
    assert row["finished_at"]


def test_retry_uses_exponential_backoff(conn, settings, storage, project):
    project_id, _, _ = project
    task = make_task(conn, settings, storage, project_id)
    queue = TaskQueue(settings)
    queue.claim(conn, "wrk_a", CAPS)
    queue.schedule_retry(conn, task["id"], ErrorClass.TRANSIENT_IO, attempt=1, detail="网络抖动")

    row = conn.execute(
        "SELECT state, next_run_at, error_class FROM tasks WHERE id = ?", (task["id"],)
    ).fetchone()
    assert row["state"] == str(TaskState.RETRY_WAITING)
    assert row["error_class"] == str(ErrorClass.TRANSIENT_IO)
    assert row["next_run_at"] > datetime.now(timezone.utc).replace(microsecond=0).isoformat()

    # 未到时间不会被抢占。
    assert queue.claim(conn, "wrk_a", CAPS) is None


def test_non_retryable_error_goes_straight_to_failed(conn, settings, storage, project):
    project_id, _, _ = project
    task = make_task(conn, settings, storage, project_id)
    queue = TaskQueue(settings)
    queue.claim(conn, "wrk_a", CAPS)
    queue.schedule_retry(conn, task["id"], ErrorClass.INPUT_CORRUPT, attempt=1, detail="坏文件")
    row = conn.execute("SELECT state, finished_at FROM tasks WHERE id = ?", (task["id"],)).fetchone()
    assert row["state"] == str(TaskState.FAILED) and row["finished_at"]


def test_backoff_is_capped():
    assert backoff_delay(1) < backoff_delay(3)
    assert backoff_delay(20) == 300.0
