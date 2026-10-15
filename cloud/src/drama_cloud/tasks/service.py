"""任务服务：配方版本管理、任务创建/取消/重试、产物登记（设计文档 §6.1、§7、§9）。

这一层是「控制面」的业务内核，API 与 CLI 都只调用它。它保证：

- **配方不可变**：每次发布产生新的 `(recipe_id, version)`，内容哈希随内容变化；
- **任务锁定版本**：创建任务时把配方快照写进 `tasks.recipe_snapshot_json`，
  之后发布新版本不会改变已在跑的任务（§6.1）；
- **幂等键**：同一个 `idempotency_key` 重复提交只会得到同一个任务；
- **取消与重试受状态机约束**：非法操作直接被拒绝。
"""

from __future__ import annotations

import copy
import secrets
from typing import List, Optional, Sequence

from ..config import Settings
from ..db import audit, json_dump, json_load, now_iso, transaction
from ..errors import ErrorClass, WorkError
from ..plan import PlanInput, build_plan
from ..recipes.registry import worker_capabilities
from ..recipes.schema import SCHEMA_VERSION, validate_recipe
from ..storage import LocalStorage
from .queue import TaskQueue
from .state import TERMINAL_STATES, TaskState, transition

#: 允许的配方状态（与 recipe_versions.status 的 CHECK 一致）。
RECIPE_STATUSES = ("draft", "reviewing", "published", "deprecated", "revoked")


def new_id(prefix: str) -> str:
    return f"{prefix}_{secrets.token_hex(8)}"


def cluster_capabilities(settings: Settings) -> List[str]:
    """本次部署声明的能力集合（用于创建任务时的前置校验）。"""
    caps = set(worker_capabilities(settings))
    caps.update(str(c) for c in settings.capabilities)
    return sorted(caps)


# ---------- 用户与项目 ----------


def create_project(conn, name: str, owner_id: str, actor: str = "system") -> str:
    project_id = new_id("prj")
    conn.execute(
        "INSERT INTO projects (id, name, owner_id, created_at) VALUES (?,?,?,?)",
        (project_id, name, owner_id, now_iso()),
    )
    audit(conn, actor, "project.create", project_id, {"name": name})
    return project_id


def ensure_default_project(conn, owner_id: str, name: str = "默认项目") -> str:
    row = conn.execute(
        "SELECT id FROM projects WHERE owner_id = ? ORDER BY created_at LIMIT 1", (owner_id,)
    ).fetchone()
    if row:
        return row["id"]
    return create_project(conn, name, owner_id)


# ---------- 素材 ----------


ASSET_ROLES = ("video", "audio", "subtitle", "metadata")


def register_asset(
    conn,
    storage: LocalStorage,
    project_id: str,
    name: str,
    source,
    role: str = "video",
    filename: Optional[str] = None,
    authorization: str = "",
    actor: str = "system",
) -> dict:
    """登记一份素材。`source` 可以是本地路径或二进制流。"""
    from pathlib import Path

    if role not in ASSET_ROLES:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"未知素材角色：{role}")
    asset_id = new_id("ast")
    final_name = filename or Path(str(source)).name or name
    key = LocalStorage.asset_key(asset_id, final_name)
    if isinstance(source, (str, Path)):
        stat = storage.put_file(key, Path(source))
    else:
        stat = storage.put_stream(key, source)
    declaration = {
        "role": role,
        "original_filename": final_name,
        "authorization": authorization or "self-produced",
    }
    conn.execute(
        """
        INSERT INTO assets (id, project_id, name, storage_key, sha256, size_bytes,
                            probe_json, source_declaration, lifecycle, created_at)
        VALUES (?,?,?,?,?,?,NULL,?, 'ready', ?)
        """,
        (asset_id, project_id, name, key, stat.sha256, stat.size_bytes,
         json_dump(declaration), now_iso()),
    )
    audit(conn, actor, "asset.register", asset_id, {"role": role, "size": stat.size_bytes})
    return {
        "id": asset_id,
        "project_id": project_id,
        "name": name,
        "storage_key": key,
        "sha256": stat.sha256,
        "size_bytes": stat.size_bytes,
        "role": role,
    }


def set_asset_probe(conn, asset_id: str, summary: dict) -> None:
    conn.execute("UPDATE assets SET probe_json = ? WHERE id = ?", (json_dump(summary), asset_id))


