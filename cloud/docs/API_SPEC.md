# 控制面 API 规范

> 交付物之四，对应设计文档 §8.3 的 8 个端点（外加学习原型需要的辅助端点）。
>
> 实现：`src/drama_cloud/api.py`（FastAPI）。默认只监听 `127.0.0.1`，
> 对外暴露必须放到反向代理后面并启用 TLS（见 §9）。

## 1. 认证与授权

三种凭证，用途严格分开：

| 凭证 | 形式 | 用途 | 有效期 |
| --- | --- | --- | --- |
| 用户令牌 | `Authorization: Bearer <token>` | 所有业务端点 | 长期（数据库存 SHA-256） |
| 用户令牌（查询串） | `?token=<token>` | 浏览器直接打开的链接 | 同上 |
| 上传会话令牌 | `X-Upload-Token: <token>` 或 `?token=` | **只**给 `PUT /uploads/{id}` | 60..86400s，默认 900s |
| 下载令牌 | `?token=<token>` | **只**给 `GET /artifacts/{id}/download` | 900s |

- 用户令牌来自 `drama-cloud initdb --admin <name>` 或 `drama-cloud user-create`，
  **一次性打印**，服务端只存 `sha256`；丢了只能重新建用户。
- 上传/下载令牌是 HMAC-SHA256 签名的自包含载荷（`scope` 区分用途），服务端不保存状态；
  篡改、过期、`scope` 不符都会被拒。这就是「预签名 URL」的语义，将来换 S3 只需替换
  `storage.sign_payload()`。
- **下载令牌不是用户令牌**：`GET /artifacts/{id}/download` 走两条独立分支，
  不会把「对象级短时授权」和「用户身份认证」混在一起。

角色等级：`viewer (1) < operator (2) < admin (3)`。

| 端点类别 | 最低角色 |
| --- | --- |
| 查询（whoami / status / assets / tasks 读 / recipes 读 / artifacts 读） | viewer |
| 上传、创建任务、取消、重试 | operator |
| 发布配方、改配方状态、审计日志 | admin |

## 2. 通用约定

**错误响应**（`WorkError` 由全局异常处理器统一转换）：

```json
{ "error": "quality_gate_failed", "retryable": false, "message": "质量门禁未通过 → decodable: 产物没有可解码的视频轨" }
```

`error` 就是错误分类枚举值（`errors.ErrorClass`），`retryable` 直接告诉客户端「重试有没有意义」。

**错误分类 → HTTP 状态码**：

| 错误分类 | HTTP | 语义 |
| --- | --- | --- |
| `param_out_of_range` | 400 | 请求参数/业务取值不合法 |
| `recipe_schema_invalid` | 422 | 配方结构、白名单、参数或依赖不合法 |
| `input_corrupt` | 422 | 素材损坏或缺少视频轨 |
| `target_unsupported` | 422 | 容器/编码组合不支持 |
| `quality_gate_failed` | 422 | 门禁未通过（结果确定，重试无意义） |
| `capability_missing` | 503 | 集群缺能力/缺可执行文件 |
| `transient_io` | 503 | 临时 IO 失败 |
| `storage_unavailable` | 503 | 存储不可用（如素材对象缺失） |
| `resource_contention` | 409 | 资源争用、乐观并发冲突 |
| `worker_lost` | 409 | Worker 失联、租约易主 |
| `cancelled` | 409 | 任务已取消 |
| `internal` | 500 | 未预期异常 |

**框架级错误**：`401`（缺少/无效令牌）、`403`（角色不足、令牌与会话不匹配）、
`404`（项目不存在）、`410`（产物已按保留期清理）、`429`（限流）。

**限流**：按主体 + 路径的令牌桶，容量 120、每秒补 4。超限返回 `429` 并带 `Retry-After: 2`：

```json
{ "detail": { "error": "rate_limited", "message": "请求过于频繁，请稍后重试" } }
```

