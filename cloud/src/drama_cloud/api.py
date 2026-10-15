"""FastAPI 控制面（设计文档 §8.3 的 8 个端点 + 学习原型需要的辅助端点）。

覆盖文档点名的接口：

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| POST | /api/v1/uploads | 申请上传会话（返回短时签名令牌） |
| PUT  | /api/v1/uploads/{upload_id} | 用令牌直传文件字节（不经过 JSON 代理） |
| POST | /api/v1/tasks | 创建任务（支持 Idempotency-Key） |
| GET  | /api/v1/tasks/{task_id} | 查询状态与进度 |
| POST | /api/v1/tasks/{task_id}/cancel | 请求取消 |
| POST | /api/v1/tasks/{task_id}/retry | 手动重试 |
| GET  | /api/v1/recipes/resolve | 取发布指针指向的配方 |
| GET  | /api/v1/recipes/{id}/versions/{v} | 取指定版本配方 |
| GET  | /api/v1/artifacts/{id} | 产物元信息与下载授权 |

安全边界（§10）：Bearer 令牌认证、角色授权（viewer < operator < admin）、按主体限流、
配方只能通过白名单校验后发布、下载必须带短时签名令牌。
"""

from __future__ import annotations

import sys
import threading
import time
from pathlib import Path
from typing import Dict, Iterable, Literal, Optional, Tuple

from fastapi import Depends, FastAPI, Header, HTTPException, Query, Request
from fastapi.responses import FileResponse, JSONResponse
from pydantic import Field

from . import __version__
from .auth import ROLE_RANK, Principal, resolve_token
from . import billing
from .base import StrictModel
from .config import Settings, load_settings
from .db import connect, init_db, json_load, now_iso
from .errors import ErrorClass, WorkError
from .plan import ENGINE_VERSION, WORKER_VERSION
from .recipes.registry import registered_modules
from .recipes.schema import recipe_json_schema
from .runtime import RuntimeManager, RuntimeManifest, local_target
from .storage import LocalStorage
from .tasks import review as review_service
from .tasks import service
from .tasks.queue import TaskQueue

#: 错误分类 → HTTP 状态码。让客户端能区分「我传错了」和「系统暂时不行」。
_STATUS_BY_CLASS = {
    ErrorClass.PARAM_OUT_OF_RANGE: 400,
    ErrorClass.RECIPE_SCHEMA_INVALID: 422,
    ErrorClass.INPUT_CORRUPT: 422,
    ErrorClass.TARGET_UNSUPPORTED: 422,
    ErrorClass.QUALITY_GATE_FAILED: 422,
    ErrorClass.QUALITY_RETRYABLE: 409,
    ErrorClass.CAPABILITY_MISSING: 503,
    ErrorClass.TRANSIENT_IO: 503,
    ErrorClass.STORAGE_UNAVAILABLE: 503,
    ErrorClass.RESOURCE_CONTENTION: 409,
    ErrorClass.WORKER_LOST: 409,
    ErrorClass.CANCELLED: 409,
    ErrorClass.INTERNAL: 500,
}

WEB_DIR = Path(__file__).resolve().parent / "web"


# ---------- 请求 / 响应模型 ----------


class UploadRequest(StrictModel):
    name: str
    role: str = "video"
    filename: str
    project_id: Optional[str] = None
    ttl_seconds: int = Field(900, ge=60, le=86400)


class TaskRequest(StrictModel):
    asset_ids: list
    recipe_id: str
    recipe_version: Optional[int] = None
    user_params: Optional[dict] = None
    priority: int = Field(100, ge=0, le=10000)
    project_id: Optional[str] = None
    idempotency_key: Optional[str] = None
    billing_quote_id: Optional[str] = None


class ReviewDecisionRequest(StrictModel):
    """人工复核裁决：`approve` 放行 / `reject` 判定不可用（通常随后重跑）。"""

    decision: Literal["approve", "reject"]
    note: str = ""


class ReviewRerunRequest(StrictModel):
    """按修正后的参数重跑：`user_params` 形态与创建任务一致（按步骤 id 覆盖已有参数）。"""

    user_params: Optional[dict] = None
    note: str = ""
    priority: int = Field(100, ge=0, le=10000)