def load_plan_inputs(conn, asset_ids: Sequence[str]) -> List[PlanInput]:
    """按任务声明的顺序取出输入素材，角色来自登记时的声明。"""
    result: List[PlanInput] = []
    for asset_id in asset_ids:
        row = conn.execute(
            "SELECT id, storage_key, sha256, name, source_declaration FROM assets WHERE id = ?",
            (asset_id,),
        ).fetchone()
        if row is None:
            raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"素材不存在：{asset_id}")
        declaration = json_load(row["source_declaration"], {}) or {}
        result.append(
            PlanInput(
                asset_id=row["id"],
                storage_key=row["storage_key"],
                role=str(declaration.get("role") or "video"),
                sha256=row["sha256"],
                name=row["name"],
            )
        )
    return result


# ---------- 配方版本管理（§6.1） ----------


def _next_version(conn, recipe_id: str) -> int:
    row = conn.execute(
        "SELECT COALESCE(MAX(version), 0) AS v FROM recipe_versions WHERE recipe_id = ?",
        (recipe_id,),
    ).fetchone()
    return int(row["v"]) + 1


def publish_recipe(
    conn,
    payload: dict,
    actor: str = "system",
    status: str = "published",
    rollout_percent: int = 100,
    reviewed_by: Optional[str] = None,
) -> dict:
    """发布一个新版本。已发布配方不允许原地修改，只能发布新版本。"""
    if status not in RECIPE_STATUSES:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"未知配方状态：{status}")
    recipe = validate_recipe(payload)
    recipe_id = recipe.recipe_id
    version = recipe.version
    existing = conn.execute(
        "SELECT content_sha256 FROM recipe_versions WHERE recipe_id = ? AND version = ?",
        (recipe_id, version),
    ).fetchone()
    if existing is not None:
        raise WorkError(
            ErrorClass.PARAM_OUT_OF_RANGE,
            f"配方 {recipe_id} 的版本 {version} 已存在且不可修改；请发布新版本",
        )

    content = recipe.content_dict()
    content_sha = recipe.content_sha256()
    required = sorted(set(recipe.compatibility.required_capabilities))

    conn.execute(
        """
        INSERT INTO recipes (id, name, purpose, published_version, created_at)
        VALUES (?,?,?,?,?)
        ON CONFLICT(id) DO UPDATE SET name = excluded.name, purpose = excluded.purpose
        """,
        (recipe_id, recipe_id, recipe.preset.mode, None, now_iso()),
    )
    conn.execute(
        """
        INSERT INTO recipe_versions (recipe_id, version, status, content_json, content_sha256,
                                     schema_version, min_worker_version, required_capabilities,
                                     rollout_percent, reviewed_by, published_at, created_at)
        VALUES (?,?,?,?,?,?,?,?,?,?,?,?)
        """,
        (
            recipe_id,
            version,
            status,
            json_dump(content),
            content_sha,
            SCHEMA_VERSION,
            recipe.compatibility.min_worker_version,
            json_dump(required),
            int(rollout_percent),
            reviewed_by,
            now_iso() if status == "published" else None,
            now_iso(),
        ),
    )
    if status == "published":
        conn.execute(
            "UPDATE recipes SET published_version = ? WHERE id = ?", (version, recipe_id)
        )
    audit(
        conn,
        actor,
        "recipe.publish",
        f"{recipe_id}@{version}",
        {"status": status, "sha256": content_sha, "rollout_percent": rollout_percent},
    )
    return {
        "recipe_id": recipe_id,
        "version": version,
        "status": status,
        "content_sha256": content_sha,
        "required_capabilities": required,
    }


def set_recipe_status(conn, recipe_id: str, version: int, status: str, actor: str = "system") -> dict:
    """撤回/弃用某个版本。撤回当前发布版本时，发布指针回退到上一个可用版本。"""
    if status not in RECIPE_STATUSES:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"未知配方状态：{status}")
    row = conn.execute(
        "SELECT status FROM recipe_versions WHERE recipe_id = ? AND version = ?",
        (recipe_id, version),
    ).fetchone()
    if row is None:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"配方版本不存在：{recipe_id}@{version}")
    conn.execute(
        "UPDATE recipe_versions SET status = ? WHERE recipe_id = ? AND version = ?",
        (status, recipe_id, version),
    )
    pointer = conn.execute(
        "SELECT published_version FROM recipes WHERE id = ?", (recipe_id,)
    ).fetchone()
    if status in ("revoked", "deprecated") and pointer and pointer["published_version"] == version:
        rollback_to(conn, recipe_id, actor=actor)
    audit(conn, actor, "recipe.status", f"{recipe_id}@{version}", {"status": status})
    return {"recipe_id": recipe_id, "version": version, "status": status}


