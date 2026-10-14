"""Worker 执行器：抢任务 → 跑模块 → 写检查点 → 登记产物（设计文档 §7、§10）。

执行原则：

- **唯一物化点**：只有 `media.encode` 会真正产出视频文件；其余模块只往计划里写意图。
- **阶段级检查点**：每个阶段记录输入哈希、配方版本、引擎版本、参数摘要与产物哈希。
  重试时（同一个 `work/<task_id>` 目录）如果这些信息完全一致且产物还在，就跳过该阶段
  直接复用结果——对应文档「恢复前重新校验这些信息」。
- **不恢复 ffmpeg 进程内部状态**：只做阶段级复用，不假装能续传一个编码进程。
- **协作式取消**：在阶段边界检查 `cancel_requested`，取消后清理工作目录，
  绝不把半成品登记成成功产物。
"""

from __future__ import annotations

import hashlib
import os
import shutil
import socket
import threading
import time
import traceback
from dataclasses import dataclass
from pathlib import Path
from typing import List, Optional, Sequence

from ..config import Settings
from ..db import connect, json_dump, json_load, now_iso
from ..errors import ErrorClass, MAX_ATTEMPTS, WorkError
from ..plan import ENGINE_VERSION, WORKER_VERSION, PlanInput
from ..recipes.registry import ensure_modules_loaded, get_module, registered_modules
from ..storage import LocalStorage
from . import service
from .queue import TaskQueue, set_progress
from .state import TaskState, transition

def default_worker_id() -> str:
    return f"wrk_{socket.gethostname()}_{os.getpid()}"


@dataclass
class StepOutcome:
    step_id: str
    module: str
    status: str  # executed | reused | skipped
    seconds: float


class LeaseLost(Exception):
    """心跳续租失败：说明任务已被调度器分配给别的 Worker。"""


class Heartbeat:
    """后台心跳线程：编码这类长步骤期间也在续租。"""

    def __init__(self, worker: "Worker", conn, task_id: str):
        self.worker = worker
        self.conn = conn
        self.task_id = task_id
        self.stop_event = threading.Event()
        self.lost = False
        self._thread = threading.Thread(target=self._loop, name="heartbeat", daemon=True)

    def __enter__(self) -> "Heartbeat":
        self._thread.start()
        return self

    def __exit__(self, *exc) -> None:
        self.stop_event.set()
        self._thread.join(timeout=2.0)

    def _loop(self) -> None:
        interval = max(1.0, float(self.worker.settings.heartbeat_seconds))
        while not self.stop_event.wait(interval):
            try:
                if not self.worker.queue.heartbeat(self.conn, self.task_id, self.worker.worker_id):
                    self.lost = True
                    return
            except Exception:  # 心跳失败不让 Worker 崩溃；由租约过期兜底
                continue


