"""Worker 执行器端到端：成功、取消、重试分类、阶段复用、租约失联。"""

from __future__ import annotations

import json
import time
from pathlib import Path

import pytest

from drama_cloud.errors import ErrorClass, WorkError
from drama_cloud.tasks import service
from drama_cloud.recipes.registry import get_module
from drama_cloud.tasks.queue import TaskQueue
from drama_cloud.tasks.runner import Heartbeat, Worker
from drama_cloud.tasks.state import TaskState

from .conftest import requires_ffmpeg
from .helpers import patch_module

pytestmark = requires_ffmpeg


def prepare(conn, settings, storage, project_id, source: Path, subtitle: Path | None = None,
            recipe_id="drama.no_edit.generic"):
    video = service.register_local_asset(conn, settings, storage, project_id, source,
                                         name="正片", actor="test")
    assets = [video["id"]]
    if subtitle is not None:
        sub = service.register_local_asset(conn, settings, storage, project_id, subtitle,
                                           name="字幕", actor="test")
        assets.append(sub["id"])
    return service.create_task(conn, settings, project_id, assets, recipe_id=recipe_id, actor="test")


def test_end_to_end_success(conn, settings, storage, project, tiny_video):
    project_id, _, _ = project
    task = prepare(conn, settings, storage, project_id, tiny_video)
    assert task["state"] == "QUEUED"

    worker = Worker(settings, worker_id="wrk_test")
    assert worker.run_once() is True
    assert worker.run_once() is False  # 队列已空

    detail = service.get_task(conn, task["id"])
    assert detail["state"] == "SUCCEEDED"
    assert detail["error"] is None
    kinds = {a["kind"] for a in detail["artifacts"]}
    assert {"output", "report"} <= kinds

    output = next(a for a in detail["artifacts"] if a["kind"] == "output")
    assert output["size_bytes"] > 0
    assert output["media"]["width"] == 160 and output["media"]["height"] == 120
    assert output["quality"]["ok"] is True
    assert all(check["ok"] for check in output["quality"]["checks"])
    assert detail["result"]["worker_id"] == "wrk_test"
    assert detail["result"]["artifacts"]

    # 检查点记录了每个阶段，且 input_sha256 是计划指纹。
    checkpoints = service.get_checkpoints(conn, task["id"])
    stages = {c["stage"] for c in checkpoints}
    assert stages == {"probe", "encode", "gate"}
    assert all(c["completed"] for c in checkpoints)
    assert all(c["recipe_version"] == 1 for c in checkpoints)
    assert all(c["engine_version"] == "ffmpeg-4" for c in checkpoints)

    # 成功后工作目录被清理。
    assert not (settings.work_dir / task["id"]).exists()


def test_heartbeat_renews_the_lease_from_its_own_thread(conn, settings, storage, project,
                                                        tiny_video):
    """心跳线程必须真的推进租约时间。

    跨线程复用主连接时 sqlite3 会抛 `ProgrammingError`，而心跳**故意**吞掉异常
    （心跳失败不该让 Worker 崩），于是「线程没崩」完全不能说明续租成功了——
    只有租约时间被推进才算数。
    """
    project_id, _, _ = project
    task = prepare(conn, settings, storage, project_id, tiny_video)
    worker = Worker(settings, worker_id="wrk_beat")
    claimed = worker.queue.claim(conn, worker.worker_id, worker.capabilities)
    assert claimed is not None
    before = conn.execute(
        "SELECT lease_expires_at FROM tasks WHERE id = %s", (task["id"],)
    ).fetchone()["lease_expires_at"]

    with Heartbeat(worker, conn, task["id"]) as beat:
        time.sleep(1.5)
        assert beat.lost is False

    after = conn.execute(
        "SELECT lease_expires_at FROM tasks WHERE id = %s", (task["id"],)
    ).fetchone()["lease_expires_at"]
    assert after > before, "心跳没有推进租约（心跳线程多半在跨线程用主连接）"


def test_subtitle_burn_end_to_end(conn, settings, storage, project, tiny_video, tiny_subtitle):
    project_id, _, _ = project
    task = prepare(conn, settings, storage, project_id, tiny_video, tiny_subtitle,
                   recipe_id="drama.subtitle.burn")
    Worker(settings, worker_id="wrk_sub").run_once()
    detail = service.get_task(conn, task["id"])
    assert detail["state"] == "SUCCEEDED", detail["error"]
    output = next(a for a in detail["artifacts"] if a["kind"] == "output")
    assert output["quality"]["ok"] is True