def rollback_to(conn, recipe_id: str, version: Optional[int] = None, actor: str = "system") -> dict:
    """回滚发布指针。`version=None` 表示回到最近一个仍可发布的版本。"""
    if version is None:
        row = conn.execute(
            """
            SELECT version FROM recipe_versions
             WHERE recipe_id = ? AND status = 'published'
             ORDER BY version DESC LIMIT 1
            """,
            (recipe_id,),
        ).fetchone()
        if row is None:
            conn.execute("UPDATE recipes SET published_version = NULL WHERE id = ?", (recipe_id,))
            audit(conn, actor, "recipe.rollback", recipe_id, {"version": None})
            return {"recipe_id": recipe_id, "published_version": None}
        version = int(row["version"])
    else:
        row = conn.execute(
            "SELECT status FROM recipe_versions WHERE recipe_id = ? AND version = ?",
            (recipe_id, version),
        ).fetchone()
        if row is None:
            raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"配方版本不存在：{recipe_id}@{version}")
        if row["status"] not in ("published",):
            raise WorkError(
                ErrorClass.PARAM_OUT_OF_RANGE, f"版本 {version} 状态为 {row['status']}，不能作为发布指针"
            )
    conn.execute("UPDATE recipes SET published_version = ? WHERE id = ?", (version, recipe_id))
    audit(conn, actor, "recipe.rollback", recipe_id, {"version": version})
    return {"recipe_id": recipe_id, "published_version": version}


def resolve_recipe(
    conn,
    recipe_id: str,
    version: Optional[int] = None,
    rollout_bucket: Optional[int] = None,
) -> dict:
    """解析配方：不给版本就用发布指针，并按灰度比例判断是否放行。"""
    if version is None:
        row = conn.execute(
            "SELECT published_version FROM recipes WHERE id = ?", (recipe_id,)
        ).fetchone()
        if row is None or row["published_version"] is None:
            raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"配方 {recipe_id} 没有已发布版本")
        version = int(row["published_version"])
    record = conn.execute(
        """
        SELECT version, status, content_json, content_sha256, required_capabilities,
               rollout_percent, min_worker_version, reviewed_by, published_at
          FROM recipe_versions WHERE recipe_id = ? AND version = ?
        """,
        (recipe_id, version),
    ).fetchone()
    if record is None:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"配方版本不存在：{recipe_id}@{version}")
    status = record["status"]
    if status in ("revoked", "deprecated"):
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"配方版本 {recipe_id}@{version} 已{status}")
    rollout = int(record["rollout_percent"])
    if rollout < 100 and rollout_bucket is not None:
        if int(rollout_bucket) % 100 >= rollout:
            raise WorkError(
                ErrorClass.PARAM_OUT_OF_RANGE,
                f"配方版本 {recipe_id}@{version} 灰度 {rollout}%，本次未命中",
            )
    return {
        "recipe_id": recipe_id,
        "version": int(record["version"]),
        "status": status,
        "content": json_load(record["content_json"], {}),
        "content_sha256": record["content_sha256"],
        "required_capabilities": json_load(record["required_capabilities"], []) or [],
        "rollout_percent": rollout,
        "min_worker_version": record["min_worker_version"],
        "reviewed_by": record["reviewed_by"],
        "published_at": record["published_at"],
    }


