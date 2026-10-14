# 数据模型与 Schema

> 交付物之五，对应设计文档 §9「核心实体」。
>
> 实现：`src/drama_cloud/db.py`。**本机没有 PostgreSQL 服务，实际用 SQLite**（WAL + `BEGIN IMMEDIATE`）。
> 差异被刻意限制在这一个文件里：DDL 只用两边都有的类型（TEXT / INTEGER / REAL），
> JSON 一律存 TEXT 并经过 `json_dump` / `json_load`，换 PG 时不需要改任何调用点。

## 1. 实体总览

```text
users ──┬─ projects ──┬─ assets                 （输入素材）
        │             └─ tasks ──┬─ task_attempts      （每次尝试）
        │                        ├─ task_checkpoints   （阶段检查点）
        │                        ├─ artifacts         （产物）
        │                        └─ review_queue      （人工复核队列，确认流分支B）
        └─ audit_events
recipes ── recipe_versions     （配方与不可变版本）
```

| 表 | 作用 | 增长方式 |
| --- | --- | --- |
| `users` | 令牌与角色 | 人工创建 |
| `projects` | 项目归属（默认自动创建） | 人工/自动 |
| `assets` | 输入素材登记 | 每次上传 |
| `recipes` | 配方标识 + 发布指针 | 每次发布 |
| `recipe_versions` | 不可变版本内容 | 每次发布 |
| `tasks` | 任务主表（状态、租约、计划快照） | 每次创建 |
| `task_attempts` | 每次执行的尝试记录 | 每次抢占 |
| `task_checkpoints` | 阶段完成情况与产物指纹 | 每步一次 |
| `artifacts` | 产物登记与保留期 | 每次登记 |
| `review_queue` | 擦除确认流的人工复核队列 | 每次需要人工确认 |
| `audit_events` | 关键操作审计 | 每次关键操作 |

主键统一用带前缀的不透明 id（`usr_` / `prj_` / `ast_` / `tsk_` / `art_` / `up_`），
由 `service.new_id()` 生成，客户端不应该解析它的含义。

## 2. 表结构

### `users`

| 列 | 类型 | 约束 | 说明 |
| --- | --- | --- | --- |
| `id` | TEXT | PK | `usr_…` |
| `name` | TEXT | NOT NULL | 显示名 |
| `role` | TEXT | NOT NULL, CHECK ∈ (`admin`,`operator`,`viewer`) | 角色 |
| `token_hash` | TEXT | NOT NULL, UNIQUE | 令牌的 SHA-256；**明文不落库** |
| `created_at` | TEXT | NOT NULL | ISO-8601 UTC |

### `projects`

| 列 | 类型 | 约束 |
| --- | --- | --- |
| `id` | TEXT | PK |
| `name` | TEXT | NOT NULL |
| `owner_id` | TEXT | NOT NULL, REFERENCES `users(id)` |
| `created_at` | TEXT | NOT NULL |

每个用户第一次操作时会自动获得一个「默认项目」（`ensure_default_project`），
学习原型不引入团队/权限模型。

### `assets`（输入素材）

| 列 | 类型 | 约束 | 说明 |
| --- | --- | --- | --- |
| `id` | TEXT | PK | `ast_…` |
| `project_id` | TEXT | NOT NULL, REFERENCES `projects(id)` | |
| `name` | TEXT | NOT NULL | 逻辑名 |
| `storage_key` | TEXT | NOT NULL | 不透明对象键，调用方不拼路径 |
| `sha256` | TEXT | NOT NULL | 内容哈希 |
| `size_bytes` | INTEGER | NOT NULL | |
| `probe_json` | TEXT | NULL | ffprobe 摘要（探测失败时为空） |
| `source_declaration` | TEXT | NULL | `{role, original_filename, authorization}`，声明素材来源与授权 |
| `lifecycle` | TEXT | NOT NULL DEFAULT `ready`, CHECK ∈ (`uploading`,`ready`,`rejected`,`deleted`) | 生命周期 |
| `created_at` | TEXT | NOT NULL | |

索引：`idx_assets_project (project_id)`。

### `recipes`（配方标识与发布指针）

| 列 | 类型 | 约束 | 说明 |
| --- | --- | --- | --- |
| `id` | TEXT | PK | 例如 `drama.full_edit` |
| `name` | TEXT | NOT NULL | 显示名 |
| `purpose` | TEXT | NOT NULL | 目前写入 `preset.mode` |
| `published_version` | INTEGER | NULL | **发布指针**；NULL 表示没有可用版本 |
| `created_at` | TEXT | NOT NULL | |

### `recipe_versions`（不可变版本）