def test_cancel_before_start(conn, settings, storage, project, tiny_video):
    project_id, _, _ = project
    task = prepare(conn, settings, storage, project_id, tiny_video)
    service.request_cancel(conn, task["id"], actor="test")
    assert service.get_task(conn, task["id"])["state"] == "CANCELLED"

    # 已取消的任务不会被 Worker 抢走，也不会留下产物。
    worker = Worker(settings, worker_id="wrk_cancel")
    assert worker.run_once() is False
    detail = service.get_task(conn, task["id"])
    assert detail["state"] == "CANCELLED" and detail["artifacts"] == []


def test_cancel_is_cooperative_and_never_marks_half_done_as_success(
    conn, settings, storage, project, tiny_video
):
    """在阶段边界收到取消请求时，任务必须落到 CANCELLED 而不是 SUCCEEDED。"""
    project_id, _, _ = project
    task = prepare(conn, settings, storage, project_id, tiny_video)

    def cancelling_probe(ctx, params):
        # 模拟「取消请求在第一个阶段执行期间到达」。
        conn.execute("UPDATE tasks SET cancel_requested = 1 WHERE id = %s", (task["id"],))
        return {}

    with patch_module("media.probe", cancelling_probe):
        Worker(settings, worker_id="wrk_cancel").run_once()

    detail = service.get_task(conn, task["id"])
    assert detail["state"] == "CANCELLED"
    assert detail["artifacts"] == []


def test_retryable_failure_then_reuse_and_success(conn, settings, storage, project, tiny_video):
    project_id, _, _ = project
    task = prepare(conn, settings, storage, project_id, tiny_video)

    calls = {"gate": 0, "encode": 0}
    real_encode = get_module("media.encode").implement

    def counting_encode(ctx, params):
        calls["encode"] += 1
        return real_encode(ctx, params)

    def flaky_gate(ctx, params):
        calls["gate"] += 1
        raise WorkError(ErrorClass.TRANSIENT_IO, "模拟磁盘抖动")

    with patch_module("media.encode", counting_encode), patch_module("media.quality_gate", flaky_gate):
        with patch_module("media.encode", counting_encode):
            pass  # 保持嵌套结构清晰（两次 patch 相同实现，等价于只 patch 一次）
        assert Worker(settings, worker_id="wrk_r1").run_once() is True

    detail = service.get_task(conn, task["id"])
    assert detail["state"] == "RETRY_WAITING"
    assert detail["error"]["class"] == str(ErrorClass.TRANSIENT_IO)
    # 可重试失败时工作目录必须保留，供下一轮复用。
    work_dir = settings.work_dir / task["id"]
    assert work_dir.exists()
    assert list(work_dir.glob("encoded*")), "编码阶段产物应当被保留下来"

    # 退避时间清零点相当于「等到了下一次可执行时间」。
    conn.execute("UPDATE tasks SET next_run_at = NULL WHERE id = %s", (task["id"],))
    with patch_module("media.encode", counting_encode):
        assert Worker(settings, worker_id="wrk_r2").run_once() is True

    detail = service.get_task(conn, task["id"])
    assert detail["state"] == "SUCCEEDED", detail["error"]
    assert detail["attempts"] == 2

    steps = {step["module"]: step["status"] for step in detail["result"]["steps"]}
    assert steps["media.encode"] == "reused", "重复执行时应当复用编码产物"
    assert steps["media.probe"] == "executed", "计划型阶段没有可复用的落盘状态，必须重跑"
    # 最硬的证据：整个生命周期里编码只真的跑过一次。
    assert calls["encode"] == 1
    # 复用路径重建的 ctx.plan["encode"] 仍要能让门禁校验通过。
    assert detail["result"]["quality"]["ok"] is True

    # attempt 记录要能追溯两次尝试。
    attempts = service.attempts_for(conn, task["id"])
    assert [a["attempt"] for a in attempts] == [1, 2]
    assert attempts[0]["error_class"] == str(ErrorClass.TRANSIENT_IO)
    assert attempts[0]["finished_at"] and attempts[1]["finished_at"]