def list_recipe_versions(conn, recipe_id: Optional[str] = None) -> List[dict]:
    if recipe_id:
        rows = conn.execute(
            """
            SELECT rv.recipe_id, rv.version, rv.status, rv.content_sha256, rv.rollout_percent,
                   rv.min_worker_version, rv.published_at, r.name, r.purpose, r.published_version
              FROM recipe_versions rv JOIN recipes r ON r.id = rv.recipe_id
             WHERE rv.recipe_id = ? ORDER BY rv.version DESC
            """,
            (recipe_id,),
        ).fetchall()
    else:
        rows = conn.execute(
            """
            SELECT rv.recipe_id, rv.version, rv.status, rv.content_sha256, rv.rollout_percent,
                   rv.min_worker_version, rv.published_at, r.name, r.purpose, r.published_version
              FROM recipe_versions rv JOIN recipes r ON r.id = rv.recipe_id
             ORDER BY rv.recipe_id, rv.version DESC
            """
        ).fetchall()
    return [dict(row) for row in rows]


# ---------- 用户参数覆盖 ----------


def apply_user_params(payload: dict, user_params: Optional[dict]) -> dict:
    """把本次任务的参数覆盖套用到配方上。

    只允许按步骤 id 覆盖**已有**参数；结果还要过一遍 `validate_recipe`，
    所以越界取值与未知字段一样会被拒绝。
    """
    if not user_params:
        return payload
    if not isinstance(user_params, dict):
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "user_params 必须是对象")
    result = copy.deepcopy(payload)
    steps = {step.get("id"): step for step in result.get("pipeline") or []}
    for step_id, overrides in user_params.items():
        step = steps.get(step_id)
        if step is None:
            raise WorkError(
                ErrorClass.PARAM_OUT_OF_RANGE,
                f"user_params 引用了不存在的步骤 id：{step_id}",
            )
        if not isinstance(overrides, dict):
            raise WorkError(
                ErrorClass.PARAM_OUT_OF_RANGE, f"步骤 {step_id} 的参数覆盖必须是对象"
            )
        merged = dict(step.get("params") or {})
        for key, value in overrides.items():
            if key not in merged:
                raise WorkError(
                    ErrorClass.PARAM_OUT_OF_RANGE,
                    f"步骤 {step_id} 没有参数 {key}；覆盖只能修改已有参数（不新增字段）",
                )
            merged[key] = value
        step["params"] = merged
    return result


# ---------- 任务 ----------


def create_task(
    conn,
    settings: Settings,
    project_id: str,
    asset_ids: Sequence[str],
    recipe_id: str,
    recipe_version: Optional[int] = None,
    user_params: Optional[dict] = None,
    priority: int = 100,
    idempotency_key: Optional[str] = None,
    actor: str = "system",
    billing_quote_id: Optional[str] = None,
    billing_user_id: Optional[str] = None,
) -> dict:
    """创建任务：锁定配方快照 → 构造计划 → 入队（幂等键保证不重复创建）。"""
    if not asset_ids:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "任务至少需要一份输入素材")

    if idempotency_key:
        row = conn.execute(
            "SELECT id FROM tasks WHERE idempotency_key = ?", (idempotency_key,)
        ).fetchone()
        if row is not None:
            if billing_quote_id:
                quote = conn.execute(
                    "SELECT status, task_id, user_id FROM billing_quotes WHERE id = ?",
                    (billing_quote_id,),
                ).fetchone()
                if (quote is None or quote["task_id"] != row["id"]
                        or quote["user_id"] != billing_user_id
                        or quote["status"] not in ("reserved", "settled", "released")):
                    raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE,
                                    "幂等任务已存在，但报价未绑定该任务")
            # 幂等命中：返回既有任务，不新建。
            return get_task(conn, row["id"], include_plan=True)

    resolved = resolve_recipe(conn, recipe_id, recipe_version)
    payload = apply_user_params(resolved["content"], user_params)
    recipe = validate_recipe(payload)

    inputs = load_plan_inputs(conn, asset_ids)
    plan = build_plan(recipe, inputs, cluster_capabilities(settings))

    task_id = new_id("tsk")
    queue = TaskQueue(settings)
    with transaction(conn):
        conn.execute(
            """
            INSERT INTO tasks (id, project_id, state, priority, idempotency_key, billing_quote_id, input_asset_ids,
                               recipe_id, recipe_version, recipe_sha256, recipe_snapshot_json,
                               plan_json, required_capabilities, target_json, user_params_json,
                               progress_json, created_at, updated_at)
            VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
            """,
            (
                task_id,
                project_id,
                str(TaskState.PENDING),
                int(priority),
                idempotency_key,
                billing_quote_id,
                json_dump(list(asset_ids)),
                recipe.recipe_id,
                recipe.version,
                recipe.content_sha256(),
                json_dump(recipe.content_dict()),
                json_dump(plan.to_dict()),
                json_dump(plan.required_capabilities()),
                json_dump(recipe.target.model_dump(mode="json")),
                json_dump(user_params or {}),
                json_dump({"stage": "created", "steps": plan.module_names()}),
                now_iso(),
                now_iso(),
            ),
        )
        if billing_quote_id:
            if not billing_user_id:
                raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "使用报价创建任务需要 billing_user_id")
            from ..billing import _reserve_quote_locked

            _reserve_quote_locked(conn, billing_quote_id, billing_user_id, task_id, actor)
        queue.enqueue(conn, task_id)
        audit(conn, actor, "task.create", task_id, {"recipe_id": recipe.recipe_id,
                                                    "version": recipe.version})
    return get_task(conn, task_id, include_plan=True)