class Worker:
    def __init__(
        self,
        settings: Settings,
        worker_id: Optional[str] = None,
        capabilities: Optional[Sequence[str]] = None,
    ):
        ensure_modules_loaded()
        self.settings = settings
        self.worker_id = worker_id or default_worker_id()
        self.capabilities = sorted(
            {str(c) for c in (capabilities or service.cluster_capabilities(settings))}
        )
        self.queue = TaskQueue(settings)
        self.storage = LocalStorage(settings)

    # ---------- 主循环 ----------

    def run_forever(
        self,
        poll_interval: float = 2.0,
        once: bool = False,
        max_tasks: Optional[int] = None,
        stop_event: Optional[threading.Event] = None,
    ) -> int:
        processed = 0
        while True:
            if stop_event is not None and stop_event.is_set():
                break
            handled = self.run_once()
            if handled:
                processed += 1
                if once or (max_tasks is not None and processed >= max_tasks):
                    break
                continue
            if once:
                break
            if stop_event is not None:
                if stop_event.wait(poll_interval):
                    break
            else:
                time.sleep(poll_interval)
        return processed

    def run_once(self) -> bool:
        conn = connect(self.settings)
        try:
            claimed = self.queue.claim(conn, self.worker_id, self.capabilities)
            if claimed is None:
                return False
            self.execute(conn, claimed.task_id, claimed.attempt)
            return True
        finally:
            conn.close()

    # ---------- 单任务执行 ----------

    def execute(self, conn, task_id: str, attempt: int) -> None:
        work_dir = self.settings.work_dir / task_id
        steps: List[StepOutcome] = []
        try:
            task = self._load_task(conn, task_id)
            self._prepare_workdir(work_dir)
            plan_inputs, paths = self._stage_inputs(conn, work_dir, task)
            context = self._build_context(work_dir, plan_inputs, paths, task)
            with Heartbeat(self, conn, task_id) as beat:
                steps = self._run_steps(conn, task, context, beat)
                if beat.lost or not self._owns(conn, task_id):
                    raise LeaseLost("心跳续租失败或租约已易主，任务已被重新分配")
                result = self._publish_artifacts(conn, task, context, steps)
            transition(
                conn,
                task_id,
                TaskState.SUCCEEDED,
                allowed_from={str(TaskState.VERIFYING)},
                fields={
                    "result_json": json_dump(result),
                    "lease_owner": None,
                    "lease_expires_at": None,
                    "finished_at": now_iso(),
                    "error_class": None,
                    "error_message": None,
                },
            )
            conn.execute(
                "UPDATE task_attempts SET finished_at = ? WHERE task_id = ? AND attempt = ?",
                (now_iso(), task_id, attempt),
            )
            set_progress(conn, task_id, {"stage": "succeeded", "steps": [s.__dict__ for s in steps]})
            self._cleanup(work_dir, keep=False)
        except LeaseLost as lost:
            # 任务已不属于本 Worker，什么都不改，避免覆盖新 Worker 的状态。
            self._record_attempt_error(conn, task_id, attempt, ErrorClass.WORKER_LOST, str(lost))
            return
        except WorkError as error:
            self._fail(conn, task_id, attempt, error.error_class, error.message, steps, work_dir)
        except Exception as error:  # 未预期异常按 Internal 处理，但要留下堆栈
            detail = f"{type(error).__name__}: {error}\n{traceback.format_exc(limit=6)}"
            self._fail(conn, task_id, attempt, ErrorClass.INTERNAL, detail, steps, work_dir)

    # ---------- 各阶段 ----------

    def _load_task(self, conn, task_id: str) -> dict:
        row = conn.execute("SELECT * FROM tasks WHERE id = ?", (task_id,)).fetchone()
        if row is None:
            raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"任务不存在：{task_id}")
        return {
            "id": row["id"],
            "plan": json_load(row["plan_json"], {}) or {},
            "recipe": json_load(row["recipe_snapshot_json"], {}) or {},
            "input_asset_ids": json_load(row["input_asset_ids"], []) or [],
            "attempts": int(row["attempts"]),
            "state": row["state"],
        }

    def _prepare_workdir(self, work_dir: Path) -> None:
        work_dir.mkdir(parents=True, exist_ok=True)

    def _stage_inputs(self, conn, work_dir: Path, task: dict):
        """把输入素材链接/复制进工作目录；返回 (PlanInput 列表, 本地路径列表)。"""
        inputs = service.load_plan_inputs(conn, task["input_asset_ids"])
        paths: List[Path] = []
        for item in inputs:
            source = self.storage.local_path(item.storage_key)
            if not source.exists():
                raise WorkError(
                    ErrorClass.STORAGE_UNAVAILABLE, f"素材对象缺失：{item.storage_key}"
                )
            target = work_dir / f"input_{len(paths):02d}{source.suffix or '.bin'}"
            if not target.exists() or target.stat().st_size != source.stat().st_size:
                try:
                    os.link(source, target)
                except OSError:
                    shutil.copy2(source, target)
            paths.append(target)
        return inputs, paths

    def _build_context(self, work_dir: Path, inputs: Sequence[PlanInput], paths: Sequence[Path], task: dict):
        from ..modules.ffmpeg import MediaContext

        device = next((i for i, item in enumerate(inputs) if item.role == "video"), None)
        if device is None:
            raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "任务没有 video 角色的输入")
        return MediaContext(
            settings=self.settings,
            work_dir=work_dir,
            primary=paths[device],
            inputs=list(paths),
            plan={"quality_policy": task["plan"].get("quality_policy") or {}},
        )

    def _run_steps(self, conn, task: dict, context, beat: Heartbeat) -> List[StepOutcome]:
        outcomes: List[StepOutcome] = []
        plan_steps = task["plan"].get("steps") or []
        if not plan_steps:
            raise WorkError(ErrorClass.INTERNAL, "计划里没有可执行步骤")

        for index, step in enumerate(plan_steps):
            if self._cancelled(conn, task["id"]):
                self._cancel(conn, task["id"], context)
                raise WorkError(ErrorClass.CANCELLED, "任务已按请求取消")
            if beat.lost:
                raise LeaseLost("心跳续租失败，任务已被重新分配")

            module = step["module"]
            if module not in registered_modules():
                raise WorkError(ErrorClass.CAPABILITY_MISSING, f"模块未注册：{module}")
            if module == "media.quality_gate":
                # 进入门禁阶段：状态机允许的 VERIFYING 语义就是「产物已生成，正在校验」。
                transition(conn, task["id"], TaskState.VERIFYING,
                           allowed_from={str(TaskState.RUNNING)})

            set_progress(conn, task["id"], {
                "stage": module,
                "index": index,
                "total": len(plan_steps),
                "steps": [s.__dict__ for s in outcomes],
            })

            started = time.time()
            reused = self._try_reuse(conn, task, step, context)
            if not reused:
                spec = get_module(module)
                params = spec.params_model.model_validate(step.get("params") or {})
                spec.implement(context, params)
                self._write_checkpoint(conn, task, step, context)
            outcomes.append(
                StepOutcome(step["id"], module, "reused" if reused else "executed",
                            round(time.time() - started, 3))
            )
            if not self.queue.heartbeat(conn, task["id"], self.worker_id):
                raise LeaseLost("心跳续租失败，任务已被重新分配")

        state = conn.execute("SELECT state FROM tasks WHERE id = ?", (task["id"],)).fetchone()["state"]
        if state == str(TaskState.RUNNING):
            # 配方没有质量门禁步骤时，也要在登记产物前经过 VERIFYING。
            transition(conn, task["id"], TaskState.VERIFYING,
                       allowed_from={str(TaskState.RUNNING)})
        return outcomes

    # ---------- 检查点 ----------

    def _params_digest(self, step: dict) -> str:
        payload = json_dump(step.get("params") or {})
        return hashlib.sha256(payload.encode("utf-8")).hexdigest()

    def _checkpoint_row(self, conn, task_id: str, stage: str):
        return conn.execute(
            "SELECT * FROM task_checkpoints WHERE task_id = ? AND stage = ?", (task_id, stage)
        ).fetchone()

    #: 只有「产出文件的阶段」才有资格被复用。像 media.probe / timeline.user_edits 这类
    #: 计划型模块，其产物是内存里的 `ctx.plan`，没有可以复用的落盘状态；跳过它们会让
    #: 后续步骤拿到空上下文。它们本身几乎不耗时，重跑一遍才是正确做法。
    REUSABLE_MODULES = frozenset({"media.encode"})

    def _try_reuse(self, conn, task: dict, step: dict, context) -> bool:
        """阶段级复用：输入哈希 / 配方版本 / 引擎版本 / 参数摘要 / 产物哈希全部一致才复用。"""
        if step["module"] not in self.REUSABLE_MODULES:
            return False
        row = self._checkpoint_row(conn, task["id"], step["id"])
        if row is None or not row["completed"]:
            return False
        if row["recipe_version"] != task["plan"].get("recipe_version"):
            return False
        if row["engine_version"] != ENGINE_VERSION:
            return False
        if row["params_digest"] != self._params_digest(step):
            return False
        artifact_key = row["artifact_key"] or ""
        if not artifact_key:
            return False
        path = Path(artifact_key)
        if not path.exists() or self._sha256(path) != row["artifact_sha256"]:
            return False

        context.encoded = path
        # 门禁要用 ctx.plan["encode"] 校验编码器/容器，而复用路径没有跑过
        # implement_encode，所以这里从冻结的计划里重建同一份信息。
        params = step.get("params") or {}
        context.plan["encode"] = {
            "video_codec": params.get("video_codec"),
            "audio_codec": params.get("audio_codec"),
            "container": params.get("container"),
            "rate_control": params.get("rate_control"),
            "crf": params.get("crf"),
        }
        context.notes.append(f"复用阶段产物：{step['id']}（{path.name}）")
        return True

    def _write_checkpoint(self, conn, task: dict, step: dict, context) -> None:
        artifact_path = ""
        artifact_sha = ""
        if step["module"] == "media.encode" and context.encoded is not None:
            artifact_path = str(context.encoded)
            artifact_sha = self._sha256(context.encoded)
        conn.execute(
            """
            INSERT INTO task_checkpoints (task_id, stage, input_sha256, recipe_version,
                                          engine_version, params_digest, artifact_key,
                                          artifact_sha256, completed, updated_at)
            VALUES (?,?,?,?,?,?,?,?,1,?)
            ON CONFLICT(task_id, stage) DO UPDATE SET
                input_sha256 = excluded.input_sha256,
                recipe_version = excluded.recipe_version,
                engine_version = excluded.engine_version,
                params_digest = excluded.params_digest,
                artifact_key = excluded.artifact_key,
                artifact_sha256 = excluded.artifact_sha256,
                completed = 1,
                updated_at = excluded.updated_at
            """,
            (
                task["id"],
                step["id"],
                task["plan"].get("fingerprint") or "",
                int(task["plan"].get("recipe_version") or 0),
                ENGINE_VERSION,
                self._params_digest(step),
                artifact_path,
                artifact_sha,
                now_iso(),
            ),
        )

    # ---------- 产物登记 ----------

    def _publish_artifacts(self, conn, task: dict, context, steps: List[StepOutcome]) -> dict:
        report = context.plan.get("quality_report") or {}
        registered: List[dict] = []
        readme = self._build_readme(task, context, steps, report)

        if context.encoded is not None and context.encoded.exists():
            chapters = context.plan.get("chapters") or {}
            if chapters.get("split"):
                from ..modules.encode import split_chapters

                parts = split_chapters(
                    context.encoded, chapters["ranges"], context.work_dir, self.settings
                )
                for index, part in enumerate(parts):
                    registered.append(
                        service.register_artifact(
                            conn, self.storage, task["id"], "chapter", part,
                            media={"index": index + 1, "source": "stream-copy"},
                            quality={k: report.get(k) for k in ("ok", "sha256")},
                            retain_days=self.settings.artifact_retention_days,
                        )
                    )

            registered.append(
                service.register_artifact(
                    conn, self.storage, task["id"], "output", context.encoded,
                    media=report.get("media") or {},
                    quality=report,
                    retain_days=self.settings.artifact_retention_days,
                )
            )

        for entry in context.artifacts:
            path = entry.get("path")
            if path is None or not Path(str(path)).exists():
                continue
            registered.append(
                service.register_artifact(
                    conn, self.storage, task["id"], entry.get("kind", "report"), path,
                    media=entry.get("media") or {},
                    quality={},
                    name=entry.get("name"),
                    retain_days=self.settings.artifact_retention_days,
                )
            )

        report_path = context.work_dir / "readme.json"
        report_path.write_text(json_dump(readme), encoding="utf-8")
        registered.append(
            service.register_artifact(
                conn, self.storage, task["id"], "report", report_path,
                media={"steps": len(steps)},
                quality={},
                name="run_report.json",
                retain_days=self.settings.artifact_retention_days,
            )
        )

        return {
            "artifacts": [
                {"id": a["id"], "kind": a["kind"], "sha256": a["sha256"], "size_bytes": a["size_bytes"]}
                for a in registered
            ],
            "quality": {"ok": bool(report.get("ok")), "checks": report.get("checks") or []},
            "steps": [s.__dict__ for s in steps],
            "worker_id": self.worker_id,
            "worker_version": WORKER_VERSION,
            "engine_version": ENGINE_VERSION,
        }

    def _build_readme(self, task: dict, context, steps: List[StepOutcome], report: dict) -> dict:
        return {
            "task_id": task["id"],
            "recipe_id": task["plan"].get("recipe_id"),
            "recipe_version": task["plan"].get("recipe_version"),
            "recipe_sha256": task["plan"].get("recipe_sha256"),
            "plan_fingerprint": task["plan"].get("fingerprint"),
            "worker_id": self.worker_id,
            "worker_version": WORKER_VERSION,
            "engine_version": ENGINE_VERSION,
            "generated_at": now_iso(),
            "steps": [s.__dict__ for s in steps],
            "notes": list(context.notes),
            "probe": context.probe,
            "quality_report": report,
        }

    # ---------- 失败与取消 ----------

    def _owns(self, conn, task_id: str) -> bool:
        """租约归属检查：只有当前租约持有者才能改写任务状态。

        没有这道闸门，一个「心跳已失败但 FFmpeg 还在跑」的 Worker 在结束时会把状态
        覆盖到别的 Worker 正在执行的任务上。
        """
        row = conn.execute(
            "SELECT lease_owner, state FROM tasks WHERE id = ?", (task_id,)
        ).fetchone()
        return bool(row) and row["lease_owner"] == self.worker_id

    def _cancelled(self, conn, task_id: str) -> bool:
        row = conn.execute(
            "SELECT cancel_requested FROM tasks WHERE id = ?", (task_id,)
        ).fetchone()
        return bool(row and row["cancel_requested"])

    def _cancel(self, conn, task_id: str, context) -> None:
        if not self._owns(conn, task_id):
            return
        transition(
            conn,
            task_id,
            TaskState.CANCELLED,
            allowed_from={str(TaskState.RUNNING), str(TaskState.VERIFYING),
                          str(TaskState.CANCEL_REQUESTED)},
            fields={
                "error_class": str(ErrorClass.CANCELLED),
                "error_message": "用户取消（Worker 在阶段边界退出）",
                "lease_owner": None,
                "lease_expires_at": None,
                "finished_at": now_iso(),
            },
        )
        set_progress(conn, task_id, {"stage": "cancelled"})
        self._cleanup(context.work_dir, keep=False)

    def _fail(self, conn, task_id: str, attempt: int, error_class: ErrorClass,
              detail: str, steps: List[StepOutcome], work_dir: Path) -> None:
        self._record_attempt_error(conn, task_id, attempt, error_class, detail)
        current = conn.execute("SELECT state FROM tasks WHERE id = ?", (task_id,)).fetchone()
        if current is None:
            return
        if not self._owns(conn, task_id):
            # 租约已被回收/易主：只留下 attempt 记录，状态由新租约持有者负责。
            return
        allowed = {str(TaskState.RUNNING), str(TaskState.VERIFYING),
                   str(TaskState.CANCEL_REQUESTED)}
        if current["state"] not in allowed:
            # 例如已被取消，不再覆盖状态。
            return
        max_attempts = MAX_ATTEMPTS
        target = self.queue.schedule_retry(
            conn, task_id, error_class, attempt, detail, max_attempts=max_attempts
        )
        set_progress(conn, task_id, {
            "stage": "retry_waiting" if target == str(TaskState.RETRY_WAITING) else "failed",
            "steps": [s.__dict__ for s in steps],
            "error_class": str(error_class),
        })
        if target != str(TaskState.RETRY_WAITING):
            # 不再重试时清理工作目录；重试则保留，供阶段级复用。
            self._cleanup(work_dir, keep=False)

    def _record_attempt_error(self, conn, task_id: str, attempt: int,
                              error_class: ErrorClass, detail: str) -> None:
        conn.execute(
            "UPDATE task_attempts SET finished_at = ?, error_class = ?, error_message = ? "
            "WHERE task_id = ? AND attempt = ?",
            (now_iso(), str(error_class), detail[:2000], task_id, attempt),
        )

    def _cleanup(self, work_dir: Path, keep: bool) -> None:
        if keep:
            return
        shutil.rmtree(work_dir, ignore_errors=True)

    # ---------- 工具 ----------

    @staticmethod
    def _sha256(path: Path) -> str:
        digest = hashlib.sha256()
        with open(path, "rb") as handle:
            for block in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(block)
        return digest.hexdigest()