class RecipePublishRequest(StrictModel):
    recipe: dict
    status: str = "published"
    rollout_percent: int = Field(100, ge=0, le=100)
    reviewed_by: Optional[str] = None


class RuntimeInstallRequest(StrictModel):
    """只允许选择本机 manifest 中声明的角色；客户端不能提交任意 URL 或哈希。"""

    role: str


class BillingQuoteRequest(StrictModel):
    units: int = Field(..., ge=1, le=1_000_000)
    unit_cost: int = Field(1, ge=1, le=1_000_000)
    ttl_seconds: int = Field(900, ge=60, le=86400)
    idempotency_key: Optional[str] = None


class BillingSettleRequest(StrictModel):
    task_id: str


# ---------- 限流 ----------


class RateLimiter:
    """按主体的令牌桶限流（§10「管理接口实施……速率限制」）。

    单进程内存实现，够学习原型用；多实例部署时应换成 Redis 计数器。
    """

    def __init__(self, capacity: int = 120, refill_per_second: float = 4.0):
        self.capacity = float(capacity)
        self.refill = float(refill_per_second)
        self._buckets: Dict[str, Tuple[float, float]] = {}
        self._lock = threading.Lock()

    def check(self, key: str) -> None:
        now = time.monotonic()
        with self._lock:
            tokens, last = self._buckets.get(key, (self.capacity, now))
            tokens = min(self.capacity, tokens + (now - last) * self.refill)
            if tokens < 1.0:
                self._buckets[key] = (tokens, now)
                raise HTTPException(
                    status_code=429,
                    detail={"error": "rate_limited", "message": "请求过于频繁，请稍后重试"},
                    headers={"Retry-After": "2"},
                )
            self._buckets[key] = (tokens - 1.0, now)


# ---------- 应用工厂 ----------