**幂等**：`POST /api/v1/tasks` 支持 `Idempotency-Key` 头（或请求体里的 `idempotency_key`）。
同一个键重复提交返回**同一个任务**（HTTP 仍是 201 语义上的「已创建」，返回既有任务对象），
不会新建第二条。键在库里是 `UNIQUE`。

## 3. 端点总表

| 方法 | 路径 | 角色 | 用途 |
| --- | --- | --- | --- |
| GET | `/api/v1/health` | 无 | 存活探测 |
| GET | `/api/v1/auth/whoami` | 登录 | 回显身份 |
| GET | `/api/v1/system/status` | viewer | 环境、能力、队列、存储概览 |
| POST | `/api/v1/uploads` | operator | 申请上传会话 |
| PUT | `/api/v1/uploads/{upload_id}` | 上传令牌 | 直传文件字节 |
| GET | `/api/v1/assets` | viewer | 列出素材 |
| POST | `/api/v1/tasks` | operator | 创建任务 |
| GET | `/api/v1/tasks` | viewer | 列出任务 |
| GET | `/api/v1/tasks/{task_id}` | viewer | 查询状态、进度、检查点、产物 |
| POST | `/api/v1/tasks/{task_id}/cancel` | operator | 请求取消 |
| POST | `/api/v1/tasks/{task_id}/retry` | operator | 手动重试 |
| GET | `/api/v1/recipes/resolve` | viewer | 解析配方（发布指针 / 指定版本 / 灰度） |
| GET | `/api/v1/recipes/{recipe_id}/versions/{version}` | viewer | 取指定版本 |
| GET | `/api/v1/recipes` | viewer | 列出配方版本 |
| GET | `/api/v1/recipes/schema.json` | viewer | 导出配方的 JSON Schema |
| POST | `/api/v1/recipes` | admin | 发布配方新版本 |
| POST | `/api/v1/recipes/{recipe_id}/versions/{version}/status` | admin | 改配方版本状态 |
| GET | `/api/v1/artifacts/{artifact_id}` | viewer | 产物元信息 + 下载链接 |
| GET | `/api/v1/artifacts/{artifact_id}/download` | 下载令牌 / Bearer | 下载产物字节 |
| GET | `/api/v1/audit` | admin | 审计事件 |
| GET | `/` `/static/*` | 无 | Web 客户端静态页 |

## 4. 端点详情

### GET `/api/v1/health`

无需认证。

```json
{ "status": "ok", "version": "0.1.0", "time": "2026-10-14T03:00:00+00:00" }
```

### GET `/api/v1/auth/whoami`

```json
{ "user_id": "usr_ab12cd34ef56", "name": "demo", "role": "admin" }
```

### GET `/api/v1/system/status`

```json
{
  "worker_version": "1.0.0",
  "engine_version": "ffmpeg-4",
  "ffmpeg": "ffmpeg version 4.2.4 ...",
  "capabilities": ["audio_filters", "ffmpeg", "ffprobe", "h264_encode", "signalstats", "subtitles", "video_filters"],
  "modules": ["audio.quality", "chapters.reorg", "..."],
  "queue_depth": 0,
  "tasks_by_state": { "SUCCEEDED": 4 },
  "storage": { "root": "/…/cloud/var/storage", "objects_bytes": 1234567 },
  "home": "/…/cloud/var"
}
```

### POST `/api/v1/uploads` → 201

请求：

```json
{ "name": "第 1 集", "filename": "e01.mp4", "role": "video", "ttl_seconds": 900 }
```

`role` ∈ `video` / `audio` / `subtitle` / `metadata`，否则 `400 bad_role`。
`ttl_seconds` ∈ 60..86400。

响应：

```json
{
  "upload_id": "up_...",
  "method": "PUT",
  "upload_url": "/api/v1/uploads/up_...",
  "token": "<upload-token>",
  "expires_in": 900
}
```

### PUT `/api/v1/uploads/{upload_id}`