def settle_task_billing(conn, task_id: str, success: bool, actor: str = "system") -> Optional[dict]:
    """按任务终态自动结算已保留报价；无报价的老任务保持兼容。"""
    row = conn.execute(
        "SELECT t.billing_quote_id, t.project_id, p.owner_id FROM tasks t "
        "JOIN projects p ON p.id = t.project_id WHERE t.id = ?", (task_id,)
    ).fetchone()
    if row is None or not row["billing_quote_id"]:
        return None
    from ..billing import settle_quote

    return settle_quote(conn, row["billing_quote_id"], row["owner_id"], task_id, success, actor=actor)


def request_cancel(conn, task_id: str, actor: str = "system") -> dict:
    """请求取消：置取消标记并迁移状态；执行中的任务在步骤边界协作退出。"""
    row = conn.execute("SELECT state FROM tasks WHERE id = ?", (task_id,)).fetchone()
    if row is None:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"任务不存在：{task_id}")
    if row["state"] in {str(s) for s in TERMINAL_STATES}:
        raise WorkError(
            ErrorClass.PARAM_OUT_OF_RANGE, f"任务已处于终态 {row['state']}，无法取消"
        )
    target = (
        TaskState.CANCEL_REQUESTED
        if row["state"] in (str(TaskState.RUNNING), str(TaskState.VERIFYING))
        else TaskState.CANCELLED
    )
    fields = {"cancel_requested": 1}
    if target is TaskState.CANCELLED:
        fields.update({"finished_at": now_iso(), "error_class": str(ErrorClass.CANCELLED),
                       "error_message": "用户取消"})
    transition(conn, task_id, target, fields=fields)
    audit(conn, actor, "task.cancel", task_id, {"to": str(target)})
    # PENDING/QUEUED 任务没有 Worker 会替它走 `_cancel`，因此必须在这里释放
    # 已保留的报价。结算放在状态迁移提交后执行，避免在同一 SQLite 连接里嵌套
    # `BEGIN IMMEDIATE`；`settle_quote` 本身是幂等的。
    if target is TaskState.CANCELLED:
        settle_task_billing(conn, task_id, False, actor=actor)
    return get_task(conn, task_id)


def retry_task(conn, task_id: str, settings: Settings, actor: str = "system") -> dict:
    """对失败任务发起重试：清空重试计数并重新入队。"""
    row = conn.execute("SELECT state FROM tasks WHERE id = ?", (task_id,)).fetchone()
    if row is None:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"任务不存在：{task_id}")
    if row["state"] not in (str(TaskState.FAILED), str(TaskState.CANCELLED)):
        raise WorkError(
            ErrorClass.PARAM_OUT_OF_RANGE,
            f"只有 FAILED/CANCELLED 任务可以手动重试，当前为 {row['state']}",
        )
    queue = TaskQueue(settings)
    with transaction(conn):
        conn.execute(
            "UPDATE tasks SET attempts = 0, error_class = NULL, error_message = NULL, "
            "cancel_requested = 0, result_json = NULL, finished_at = NULL WHERE id = ?",
            (task_id,),
        )
        # FAILED/CANCELLED 都是终态，必须先显式回到 RETRY_WAITING 才能重新入队。
        conn.execute(
            "UPDATE tasks SET state = ? WHERE id = ?", (str(TaskState.RETRY_WAITING), task_id)
        )
        queue.enqueue(conn, task_id)
        audit(conn, actor, "task.retry", task_id, {})
    return get_task(conn, task_id)