def test_non_retryable_failure_goes_to_failed_and_cleans_up(
    conn, settings, storage, project, tiny_video
):
    project_id, _, _ = project
    task = prepare(conn, settings, storage, project_id, tiny_video)

    def broken_encode(ctx, params):
        raise WorkError(ErrorClass.INPUT_CORRUPT, "模拟输入损坏")

    with patch_module("media.encode", broken_encode):
        Worker(settings, worker_id="wrk_bad").run_once()

    detail = service.get_task(conn, task["id"])
    assert detail["state"] == "FAILED"
    assert detail["error"]["class"] == str(ErrorClass.INPUT_CORRUPT)
    assert detail["artifacts"] == []
    assert not (settings.work_dir / task["id"]).exists()


def test_quality_gate_failure_is_terminal(conn, settings, storage, project, tiny_video):
    project_id, _, _ = project
    task = prepare(conn, settings, storage, project_id, tiny_video)

    def impossible_gate(ctx, params):
        # 直接产出一个不可能通过的结论，验证门禁失败不会自动重试。
        raise WorkError(ErrorClass.QUALITY_GATE_FAILED, "模拟门禁不通过")

    with patch_module("media.quality_gate", impossible_gate):
        Worker(settings, worker_id="wrk_gate").run_once()

    detail = service.get_task(conn, task["id"])
    assert detail["state"] == "FAILED"
    assert detail["error"]["class"] == str(ErrorClass.QUALITY_GATE_FAILED)
    assert detail["artifacts"] == []


def test_manual_retry_after_failure_requeues(conn, settings, storage, project, tiny_video):
    project_id, _, _ = project
    task = prepare(conn, settings, storage, project_id, tiny_video)

    def broken_probe(ctx, params):
        raise WorkError(ErrorClass.INTERNAL, "模拟内部错误")

    with patch_module("media.probe", broken_probe):
        Worker(settings, worker_id="wrk_x").run_once()
    assert service.get_task(conn, task["id"])["state"] == "FAILED"

    service.retry_task(conn, task["id"], settings, actor="test")
    assert service.get_task(conn, task["id"])["state"] == "QUEUED"
    assert Worker(settings, worker_id="wrk_y").run_once() is True
    detail = service.get_task(conn, task["id"])
    assert detail["state"] == "SUCCEEDED"
    assert detail["attempts"] == 1  # 手动重试会把计数清零


def test_worker_without_capability_does_not_take_task(conn, settings, storage, project, tiny_video):
    project_id, _, _ = project
    task = prepare(conn, settings, storage, project_id, tiny_video)
    weak = Worker(settings, worker_id="wrk_weak", capabilities=["ffprobe"])
    assert weak.run_once() is False
    assert service.get_task(conn, task["id"])["state"] == "QUEUED"


def test_lease_loss_does_not_clobber_new_owner(conn, settings, storage, project, tiny_video):
    """一个租约已过期的 Worker 结束时，不能覆盖新持有者的状态。"""
    project_id, _, _ = project
    task = prepare(conn, settings, storage, project_id, tiny_video)
    queue = TaskQueue(settings)
    claimed = queue.claim(conn, "wrk_old", Worker(settings).capabilities)
    assert claimed is not None

    # 模拟：旧 Worker 还在跑，但租约已经易主给新 Worker。
    conn.execute(
        "UPDATE tasks SET lease_owner = 'wrk_new' WHERE id = %s", (task["id"],)
    )
    worker = Worker(settings, worker_id="wrk_old")
    worker.execute(conn, task["id"], claimed.attempt)

    row = conn.execute(
        "SELECT state, lease_owner, error_class FROM tasks WHERE id = %s", (task["id"],)
    ).fetchone()
    assert row["lease_owner"] == "wrk_new", "状态与租约都不能被旧 Worker 改写"
    assert row["state"] == str(TaskState.RUNNING)

    # 但 attempt 记录仍留有一条可诊断的错误。
    attempts = service.attempts_for(conn, task["id"])
    assert attempts[0]["error_class"] == str(ErrorClass.WORKER_LOST)