- 认证：`X-Upload-Token: <token>` 或 `?token=<token>`（**不**要求 Bearer）。
- 请求体：**原始文件字节**，走 `request.stream()` 落盘，不经过 JSON 编解码，也不读进内存。
- 令牌里的 `upload_id` 必须与路径一致，否则 `403`。
- 空请求体 → `400 empty_body`。
- 成功后自动登记素材并探测一次；探测失败**不阻断上传**，素材仍然登记，
  任务执行时会以 `input_corrupt` 明确失败。

响应：

```json
{
  "asset": {
    "id": "ast_...", "project_id": "prj_...", "name": "第 1 集",
    "storage_key": "assets/ast_.../e01.mp4", "sha256": "...", "size_bytes": 123456,
    "role": "video",
    "probe": { "duration": 4.0, "width": 640, "height": 360, "fps": 25.0, "has_audio": true }
  }
}
```

### GET `/api/v1/assets`

最近 200 条 `lifecycle='ready'` 的素材：

```json
{
  "assets": [
    { "id": "ast_...", "project_id": "prj_...", "name": "第 1 集", "sha256": "...",
      "size_bytes": 123456, "source_declaration": "{\"role\":\"video\",...}",
      "created_at": "2026-10-14T03:00:00+00:00",
      "declaration": { "role": "video", "original_filename": "e01.mp4", "authorization": "self-produced" } }
  ]
}
```

### POST `/api/v1/tasks` → 201

请求：

```json
{
  "asset_ids": ["ast_aaa", "ast_bbb"],
  "recipe_id": "drama.subtitle.burn",
  "recipe_version": null,
  "user_params": { "encode": { "crf": 21 } },
  "priority": 100,
  "project_id": null,
  "idempotency_key": null
}
```

- `recipe_version=null` 表示用**发布指针**指向的版本。
- `asset_ids` 的顺序就是配方里 `input_index` 的顺序。
- `priority` 越小越先执行（0..10000，默认 100）。
- `user_params` 只能覆盖**已有**参数，不能新增字段（见 `RECIPE_SCHEMA.md` §10）。

响应体即任务详情（同 `GET /tasks/{id}`，含 `plan` 与 `recipe` 快照）。

### GET `/api/v1/tasks?state=&limit=`

`state` 可选（如 `RUNNING`），`limit` 1..500（默认 50）：

```json
{ "tasks": [ { "id": "tsk_...", "state": "SUCCEEDED", "priority": 100,
               "recipe_id": "drama.full_edit", "recipe_version": 1, "attempts": 1,
               "created_at": "...", "updated_at": "..." } ] }
```

### GET `/api/v1/tasks/{task_id}`

`include_plan=true` 时附带 `plan` 与 `recipe` 快照。响应还包含以 `/tasks` 端点补充的
`attempts_detail` 与 `checkpoints`：

```json
{
  "id": "tsk_...", "project_id": "prj_...", "state": "SUCCEEDED", "priority": 100, "attempts": 1,
  "input_asset_ids": ["ast_..."],
  "recipe_id": "drama.full_edit", "recipe_version": 1, "recipe_sha256": "…",
  "target": { "platform_profile": "vertical-1080x1920", "container": "mp4", "...": "..." },
  "cancel_requested": false,
  "progress": { "stage": "succeeded", "steps": [ { "id": "encode", "module": "media.encode", "status": "executed", "seconds": 1.23 } ] },
  "error": null,
  "result": { "artifacts": [ { "id": "art_...", "kind": "output", "sha256": "…", "size_bytes": 123 } ],
              "quality": { "ok": true, "checks": [ { "name": "decodable", "ok": true, "detail": "640x360 @ 25.0fps" } ] },
              "steps": [ "..." ], "worker_id": "worker-abc", "worker_version": "1.0.0", "engine_version": "ffmpeg-4" },
  "created_at": "…", "updated_at": "…", "started_at": "…", "finished_at": "…",
  "lease": { "owner": null, "expires_at": null },
  "artifacts": [ { "id": "art_...", "kind": "output", "sha256": "…", "size_bytes": 123, "media": {}, "quality": {}, "retain_until": null, "created_at": "…" } ],
  "attempts_detail": [ { "attempt": 1, "worker_id": "worker-abc", "started_at": "…", "finished_at": "…", "error_class": null, "error_message": null } ],
  "checkpoints": [ { "stage": "media.encode", "input_sha256": "…", "recipe_version": 1, "engine_version": "ffmpeg-4", "params_digest": "…", "artifact_key": "…", "artifact_sha256": "…", "completed": 1, "updated_at": "…" } ]
}
```