def get_task(conn, task_id: str, include_plan: bool = False) -> dict:
    row = conn.execute("SELECT * FROM tasks WHERE id = ?", (task_id,)).fetchone()
    if row is None:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"任务不存在：{task_id}")
    detail = {
        "id": row["id"],
        "project_id": row["project_id"],
        "state": row["state"],
        "priority": row["priority"],
        "attempts": row["attempts"],
        "input_asset_ids": json_load(row["input_asset_ids"], []) or [],
        "recipe_id": row["recipe_id"],
        "recipe_version": row["recipe_version"],
        "recipe_sha256": row["recipe_sha256"],
        "target": json_load(row["target_json"], {}) or {},
        "cancel_requested": bool(row["cancel_requested"]),
        "progress": json_load(row["progress_json"], {}) or {},
        "error": {"class": row["error_class"], "message": row["error_message"]}
        if row["error_class"]
        else None,
        "result": json_load(row["result_json"], None),
        "created_at": row["created_at"],
        "updated_at": row["updated_at"],
        "started_at": row["started_at"],
        "finished_at": row["finished_at"],
        "lease": {"owner": row["lease_owner"], "expires_at": row["lease_expires_at"]},
        "artifacts": list_artifacts(conn, task_id),
        # 人工复核投影（确认流分支B）：没有记录时为 None。延迟导入避免 service ↔ review 循环。
        "review": _review_summary(conn, task_id),
    }
    if include_plan:
        detail["plan"] = json_load(row["plan_json"], {}) or {}
        detail["recipe"] = json_load(row["recipe_snapshot_json"], {}) or {}
    return detail


def _review_summary(conn, task_id: str):
    from . import review

    return review.review_summary(conn, task_id)


def list_tasks(conn, state: Optional[str] = None, limit: int = 50) -> List[dict]:
    if state:
        rows = conn.execute(
            "SELECT id, state, priority, recipe_id, recipe_version, attempts, created_at, updated_at "
            "FROM tasks WHERE state = ? ORDER BY created_at DESC LIMIT ?",
            (state, int(limit)),
        ).fetchall()
    else:
        rows = conn.execute(
            "SELECT id, state, priority, recipe_id, recipe_version, attempts, created_at, updated_at "
            "FROM tasks ORDER BY created_at DESC LIMIT ?",
            (int(limit),),
        ).fetchall()
    return [dict(row) for row in rows]


def attempts_for(conn, task_id: str) -> List[dict]:
    rows = conn.execute(
        "SELECT attempt, worker_id, started_at, finished_at, error_class, error_message "
        "FROM task_attempts WHERE task_id = ? ORDER BY attempt",
        (task_id,),
    ).fetchall()
    return [dict(row) for row in rows]


# ---------- 产物 ----------


def register_artifact(
    conn,
    storage: LocalStorage,
    task_id: str,
    kind: str,
    path,
    media: Optional[dict] = None,
    quality: Optional[dict] = None,
    name: Optional[str] = None,
    retain_days: int = 0,
) -> dict:
    """把产物落到对象存储并登记元信息。质量报告与哈希一起存，便于溯源。"""
    from datetime import datetime, timedelta, timezone
    from pathlib import Path

    artifact_id = new_id("art")
    filename = name or Path(str(path)).name
    key = LocalStorage.artifact_key(task_id, f"{artifact_id}_{filename}")
    stat = storage.put_file(key, Path(str(path)))
    retain_until = None
    if retain_days > 0:
        retain_until = (
            datetime.now(timezone.utc) + timedelta(days=retain_days)
        ).replace(microsecond=0).isoformat()
    conn.execute(
        """
        INSERT INTO artifacts (id, task_id, kind, storage_key, sha256, size_bytes,
                               media_json, quality_json, retain_until, created_at)
        VALUES (?,?,?,?,?,?,?,?,?,?)
        """,
        (
            artifact_id,
            task_id,
            kind,
            key,
            stat.sha256,
            stat.size_bytes,
            json_dump(media) if media is not None else None,
            json_dump(quality) if quality is not None else None,
            retain_until,
            now_iso(),
        ),
    )
    return {
        "id": artifact_id,
        "task_id": task_id,
        "kind": kind,
        "storage_key": key,
        "sha256": stat.sha256,
        "size_bytes": stat.size_bytes,
        "media": media or {},
        "quality": quality or {},
        "retain_until": retain_until,
    }