| 列 | 类型 | 约束 | 说明 |
| --- | --- | --- | --- |
| `recipe_id` | TEXT | NOT NULL, REFERENCES `recipes(id)` | |
| `version` | INTEGER | NOT NULL | 与 `recipe_id` 组成主键 |
| `status` | TEXT | NOT NULL, CHECK ∈ (`draft`,`reviewing`,`published`,`deprecated`,`revoked`) | |
| `content_json` | TEXT | NOT NULL | 冻结的配方内容 |
| `content_sha256` | TEXT | NOT NULL | 内容哈希（**不含 `status`**） |
| `schema_version` | TEXT | NOT NULL | 发布时的配方 schema 版本 |
| `min_worker_version` | TEXT | NOT NULL | 最低 Worker 版本 |
| `required_capabilities` | TEXT | NOT NULL | JSON 数组 |
| `rollout_percent` | INTEGER | NOT NULL DEFAULT 100 | 灰度比例 0..100 |
| `reviewed_by` | TEXT | NULL | 审核人 |
| `published_at` | TEXT | NULL | 仅 `published` 时写入 |
| `created_at` | TEXT | NOT NULL | |

主键：`(recipe_id, version)`。

不变式：

1. 同一 `(recipe_id, version)` 写入后**不可修改**；修改只能发新版本（`publish_recipe` 会先查存在性）。
2. `content_sha256` 不含 `status`，所以发布/撤回不会改变内容指纹，任务里的 `recipe_sha256` 保持稳定。
3. 撤回/弃用当前发布版本时，发布指针自动回退到上一个仍为 `published` 的版本（`rollback_to`）。

### `tasks`（任务主表）

| 列 | 类型 | 约束 | 说明 |
| --- | --- | --- | --- |
| `id` | TEXT | PK | `tsk_…` |
| `project_id` | TEXT | NOT NULL, REFERENCES `projects(id)` | |
| `state` | TEXT | NOT NULL, CHECK ∈ 9 个状态 | 状态机兜底约束，见 §3 |
| `priority` | INTEGER | NOT NULL DEFAULT 100 | 越小越先执行 |
| `idempotency_key` | TEXT | UNIQUE | 幂等键 |
| `input_asset_ids` | TEXT | NOT NULL | JSON 数组，顺序即 `input_index` |
| `recipe_id` | TEXT | NOT NULL | |
| `recipe_version` | INTEGER | NOT NULL | |
| `recipe_sha256` | TEXT | NOT NULL | 创建时的内容哈希 |
| `recipe_snapshot_json` | TEXT | NOT NULL DEFAULT `{}` | **冻结的配方快照** |
| `plan_json` | TEXT | NOT NULL DEFAULT `{}` | 解析后的 `ProcessingPlan` |
| `required_capabilities` | TEXT | NOT NULL DEFAULT `[]` | 抢占时按能力过滤 |
| `target_json` | TEXT | NOT NULL | 目标规格快照 |
| `user_params_json` | TEXT | NOT NULL DEFAULT `{}` | 本次覆盖 |
| `cancel_requested` | INTEGER | NOT NULL DEFAULT 0 | 0/1 |
| `attempts` | INTEGER | NOT NULL DEFAULT 0 | 已尝试次数 |
| `progress_json` | TEXT | NOT NULL DEFAULT `{}` | 当前阶段与已完成步骤 |
| `next_run_at` | TEXT | NULL | 退避到期时间 |
| `lease_owner` | TEXT | NULL | 租约持有者（Worker id） |
| `lease_expires_at` | TEXT | NULL | 租约到期时间 |
| `error_class` | TEXT | NULL | 错误分类 |
| `error_message` | TEXT | NULL | 错误详情 |
| `result_json` | TEXT | NULL | 成功时的结果（产物列表、门禁报告） |
| `created_at` | TEXT | NOT NULL | |
| `started_at` | TEXT | NULL | 首次抢占时间 |
| `finished_at` | TEXT | NULL | 进入终态时间 |
| `updated_at` | TEXT | NOT NULL | |

索引：`idx_tasks_state (state, priority, created_at)`——正好是 `claim()` 的排序键。

`state` 上的 `CHECK` 是**兜底**：真正的合法性判定在 `tasks/state.py` 的 `ALLOWED_TRANSITIONS`，
`CHECK` 只是防止绕过状态机直接写库。

### `task_attempts`