def create_app(settings: Optional[Settings] = None) -> FastAPI:
    from .recipes.registry import ensure_modules_loaded

    ensure_modules_loaded()
    settings = settings or load_settings()
    settings.ensure_dirs()
    init_db(settings)
    limiter = RateLimiter()
    storage = LocalStorage(settings)

    app = FastAPI(
        title="云端短剧视频制作与多平台适配系统（学习原型）",
        version=__version__,
        description="控制面 API。仅用于技术学习与个人自有素材。",
    )
    app.state.settings = settings
    app.state.limiter = limiter
    app.state.storage = storage

    def runtime_manifest() -> RuntimeManifest:
        if settings.runtime_manifest is None:
            raise HTTPException(503, detail={"error": "runtime_manifest_missing",
                                             "message": "尚未配置 DRAMA_CLOUD_RUNTIME_MANIFEST"})
        try:
            return RuntimeManifest.load(Path(settings.runtime_manifest))
        except (OSError, ValueError) as error:
            raise HTTPException(503, detail={"error": "runtime_manifest_invalid",
                                             "message": str(error)}) from error

    def get_conn():
        conn = connect(settings)
        try:
            yield conn
        finally:
            conn.close()

    def principal_from_header(authorization: Optional[str]) -> Principal:
        """只认 `Authorization: Bearer <用户令牌>`。"""
        if not authorization or not authorization.lower().startswith("bearer "):
            raise HTTPException(
                401, detail={"error": "unauthorized", "message": "需要 Authorization: Bearer 令牌"}
            )
        conn = connect(settings)
        try:
            principal = resolve_token(conn, authorization[7:].strip())
        finally:
            conn.close()
        if principal is None:
            raise HTTPException(401, detail={"error": "unauthorized", "message": "令牌无效"})
        return principal

    def current_principal(request: Request, authorization: Optional[str] = Header(None)) -> Principal:
        """用户令牌只认 `Authorization: Bearer` 请求头，不接受 `?token=`。

        网址会被写进访问日志和浏览器历史，长期有效的用户令牌放进网址就等于泄露。
        上传、下载用的短时专用令牌另有校验路径，不受影响。
        """
        principal = principal_from_header(authorization)
        limiter.check(f"{principal.user_id}:{request.url.path}")
        return principal

    def require_role(minimum: str):
        def dependency(principal: Principal = Depends(current_principal)) -> Principal:
            if ROLE_RANK.get(principal.role, 0) < ROLE_RANK[minimum]:
                raise HTTPException(
                    403,
                    detail={
                        "error": "forbidden",
                        "message": f"需要 {minimum} 及以上权限，当前角色 {principal.role}",
                    },
                )
            return principal

        return dependency

    def project_for(conn, principal: Principal, project_id: Optional[str]) -> str:
        if project_id:
            row = conn.execute("SELECT id FROM projects WHERE id = ?", (project_id,)).fetchone()
            if row is None:
                raise HTTPException(404, detail={"error": "not_found", "message": "项目不存在"})
            return row["id"]
        return service.ensure_default_project(conn, principal.user_id)

    @app.exception_handler(WorkError)
    async def _work_error_handler(request: Request, error: WorkError):
        status = _STATUS_BY_CLASS.get(error.error_class, 500)
        return JSONResponse(
            status_code=status,
            content={
                "error": str(error.error_class),
                "retryable": error.retryable,
                "message": error.message,
            },
        )

    # ---------- 基础 ----------

    @app.get("/api/v1/health")
    def health():
        return {"status": "ok", "version": __version__, "time": now_iso()}

    @app.get("/api/v1/auth/whoami")
    def whoami(principal: Principal = Depends(current_principal)):
        return {"user_id": principal.user_id, "name": principal.name, "role": principal.role}

    @app.get("/api/v1/system/status")
    def system_status(principal: Principal = Depends(require_role("viewer")), conn=Depends(get_conn)):
        from .modules.ffmpeg import ffmpeg_version

        queue = TaskQueue(settings)
        # 状态页本身不能因为可选的外部媒体工具缺失而不可用；把缺失记录成
        # 可读的状态值，任务创建/执行时仍会按能力门禁返回明确错误。
        try:
            ffmpeg = ffmpeg_version(settings)
        except WorkError as error:
            ffmpeg = f"unavailable: {error.message}"
        return {
            "worker_version": WORKER_VERSION,
            "engine_version": ENGINE_VERSION,
            "ffmpeg": ffmpeg,
            "capabilities": service.cluster_capabilities(settings),
            "modules": sorted(registered_modules()),
            "queue_depth": queue.depth(conn),
            "tasks_by_state": queue.stats(conn),
            "reviews_pending": len(review_service.list_reviews(conn, status="pending", limit=1000)),
            "storage": {
                "root": str(settings.storage_dir),
                "objects_bytes": _dir_size(settings.storage_dir),
            },
            "runtimes": RuntimeManager(settings.runtime_root or (settings.home / "runtime")).status(),
            "home": str(settings.home),
        }

    @app.get("/api/v1/runtimes")
    def runtime_status_endpoint(principal: Principal = Depends(require_role("viewer"))):
        """查询本地 RuntimeRoles 状态与当前设备适配角色。"""
        manifest = runtime_manifest()
        target = local_target()
        return {
            "target": {"platform": target[0], "arch": target[1]},
            "available_roles": [role.name for role in manifest.for_target(*target)],
            **RuntimeManager(settings.runtime_root or (settings.home / "runtime")).status(),
        }

    @app.post("/api/v1/runtimes/install")
    def runtime_install_endpoint(
        payload: RuntimeInstallRequest,
        principal: Principal = Depends(require_role("admin")),
    ):
        """按可信 manifest 安装单个角色；不会接受命令、脚本或客户端 URL。"""
        manifest = runtime_manifest()
        target = local_target()
        role = next((item for item in manifest.for_target(*target) if item.name == payload.role), None)
        if role is None:
            raise HTTPException(400, detail={"error": "runtime_role_unavailable",
                                             "message": "manifest 中没有适配本机的该角色"})
        try:
            return RuntimeManager(settings.runtime_root or (settings.home / "runtime")).install(role)
        except (OSError, ValueError) as error:
            raise HTTPException(400, detail={"error": "runtime_install_failed",
                                             "message": str(error)}) from error

    @app.post("/api/v1/runtimes/{role_name}/rollback")
    def runtime_rollback_endpoint(
        role_name: str,
        principal: Principal = Depends(require_role("admin")),
    ):
        try:
            return RuntimeManager(settings.runtime_root or (settings.home / "runtime")).rollback(role_name)
        except FileNotFoundError as error:
            raise HTTPException(404, detail={"error": "runtime_rollback_missing",
                                             "message": str(error)}) from error
        except (OSError, ValueError) as error:
            raise HTTPException(400, detail={"error": "runtime_rollback_failed",
                                             "message": str(error)}) from error

    # ---------- 本地积分账本（不连接支付网关） ----------

    @app.get("/api/v1/billing/balance")
    def billing_balance_endpoint(
        principal: Principal = Depends(require_role("operator")), conn=Depends(get_conn)
    ):
        return billing.get_balance(conn, principal.user_id)

    @app.post("/api/v1/billing/quotes", status_code=201)
    def billing_quote_endpoint(
        payload: BillingQuoteRequest,
        principal: Principal = Depends(require_role("operator")), conn=Depends(get_conn),
    ):
        return billing.create_quote(conn, principal.user_id, payload.units, payload.unit_cost,
                                    payload.ttl_seconds, payload.idempotency_key,
                                    actor=principal.user_id)

    @app.get("/api/v1/billing/quotes/{quote_id}")
    def billing_get_quote_endpoint(
        quote_id: str,
        principal: Principal = Depends(require_role("operator")), conn=Depends(get_conn),
    ):
        quote = billing.get_quote(conn, quote_id)
        if quote["user_id"] != principal.user_id and principal.role != "admin":
            raise HTTPException(403, detail={"error": "forbidden", "message": "报价不属于当前用户"})
        return quote

    @app.post("/api/v1/billing/quotes/{quote_id}/settle")
    def billing_settle_endpoint(
        quote_id: str,
        payload: BillingSettleRequest,
        principal: Principal = Depends(require_role("operator")), conn=Depends(get_conn),
    ):
        quote = billing.get_quote(conn, quote_id)
        if quote["user_id"] != principal.user_id:
            raise HTTPException(403, detail={"error": "forbidden", "message": "报价不属于当前用户"})
        task = conn.execute("SELECT state, project_id FROM tasks WHERE id = ?", (payload.task_id,)).fetchone()
        if task is None:
            raise HTTPException(404, detail={"error": "not_found", "message": "任务不存在"})
        project = conn.execute("SELECT owner_id FROM projects WHERE id = ?", (task["project_id"],)).fetchone()
        if project is None or project["owner_id"] != principal.user_id:
            raise HTTPException(403, detail={"error": "forbidden", "message": "任务不属于当前用户"})
        if task["state"] == "SUCCEEDED":
            succeeded = True
        elif task["state"] in ("FAILED", "CANCELLED"):
            succeeded = False
        else:
            raise HTTPException(409, detail={"error": "task_not_terminal",
                                             "message": "任务尚未结束，暂不能结算"})
        return billing.settle_quote(conn, quote_id, principal.user_id, payload.task_id,
                                    succeeded, actor=principal.user_id)

    # ---------- 上传 ----------

    @app.post("/api/v1/uploads", status_code=201)
    def create_upload(
        payload: UploadRequest,
        principal: Principal = Depends(require_role("operator")),
        conn=Depends(get_conn),
    ):
        """申请上传会话：返回短时签名令牌与直传地址。"""
        if payload.role not in service.ASSET_ROLES:
            raise HTTPException(400, detail={"error": "bad_role", "message": f"未知角色 {payload.role}"})
        upload_id = service.new_id("up")
        token = storage.sign_payload(
            {
                "scope": "upload",
                "upload_id": upload_id,
                "project_id": project_for(conn, principal, payload.project_id),
                "name": payload.name,
                "role": payload.role,
                "filename": payload.filename,
                "owner": principal.user_id,
            },
            ttl_seconds=payload.ttl_seconds,
        )
        return {
            "upload_id": upload_id,
            "method": "PUT",
            "upload_url": f"/api/v1/uploads/{upload_id}",
            "token": token,
            "expires_in": payload.ttl_seconds,
        }

    @app.put("/api/v1/uploads/{upload_id}")
    async def upload_bytes(
        upload_id: str,
        request: Request,
        x_upload_token: Optional[str] = Header(None),
        token: Optional[str] = Query(None),
    ):
        """直传文件字节。

        这里**只**认上传会话令牌，不要求 Bearer：这正是「预签名 URL」的语义——授权在
        `POST /api/v1/uploads` 那一刻已经完成，令牌本身自证身份与用途、并且很快过期。
        请求体走 `request.stream()` 落盘，不经过 JSON 编解码，也不会把大文件读进内存。
        """
        raw_token = x_upload_token or token
        if not raw_token:
            raise HTTPException(401, detail={"error": "unauthorized", "message": "缺少上传令牌"})
        payload = storage.verify_payload(raw_token, scope="upload")
        if payload.get("upload_id") != upload_id:
            raise HTTPException(403, detail={"error": "forbidden", "message": "上传令牌与会话不匹配"})

        staging = settings.work_dir / f"upload_{upload_id}"
        staging.parent.mkdir(parents=True, exist_ok=True)
        digest_size = 0
        with staging.open("wb") as handle:
            async for block in request.stream():
                if block:
                    digest_size += len(block)
                    handle.write(block)
        if digest_size == 0:
            staging.unlink(missing_ok=True)
            raise HTTPException(400, detail={"error": "empty_body", "message": "上传内容为空"})

        conn = connect(settings)
        try:
            asset = service.register_asset(
                conn,
                storage,
                payload["project_id"],
                payload["name"],
                staging,
                role=payload["role"],
                filename=payload["filename"],
                actor=str(payload.get("owner") or "upload-session"),
            )
            from .modules.ffmpeg import probe_json, summarize

            try:
                asset["probe"] = summarize(probe_json(staging, settings))
                service.set_asset_probe(conn, asset["id"], asset["probe"])
            except WorkError as error:
                # 探测失败不阻断上传：素材仍登记，任务执行时会以 input_corrupt 明确失败。
                asset["probe_error"] = error.message
        finally:
            conn.close()
            staging.unlink(missing_ok=True)
        return {"asset": asset}

    @app.get("/api/v1/assets")
    def list_assets(
        principal: Principal = Depends(require_role("viewer")), conn=Depends(get_conn)
    ):
        rows = conn.execute(
            "SELECT id, project_id, name, sha256, size_bytes, source_declaration, created_at "
            "FROM assets WHERE lifecycle = 'ready' ORDER BY created_at DESC LIMIT 200"
        ).fetchall()
        return {
            "assets": [
                {
                    **dict(row),
                    "declaration": json_load(row["source_declaration"], {}) or {},
                }
                for row in rows
            ]
        }

    # ---------- 任务 ----------

    @app.post("/api/v1/tasks", status_code=201)
    def create_task_endpoint(
        payload: TaskRequest,
        principal: Principal = Depends(require_role("operator")),
        conn=Depends(get_conn),
        idempotency_header: Optional[str] = Header(None, alias="Idempotency-Key"),
    ):
        project_id = project_for(conn, principal, payload.project_id)
        idempotency_key = idempotency_header or payload.idempotency_key
        return service.create_task(
            conn,
            settings,
            project_id=project_id,
            asset_ids=payload.asset_ids,
            recipe_id=payload.recipe_id,
            recipe_version=payload.recipe_version,
            user_params=payload.user_params,
            priority=payload.priority,
            idempotency_key=idempotency_key,
            actor=principal.user_id,
            billing_quote_id=payload.billing_quote_id,
            billing_user_id=principal.user_id if payload.billing_quote_id else None,
        )

    @app.get("/api/v1/tasks")
    def list_tasks_endpoint(
        state: Optional[str] = None,
        limit: int = Query(50, ge=1, le=500),
        principal: Principal = Depends(require_role("viewer")),
        conn=Depends(get_conn),
    ):
        return {"tasks": service.list_tasks(conn, state=state, limit=limit)}

    @app.get("/api/v1/tasks/{task_id}")
    def get_task_endpoint(
        task_id: str,
        include_plan: bool = False,
        principal: Principal = Depends(require_role("viewer")),
        conn=Depends(get_conn),
    ):
        detail = service.get_task(conn, task_id, include_plan=include_plan)
        detail["attempts_detail"] = service.attempts_for(conn, task_id)
        detail["checkpoints"] = service.get_checkpoints(conn, task_id)
        return detail

    @app.post("/api/v1/tasks/{task_id}/cancel")
    def cancel_task_endpoint(
        task_id: str,
        principal: Principal = Depends(require_role("operator")),
        conn=Depends(get_conn),
    ):
        return service.request_cancel(conn, task_id, actor=principal.user_id)

    @app.post("/api/v1/tasks/{task_id}/retry")
    def retry_task_endpoint(
        task_id: str,
        principal: Principal = Depends(require_role("operator")),
        conn=Depends(get_conn),
    ):
        return service.retry_task(conn, task_id, settings, actor=principal.user_id)

    # ---------- 配方 ----------

    @app.get("/api/v1/recipes/resolve")
    def resolve_recipe_endpoint(
        recipe_id: str,
        version: Optional[int] = None,
        rollout_bucket: Optional[int] = None,
        principal: Principal = Depends(require_role("viewer")),
        conn=Depends(get_conn),
    ):
        return service.resolve_recipe(conn, recipe_id, version, rollout_bucket)

    @app.get("/api/v1/recipes/{recipe_id}/versions/{version}")
    def get_recipe_version_endpoint(
        recipe_id: str,
        version: int,
        principal: Principal = Depends(require_role("viewer")),
        conn=Depends(get_conn),
    ):
        return service.resolve_recipe(conn, recipe_id, version)

    @app.get("/api/v1/recipes")
    def list_recipes_endpoint(
        recipe_id: Optional[str] = None,
        principal: Principal = Depends(require_role("viewer")),
        conn=Depends(get_conn),
    ):
        return {"recipes": service.list_recipe_versions(conn, recipe_id)}

    @app.get("/api/v1/recipes/schema.json")
    def recipe_schema_endpoint(principal: Principal = Depends(require_role("viewer"))):
        return recipe_json_schema()

    @app.post("/api/v1/recipes", status_code=201)
    def publish_recipe_endpoint(
        payload: RecipePublishRequest,
        principal: Principal = Depends(require_role("admin")),
        conn=Depends(get_conn),
    ):
        """发布新版本。已发布版本不可修改（§6.1）。"""
        return service.publish_recipe(
            conn,
            payload.recipe,
            actor=principal.user_id,
            status=payload.status,
            rollout_percent=payload.rollout_percent,
            reviewed_by=payload.reviewed_by,
        )

    @app.post("/api/v1/recipes/{recipe_id}/versions/{version}/status")
    def set_recipe_status_endpoint(
        recipe_id: str,
        version: int,
        status: str = Query(...),
        principal: Principal = Depends(require_role("admin")),
        conn=Depends(get_conn),
    ):
        return service.set_recipe_status(conn, recipe_id, version, status, actor=principal.user_id)

    # ---------- 产物 ----------

    @app.get("/api/v1/artifacts/{artifact_id}")
    def get_artifact_endpoint(
        artifact_id: str,
        principal: Principal = Depends(require_role("viewer")),
        conn=Depends(get_conn),
    ):
        artifact = service.get_artifact(conn, artifact_id)
        token = storage.sign_payload(
            {"scope": "artifact", "artifact_id": artifact_id}, ttl_seconds=900
        )
        artifact["download_url"] = f"/api/v1/artifacts/{artifact_id}/download?token={token}"
        artifact["download_expires_in"] = 900
        return artifact

    @app.get("/api/v1/artifacts/{artifact_id}/download")
    def download_artifact_endpoint(
        artifact_id: str,
        request: Request,
        authorization: Optional[str] = Header(None),
        token: Optional[str] = Query(None),
        conn=Depends(get_conn),
    ):
        """两种访问方式二选一：

        1. `?token=<短时签名令牌>`：给浏览器直接打开的下载链接用（对应文档的「短时授权」）；
        2. `Authorization: Bearer <用户令牌>`：给脚本/CLI 用。

        注意下载令牌**不是**用户令牌，所以这里不能走 `current_principal`——那样会把
        「对象级短时授权」和「用户身份认证」两件事混在一起。
        """
        if token:
            payload = storage.verify_payload(token, scope="artifact")
            if payload.get("artifact_id") != artifact_id:
                raise HTTPException(403, detail={"error": "forbidden", "message": "下载令牌与产物不匹配"})
        else:
            principal = principal_from_header(authorization)
            limiter.check(f"{principal.user_id}:{request.url.path}")
        artifact = service.get_artifact(conn, artifact_id)
        path = storage.local_path(artifact["storage_key"])
        if not path.exists():
            raise HTTPException(410, detail={"error": "gone", "message": "产物已被清理"})
        return FileResponse(
            path,
            filename=Path(artifact["storage_key"]).name,
            media_type="application/octet-stream",
            headers={"X-Artifact-SHA256": artifact["sha256"]},
        )

    # ---------- 人工复核队列（确认流分支B） ----------

    @app.get("/api/v1/reviews")
    def list_reviews_endpoint(
        status: Optional[str] = Query(None),
        task_id: Optional[str] = Query(None),
        limit: int = Query(100, ge=1, le=500),
        principal: Principal = Depends(require_role("viewer")),
        conn=Depends(get_conn),
    ):
        return {"reviews": review_service.list_reviews(
            conn, status=status, task_id=task_id, limit=limit
        )}

    @app.get("/api/v1/reviews/{review_id}")
    def get_review_endpoint(
        review_id: str,
        principal: Principal = Depends(require_role("viewer")),
        conn=Depends(get_conn),
    ):
        return review_service.get_review(conn, review_id)

    @app.post("/api/v1/reviews/{review_id}/claim")
    def claim_review_endpoint(
        review_id: str,
        principal: Principal = Depends(require_role("operator")),
        conn=Depends(get_conn),
    ):
        return review_service.claim(conn, review_id, actor=principal.user_id)

    @app.post("/api/v1/reviews/{review_id}/decision")
    def decide_review_endpoint(
        review_id: str,
        payload: ReviewDecisionRequest,
        principal: Principal = Depends(require_role("operator")),
        conn=Depends(get_conn),
    ):
        """标记通过 / 标记失败。只改复核记录与审计，不改任务状态。"""
        return review_service.decide(
            conn, review_id, payload.decision, note=payload.note, actor=principal.user_id
        )

    @app.post("/api/v1/reviews/{review_id}/rerun", status_code=201)
    def rerun_review_endpoint(
        review_id: str,
        payload: ReviewRerunRequest,
        principal: Principal = Depends(require_role("operator")),
        conn=Depends(get_conn),
    ):
        """修正 mask / 参数后重跑：创建一个新任务，本复核记录转 superseded。"""
        return review_service.rerun(
            conn,
            settings,
            review_id,
            user_params=payload.user_params,
            note=payload.note,
            actor=principal.user_id,
            priority=payload.priority,
        )

    # ---------- 审计 ----------

    @app.get("/api/v1/audit")
    def list_audit_endpoint(
        limit: int = Query(100, ge=1, le=1000),
        principal: Principal = Depends(require_role("admin")),
        conn=Depends(get_conn),
    ):
        rows = conn.execute(
            "SELECT id, ts, actor, action, subject, detail_json FROM audit_events "
            "ORDER BY id DESC LIMIT ?",
            (limit,),
        ).fetchall()
        return {"events": [dict(row) for row in rows]}

    # ---------- Web 客户端 ----------

    if WEB_DIR.is_dir():
        from fastapi.responses import HTMLResponse
        from fastapi.staticfiles import StaticFiles

        app.mount("/static", StaticFiles(directory=str(WEB_DIR)), name="static")

        @app.get("/", response_class=HTMLResponse, include_in_schema=False)
        def index():
            page = WEB_DIR / "index.html"
            if not page.exists():
                return HTMLResponse("<h1>drama-cloud</h1><p>缺少 web/index.html</p>")
            return HTMLResponse(page.read_text(encoding="utf-8"))

    return app


def _dir_size(root: Path) -> int:
    total = 0
    for path in root.rglob("*"):
        try:
            if path.is_file():
                total += path.stat().st_size
        except OSError:
            continue
    return total


app = create_app()


def main(argv: Optional[Iterable[str]] = None) -> int:
    """`uvicorn drama_cloud.api:app` 之外的另一种启动方式（便于测试）。"""
    import uvicorn

    args = list(argv) if argv is not None else sys.argv[1:]
    host = args[0] if args else "127.0.0.1"
    port = int(args[1]) if len(args) > 1 else 8000
    uvicorn.run(app, host=host, port=port, log_level="info")
    return 0