def list_artifacts(conn, task_id: str) -> List[dict]:
    rows = conn.execute(
        "SELECT id, kind, storage_key, sha256, size_bytes, media_json, quality_json, "
        "retain_until, created_at FROM artifacts WHERE task_id = ? ORDER BY created_at",
        (task_id,),
    ).fetchall()
    return [
        {
            "id": row["id"],
            "kind": row["kind"],
            "storage_key": row["storage_key"],
            "sha256": row["sha256"],
            "size_bytes": row["size_bytes"],
            "media": json_load(row["media_json"], {}) or {},
            "quality": json_load(row["quality_json"], {}) or {},
            "retain_until": row["retain_until"],
            "created_at": row["created_at"],
        }
        for row in rows
    ]


def get_artifact(conn, artifact_id: str) -> dict:
    row = conn.execute("SELECT * FROM artifacts WHERE id = ?", (artifact_id,)).fetchone()
    if row is None:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"产物不存在：{artifact_id}")
    return {
        "id": row["id"],
        "task_id": row["task_id"],
        "kind": row["kind"],
        "storage_key": row["storage_key"],
        "sha256": row["sha256"],
        "size_bytes": row["size_bytes"],
        "media": json_load(row["media_json"], {}) or {},
        "quality": json_load(row["quality_json"], {}) or {},
        "retain_until": row["retain_until"],
        "created_at": row["created_at"],
    }


def cleanup_expired_artifacts(conn, storage: LocalStorage) -> int:
    """按保留期限清理产物（§7.3「按生命周期策略清理」）。"""
    rows = conn.execute(
        "SELECT id, storage_key FROM artifacts WHERE retain_until IS NOT NULL AND retain_until < ?",
        (now_iso(),),
    ).fetchall()
    for row in rows:
        storage.delete(row["storage_key"])
        conn.execute("DELETE FROM artifacts WHERE id = ?", (row["id"],))
    return len(rows)


def get_checkpoints(conn, task_id: str) -> List[dict]:
    rows = conn.execute(
        "SELECT stage, input_sha256, recipe_version, engine_version, params_digest, artifact_key, "
        "artifact_sha256, completed, updated_at FROM task_checkpoints WHERE task_id = ? "
        "ORDER BY updated_at",
        (task_id,),
    ).fetchall()
    return [dict(row) for row in rows]


#: 按扩展名推断素材角色（未显式指定时使用）。
_ROLE_BY_SUFFIX = {
    ".srt": "subtitle", ".ass": "subtitle", ".ssa": "subtitle", ".vtt": "subtitle",
    ".mp3": "audio", ".wav": "audio", ".m4a": "audio", ".aac": "audio",
    ".json": "metadata", ".txt": "metadata",
}


def role_for_path(path) -> str:
    from pathlib import Path

    return _ROLE_BY_SUFFIX.get(Path(str(path)).suffix.lower(), "video")


def register_local_asset(
    conn,
    settings: Settings,
    storage: LocalStorage,
    project_id: str,
    path,
    name: Optional[str] = None,
    role: Optional[str] = None,
    actor: str = "cli",
) -> dict:
    """登记本地文件为素材，并顺带探测一次（探测失败不算登记失败）。"""
    from pathlib import Path

    from ..modules.ffmpeg import probe_json, summarize

    source = Path(str(path))
    if not source.exists():
        raise WorkError(ErrorClass.INPUT_CORRUPT, f"文件不存在：{source}")
    asset = register_asset(
        conn,
        storage,
        project_id,
        name or source.stem,
        source,
        role=role or role_for_path(source),
        filename=source.name,
        actor=actor,
    )
    try:
        summary = summarize(probe_json(source, settings))
        set_asset_probe(conn, asset["id"], summary)
        asset["probe"] = summary
    except WorkError as error:
        asset["probe_error"] = error.message
    return asset


def validate_recipe_safe(payload: dict):
    """校验配方，失败时返回 None（供演示脚本这类「尽力而为」的调用点使用）。"""
    try:
        return validate_recipe(payload)
    except WorkError:
        return None