任务状态取值见 `DATABASE_SCHEMA.md` §`tasks`。

### POST `/api/v1/tasks/{task_id}/cancel`

- 任务已在终态 → `400`。
- `RUNNING` / `VERIFYING` → `CANCEL_REQUESTED`（Worker 在步骤边界协作退出）；
- 其他非终态 → 直接 `CANCELLED`。

返回更新后的任务详情。审计动作 `task.cancel`。

### POST `/api/v1/tasks/{task_id}/retry`

只允许对 `FAILED` / `CANCELLED` 重试：清零 `attempts`、清空错误与 `cancel_requested`，
回到 `RETRY_WAITING` 并重新入队。审计动作 `task.retry`。

### GET `/api/v1/recipes/resolve?recipe_id=&version=&rollout_bucket=`

- 不给 `version` 则用发布指针；`deprecated` / `revoked` 版本返回 `400`。
- `rollout_percent < 100` 且给了 `rollout_bucket` 时，`bucket % 100 >= rollout` 表示未命中 → `400`。

```json
{
  "recipe_id": "drama.full_edit", "version": 1, "status": "published",
  "content": { "schema_version": "1.0", "…": "…" },
  "content_sha256": "…", "required_capabilities": ["h264_encode", "video_filters", "audio_filters"],
  "rollout_percent": 100, "min_worker_version": "1.0.0", "reviewed_by": null,
  "published_at": "2026-10-14T03:00:00+00:00"
}
```

### GET `/api/v1/recipes/{recipe_id}/versions/{version}`

同上（指定版本），不受发布指针影响。

### GET `/api/v1/recipes?recipe_id=`

```json
{ "recipes": [ { "recipe_id": "drama.full_edit", "version": 1, "status": "published",
                 "content_sha256": "…", "rollout_percent": 100, "min_worker_version": "1.0.0",
                 "published_at": "…", "name": "drama.full_edit", "purpose": "full_edit",
                 "published_version": 1 } ] }
```

### GET `/api/v1/recipes/schema.json`

`Recipe.model_json_schema()`。只覆盖结构，跨字段规则见 `RECIPE_SCHEMA.md` §9。

### POST `/api/v1/recipes` → 201

```json
{ "recipe": { "schema_version": "1.0", "…": "…" }, "status": "published", "rollout_percent": 100, "reviewed_by": "alice" }
```

- 同一 `(recipe_id, version)` 已存在 → `400`（已发布版本不可修改）。
- `status` ∈ `draft`/`reviewing`/`published`/`deprecated`/`revoked`。
- 只有 `status=published` 才会更新发布指针。
- 返回 `{ recipe_id, version, status, content_sha256, required_capabilities }`。
- 审计动作 `recipe.publish`。

### POST `/api/v1/recipes/{recipe_id}/versions/{version}/status?status=`

撤回/弃用。撤回**当前发布版本**时，发布指针自动回退到上一个仍为 `published` 的版本。
审计动作 `recipe.status`。

### GET `/api/v1/artifacts/{artifact_id}`

```json
{
  "id": "art_...", "task_id": "tsk_...", "kind": "output",
  "storage_key": "artifacts/tsk_.../art_..._output.mp4",
  "sha256": "…", "size_bytes": 123456, "media": { }, "quality": { "ok": true, "checks": [ "…" ] },
  "retain_until": null, "created_at": "…",
  "download_url": "/api/v1/artifacts/art_.../download?token=<download-token>",
  "download_expires_in": 900
}
```