def test_idempotency_key_returns_same_task(conn, settings, storage, project, tiny_video):
    project_id, _, _ = project
    video = service.register_local_asset(conn, settings, storage, project_id, tiny_video, actor="test")
    first = service.create_task(conn, settings, project_id, [video["id"]],
                                recipe_id="drama.no_edit.generic",
                                idempotency_key="ep01", actor="test")
    second = service.create_task(conn, settings, project_id, [video["id"]],
                                 recipe_id="drama.no_edit.generic",
                                 idempotency_key="ep01", actor="test")
    assert first["id"] == second["id"]
    count = conn.execute("SELECT COUNT(*) AS n FROM tasks").fetchone()["n"]
    assert count == 1


def test_user_params_override_and_snapshot_is_frozen(conn, settings, storage, project, tiny_video):
    project_id, _, _ = project
    video = service.register_local_asset(conn, settings, storage, project_id, tiny_video, actor="test")
    task = service.create_task(
        conn, settings, project_id, [video["id"]], recipe_id="drama.no_edit.generic",
        user_params={"encode": {"crf": 30}}, actor="test",
    )
    plan = json.loads(conn.execute(
        "SELECT plan_json FROM tasks WHERE id = %s", (task["id"],)
    ).fetchone()["plan_json"])
    encode_step = next(s for s in plan["steps"] if s["module"] == "media.encode")
    assert encode_step["params"]["crf"] == 30

    # 覆盖不存在的参数会被拒绝。
    with pytest.raises(WorkError):
        service.create_task(
            conn, settings, project_id, [video["id"]], recipe_id="drama.no_edit.generic",
            user_params={"encode": {"not_a_param": 1}}, actor="test",
        )
    with pytest.raises(WorkError):
        service.create_task(
            conn, settings, project_id, [video["id"]], recipe_id="drama.no_edit.generic",
            user_params={"nope": {"crf": 30}}, actor="test",
        )


def test_task_snapshot_does_not_follow_published_pointer(conn, settings, storage, project, tiny_video):
    """任务创建后发布新版本，不会改变已创建任务的配方。"""
    project_id, _, _ = project
    video = service.register_local_asset(conn, settings, storage, project_id, tiny_video, actor="test")
    task = service.create_task(conn, settings, project_id, [video["id"]],
                               recipe_id="drama.no_edit.generic", actor="test")

    payload = json.loads(
        (Path(__file__).resolve().parents[1] / "recipes" / "drama-no-edit-generic.json")
        .read_text(encoding="utf-8")
    )
    payload["version"] = 2
    payload["pipeline"][1]["params"]["crf"] = 33
    service.publish_recipe(conn, payload, actor="test")
    assert service.resolve_recipe(conn, "drama.no_edit.generic")["version"] == 2

    detail = service.get_task(conn, task["id"], include_plan=True)
    assert detail["recipe_version"] == 1
    assert detail["recipe"]["pipeline"][1]["params"]["crf"] == 21


def test_chapter_split_produces_multiple_artifacts(conn, settings, storage, project, tiny_video):
    project_id, _, _ = project
    video = service.register_local_asset(conn, settings, storage, project_id, tiny_video, actor="test")
    # 2 秒素材切成 1 秒一集，验证 C10 的「输出多个分集文件」。
    payload = json.loads(
        (Path(__file__).resolve().parents[1] / "recipes" / "drama-episodes-split.json")
        .read_text(encoding="utf-8")
    )
    payload["recipe_id"] = "test.episodes.split"
    payload["pipeline"] = [
        step for step in payload["pipeline"] if step["module"] != "video.quality"
    ]
    payload["target"] = {
        "platform_profile": "generic-h264-mp4",
        "container": "mp4",
        "video_codec": "h264",
        "audio_codec": "aac",
    }
    for step in payload["pipeline"]:
        if step["module"] == "chapters.reorg":
            step["params"]["episode_seconds"] = 1.2
            step["params"]["min_segment_seconds"] = 0.5
    service.publish_recipe(conn, payload, actor="test")

    task = service.create_task(conn, settings, project_id, [video["id"]],
                               recipe_id="test.episodes.split", actor="test")
    Worker(settings, worker_id="wrk_split").run_once()
    detail = service.get_task(conn, task["id"])
    assert detail["state"] == "SUCCEEDED", detail["error"]
    chapters = [a for a in detail["artifacts"] if a["kind"] == "chapter"]
    assert len(chapters) == 2
    assert all(a["size_bytes"] > 0 for a in chapters)