| 列 | 类型 | 约束 | 说明 |
| --- | --- | --- | --- |
| `id` | INTEGER | PK AUTOINCREMENT | |
| `task_id` | TEXT | NOT NULL, REFERENCES `tasks(id)` | |
| `attempt` | INTEGER | NOT NULL | 第几次尝试（从 1 开始） |
| `worker_id` | TEXT | NULL | |
| `started_at` | TEXT | NOT NULL | |
| `finished_at` | TEXT | NULL | 成功或失败时写入 |
| `error_class` | TEXT | NULL | |
| `error_message` | TEXT | NULL | 截断到 2000 字符 |

唯一约束：`(task_id, attempt)`。抢占时用 `INSERT OR REPLACE`，
所以重复抢占同一 attempt 不会产生重复行。

### `task_checkpoints`

| 列 | 类型 | 约束 | 说明 |
| --- | --- | --- | --- |
| `task_id` | TEXT | NOT NULL, REFERENCES `tasks(id)` | |
| `stage` | TEXT | NOT NULL | 步骤 id |
| `input_sha256` | TEXT | NOT NULL | 计划指纹 |
| `recipe_version` | INTEGER | NOT NULL | |
| `engine_version` | TEXT | NOT NULL | 例如 `ffmpeg-4` |
| `params_digest` | TEXT | NOT NULL | 该步参数的 SHA-256 |
| `artifact_key` | TEXT | NULL | 仅物化文件的步骤有 |
| `artifact_sha256` | TEXT | NULL | |
| `completed` | INTEGER | NOT NULL DEFAULT 0 | |
| `updated_at` | TEXT | NOT NULL | |

主键：`(task_id, stage)`；写入用 `ON CONFLICT ... DO UPDATE`（幂等 upsert）。

### `artifacts`（产物）

| 列 | 类型 | 约束 | 说明 |
| --- | --- | --- | --- |
| `id` | TEXT | PK | `art_…` |
| `task_id` | TEXT | NOT NULL, REFERENCES `tasks(id)` | |
| `kind` | TEXT | NOT NULL | `output` / `chapter` / `report` / … |
| `storage_key` | TEXT | NOT NULL | |
| `sha256` | TEXT | NOT NULL | |
| `size_bytes` | INTEGER | NOT NULL | |
| `media_json` | TEXT | NULL | 媒体信息 |
| `quality_json` | TEXT | NULL | 门禁报告 |
| `retain_until` | TEXT | NULL | NULL 表示不自动清理 |
| `created_at` | TEXT | NOT NULL | |

索引：`idx_artifacts_task (task_id)`。

### `review_queue`（人工复核队列）

擦除确认流（`docs/INPAINT_REVIEW_FLOW.md`）的落库形态：自动校验失败且自动重试用尽时，
或结论为 `warn` 时，登记一条待复核记录。

| 列 | 类型 | 约束 | 说明 |
| --- | --- | --- | --- |
| `id` | TEXT | PK | `rev_…` |
| `task_id` | TEXT | NOT NULL, FK → tasks | 关联任务 |
| `artifact_id` | TEXT | NULL | 待放行产物；自动校验失败时为空（无成品可放行） |
| `reason` | TEXT | NOT NULL | `auto_verify_failed` / `auto_verify_warn` |
| `severity` | TEXT | NOT NULL, CHECK ∈ (`info`,`warn`,`high`) | 严重度 |
| `status` | TEXT | NOT NULL, CHECK ∈ (`pending`,`approved`,`rejected`,`superseded`) | 复核状态 |
| `detail_json` | TEXT | NULL | 擦除参数、指标摘要、逐项判据、修正建议 |
| `claimed_by` | TEXT | NULL | 认领人（避免并发处理同一条） |
| `created_at` | TEXT | NOT NULL | |
| `decided_by` | TEXT | NULL | 裁决人 |
| `decided_at` | TEXT | NULL | 裁决时间 |
| `decision_note` | TEXT | NULL | 备注；重跑时记录新任务 id |

索引：`idx_review_status (status, created_at)`、`idx_review_task (task_id)`。

规则：同一个 `(task_id, reason)` 只保留一条 `pending` 记录；复核是**记录 + 审计**，
不改写原任务的状态、尝试记录与参数——「修正后重跑」产生的是新任务。

### `audit_events`

| 列 | 类型 | 约束 |
| --- | --- | --- |
| `id` | INTEGER | PK AUTOINCREMENT |
| `ts` | TEXT | NOT NULL |
| `actor` | TEXT | NOT NULL |
| `action` | TEXT | NOT NULL |
| `subject` | TEXT | NULL |
| `detail_json` | TEXT | NULL |

索引：`idx_audit_ts (ts)`。

## 3. 状态约束

`tasks.state` 的 9 个取值与允许的迁移（`tasks/state.py`）：