产物 `kind`（`output` / `chapter` / `report`）见 `PROCESSING_PIPELINE.md` §12。

### GET `/api/v1/artifacts/{artifact_id}/download`

- `?token=<下载令牌>`（浏览器直接打开）或 `Authorization: Bearer <用户令牌>`（脚本/CLI），二选一。
- 响应为文件字节，`Content-Type: application/octet-stream`，
  并带 `X-Artifact-SHA256` 头供客户端校验。
- 文件已被保留期清理 → `410 gone`。

### GET `/api/v1/audit?limit=`

admin 专属，`limit` 1..1000（默认 100），按 id 倒序：

```json
{ "events": [ { "id": 12, "ts": "…", "actor": "usr_...", "action": "recipe.publish",
                "subject": "drama.full_edit@1", "detail_json": "{\"status\":\"published\"}" } ] }
```

已记录的审计动作：`user.create`、`recipe.publish`、`recipe.status`、`recipe.rollback`、
`task.create`、`task.cancel`、`task.retry`、`asset.register`。

### GET `/` 与 `/static/*`

Web 客户端（令牌登录 → 直传素材 → 选配方建任务 → 轮询状态 → 下载产物），
静态文件来自 `src/drama_cloud/web/`。

## 5. 典型调用序列

```bash
TOKEN=<登录令牌>

# 1) 申请上传会话
UP=$(curl -sS -X POST http://127.0.0.1:8000/api/v1/uploads \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"name":"第 1 集","filename":"e01.mp4","role":"video"}')
UPLOAD_ID=$(python3 -c "import json,sys;print(json.load(sys.stdin)['upload_id'])" <<<"$UP")
UPLOAD_TOKEN=$(python3 -c "import json,sys;print(json.load(sys.stdin)['token'])" <<<"$UP")

# 2) 直传字节
curl -sS -X PUT "http://127.0.0.1:8000/api/v1/uploads/$UPLOAD_ID" \
  -H "X-Upload-Token: $UPLOAD_TOKEN" --data-binary @e01.mp4

# 3) 建任务（带幂等键）
curl -sS -X POST http://127.0.0.1:8000/api/v1/tasks \
  -H "Authorization: Bearer $TOKEN" -H 'Idempotency-Key: e01-run-1' \
  -H 'Content-Type: application/json' \
  -d '{"asset_ids":["ast_..."],"recipe_id":"drama.no_edit_generic"}'

# 4) 起 Worker 并轮询
drama-cloud run-worker --once
curl -sS "http://127.0.0.1:8000/api/v1/tasks/tsk_..." -H "Authorization: Bearer $TOKEN"

# 5) 取下载链接（含短时令牌）
curl -sS "http://127.0.0.1:8000/api/v1/artifacts/art_..." -H "Authorization: Bearer $TOKEN"
```

## 6. 一致性规则（客户端需要知道的）

1. **创建任务不等待执行**：`POST /tasks` 只做校验与入队，返回时任务通常是 `PENDING`/`QUEUED`。
2. **状态只能由服务端迁移**：客户端无法直接写状态；`cancel` / `retry` 也走各自的语义。
3. **配方快照冻结**：任务创建时把配方内容写进 `recipe_snapshot_json`，
   执行期间发布新版本不会影响已创建的任务。
4. **能力不匹配在创建时就报错**：`capability_missing` → `503`，不会先排队再失败。
5. **`409` 表示「可能因为并发/资源」**：客户端可以退避重试拿到的最新状态。
6. **`422` 表示「请求本身有问题」**：重试不会改变结果。

## 7. 已知限制（原型）

- 限流是单进程内存实现，多实例部署需要换成集中式计数器；
- 无 OAuth/OIDC，令牌是数据库里的不透明值；
- 无分页游标，列表端点都用 `limit` 截断；
- 无 WebSocket/SSE 推送，状态靠客户端轮询。