```text
PENDING ──▶ QUEUED ──▶ RUNNING ──▶ VERIFYING ──▶ SUCCEEDED
   │           │          │  │          │
   │           │          │  └─▶ RETRY_WAITING ──▶ QUEUED
   └───────────┴──────────┴────▶ CANCEL_REQUESTED ──▶ CANCELLED
                              └─▶ FAILED
```

| 状态 | 出边 |
| --- | --- |
| `PENDING` | QUEUED / CANCELLED / FAILED |
| `QUEUED` | RUNNING / CANCEL_REQUESTED / CANCELLED / FAILED |
| `RUNNING` | VERIFYING / RETRY_WAITING / FAILED / CANCEL_REQUESTED / CANCELLED |
| `VERIFYING` | SUCCEEDED / RETRY_WAITING / FAILED / CANCELLED |
| `RETRY_WAITING` | QUEUED / CANCELLED / FAILED |
| `CANCEL_REQUESTED` | CANCELLED / SUCCEEDED / FAILED |
| `SUCCEEDED` / `CANCELLED` / `FAILED` | 无（终态） |

所有写状态的路径都必须经过 `state.transition()`，它支持 `allowed_from` 做**乐观并发**：
只有「当前状态仍在预期集合内」才迁移，避免两个 Worker 互相覆盖。

## 4. 连接与事务

```python
conn = sqlite3.connect(db_path, timeout=30.0, isolation_level=None)  # 显式事务
conn.row_factory = sqlite3.Row
PRAGMA journal_mode = WAL      # 读写并发
PRAGMA foreign_keys = ON       # 外键真的生效
PRAGMA busy_timeout = 30000
```

事务统一走 `db.transaction()`（`BEGIN IMMEDIATE` → `COMMIT`/`ROLLBACK`）。
抢占、创建任务、重试等需要原子性的操作都包在里面；
`claim()` 的 `UPDATE ... WHERE state IN (...) AND id = ?` 的 rowcount 就是「谁抢到了」的答案。

## 5. 迁移策略

学习原型不引入迁移框架，用两层防护：

1. **`SCHEMA` 里全是 `CREATE TABLE IF NOT EXISTS`**——空库可直接建全。
2. **`MIGRATIONS` + `_ensure_columns()`**——`IF NOT EXISTS` 不会给已有表补列，
   所以显式声明要补的列，`init_db()` 每次启动都会检查 `PRAGMA table_info()` 并
   `ALTER TABLE ADD COLUMN`。

当前补列清单（`MIGRATIONS`）：

```python
MIGRATIONS = {
  "tasks": {
    "recipe_snapshot_json": "TEXT NOT NULL DEFAULT '{}'",
    "plan_json":            "TEXT NOT NULL DEFAULT '{}'",
    "required_capabilities":"TEXT NOT NULL DEFAULT '[]'",
    "progress_json":        "TEXT NOT NULL DEFAULT '{}'",
    "started_at":           "TEXT",
    "finished_at":          "TEXT",
  },
}
```

这样改 schema 时不需要删库重建（`cloud/var/` 是可删目录，但学习者不必每次都重来）。

## 6. 迁移到 PostgreSQL 的替换点

| 项 | SQLite（当前） | PostgreSQL |
| --- | --- | --- |
| 连接 | `sqlite3.connect(path)` | `psycopg` 连接池 |
| 占位符 | `?` | `%s` |
| 自增主键 | `INTEGER PRIMARY KEY AUTOINCREMENT` | `BIGINT GENERATED ALWAYS AS IDENTITY` |
| JSON | TEXT + `json_dump`/`json_load` | `jsonb`（或保持 TEXT 零改动） |
| 并发控制 | `BEGIN IMMEDIATE` + `busy_timeout` | `SELECT … FOR UPDATE SKIP LOCKED` |
| 补列 | `ALTER TABLE ADD COLUMN` | 迁移工具（Alembic / Flyway） |
| 时间 | TEXT ISO-8601（UTC） | `timestamptz` |

因为 `db.py` 是唯一的方言集中点，替换时触达面就是这一个文件 + SQL 语句的字面量。

## 7. 保留期与清理

| 对象 | 策略 |
| --- | --- |
| `artifacts` | `retain_until` 早于当前时间即删除（`cleanup_expired_artifacts`）；`settings.artifact_retention_days=0` 表示不自动清理 |
| `assets` | 记录保留；对象删除不在原型范围内 |
| `task_checkpoints` | 随任务保留（复用判据） |
| `audit_events` | 只增不减（原型没有轮转策略） |
| `tasks` | 保留（终态任务的历史对排查很有用） |

清理入口：`drama-cloud cleanup`（可挂 cron）与 `cloud/demo.sh` 之外的手工调用。
