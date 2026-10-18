# 数据模型与 Schema

> 交付物之五，对应设计文档 §9「核心实体」。
>
> 实现：`src/drama_cloud/db.py` + `src/drama_cloud/migrations/*.sql`。M5b 起**已迁到 PostgreSQL**（psycopg 3）。
> 换库时差异被刻意限制在 `db.py`：JSON 仍存 TEXT 并经过 `json_dump` / `json_load`，时间仍是 ISO-8601 字符串，
> 所以换 `jsonb` / `timestamptz` 不需要改调用点。
>
> 账号、卡密、积分、支付在**另一个服务、另一套库**（`commerce/migrations/`）里，本文只讲媒体控制面自己的表，
> 两者的分工见 `ARCHITECTURE.md` §10。

## 1. 实体总览

```text
users ──┬─ projects ──┬─ assets                 （输入素材）
        │             ├─ upload_sessions      （分片上传会话，M5b）
        │             └─ tasks ──┬─ task_attempts      （每次尝试）
        │                        ├─ task_checkpoints   （阶段检查点）
        │                        ├─ artifacts         （产物）
        │                        └─ review_queue      （人工复核队列，确认流分支B）
        └─ audit_events
media_admins / media_viewers   （角色白名单，按 commerce 邮箱，M5b）
recipes ── recipe_versions     （配方与不可变版本）
users ── billing_accounts ── billing_quotes ── billing_ledger（本地积分账本）
```

| 表 | 作用 | 增长方式 |
| --- | --- | --- |
| `users` | commerce 身份的**镜像** + 角色（M5b 起不再存可用凭据） | 每次登录内省时 upsert |
| `media_admins` | 管理员白名单（邮箱） | `initdb --admin`（M5b） |
| `media_viewers` | 只读白名单（邮箱） | 手工插入（M5b） |
| `projects` | 项目归属（默认自动创建） | 人工/自动 |
| `assets` | 输入素材登记，含保留期字段 | 每次上传 |
| `upload_sessions` | 分片上传会话与已收分片 | 每次开上传会话（M5b） |
| `recipes` | 配方标识 + 发布指针 | 每次发布 |
| `recipe_versions` | 不可变版本内容 | 每次发布 |
| `tasks` | 任务主表（状态、租约、计划快照、扣费引用） | 每次创建 |
| `task_attempts` | 每次执行的尝试记录 | 每次抢占 |
| `task_checkpoints` | 阶段完成情况与产物指纹 | 每步一次 |
| `artifacts` | 产物登记与保留期 | 每次登记 |
| `review_queue` | 擦除确认流的人工复核队列 | 每次需要人工确认 |
| `audit_events` | 关键操作审计 | 每次关键操作 |
| `billing_accounts` | 用户积分余额 | 仅本地学习账本 |
| `billing_quotes` | quote→task→settle 状态 | 报价、保留、结算/释放 |
| `billing_ledger` | 积分变更流水 | grant/hold/release 幂等记录 |
| `schema_migrations` | 迁移记账（哪个编号文件跑过了） | 每次迁移 |

主键统一用带前缀的不透明 id（`usr_` / `prj_` / `ast_` / `tsk_` / `art_` / `up_`），
由 `service.new_id()` 生成（分片上传的 id 由 `uploads.create` 用 sha256 前 24 位生成），
客户端不应该解析它的含义。只有 `task_attempts` 与 `audit_events` 用自增（`GENERATED ALWAYS AS IDENTITY`）。

## 2. 表结构

### `users`

M5b 起这张表只是 **commerce 身份的本地镜像**，不再存放能用来登录的凭据（`auth.py` 的模块注释）：

| 列 | 类型 | 约束 | 说明 |
| --- | --- | --- | --- |
| `id` | TEXT | PK | 直接等于 commerce 的 `user_id`（UUID 字符串） |
| `name` | TEXT | NOT NULL | 邮箱（小写）；拿不到邮箱时退化成 `user_id` |
| `role` | TEXT | NOT NULL, CHECK ∈ (`admin`,`operator`,`viewer`) | **只由本库白名单决定**，不看 commerce 的角色字段 |
| `token_hash` | TEXT | NOT NULL, UNIQUE | 影子行填 `commerce:<user_id>`；这个明文前缀串不等于任何 SHA-256 摘要，所以不会被 `resolve_token` 命中 |
| `created_at` | TEXT | NOT NULL | ISO-8601 UTC |

影子行由 `principal_for_email(conn, user_id, email)` 在每次内省后 `INSERT ... ON CONFLICT (id) DO UPDATE`
写入。**为什么要有它**：`projects.owner_id`、`assets`、`audit_events.actor` 等处都是 `REFERENCES users(id)`，
本地没有这一行外键会直接拦住建项目。

历史上用 `create_user` / `resolve_token` 建的不透明令牌用户保留在表里（迁移兼容与审计），但认证路径已不再走它们。

### `media_admins` / `media_viewers`（角色白名单，M5b）

| 列 | 类型 | 约束 | 说明 |
| --- | --- | --- | --- |
| `email` | TEXT | PK | 小写邮箱 |
| `created_at` | TEXT | NOT NULL | |

`role_for_email()` 按 `admin > viewer > operator` 的顺序判定：在白名单里的取对应角色，**都不在的一律 operator**
（默认给 operator 而不是 viewer：能传素材、建任务、看自己的东西，这是付费用户该有的能力）。
`initdb --admin <EMAIL>` 往 `media_admins` 插一行，`ON CONFLICT (email) DO NOTHING`，所以重复执行不报错。

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
| `retain_until` | TEXT | NULL | 保留期（M5b 由 `0002` 补列）；到期由 `cleanup` 处理 |
| `created_at` | TEXT | NOT NULL | |

索引：`idx_assets_project (project_id)`。

**保留期怎么用**：`cleanup_expired_assets` 看的是 `lifecycle = 'ready' AND created_at < cutoff`
（`cutoff` 由 `settings.asset_retention_days` 算，默认 30 天，0 表示不清理），
而不是看 `retain_until` —— `retain_until` 留给「按素材单独设期限」将来用。
到期时**真删磁盘文件**，元数据行保留：`lifecycle` 改为 `deleted`，`name` 与 `source_declaration`、`probe_json`
清空（这些才可能含敏感信息），`sha256` 与 `size_bytes` 留着供对账与审计。已经 `deleted` 的不会被再处理一次。

### `upload_sessions`（分片上传会话，M5b）

`uploads.py` 的落库形态。一个会话对应一次文件上传：客户端按 `chunk_size` 切块上传，
服务端把每块落到 `home/uploads/<id>/<index>.part`，完成时按序拼接并校验整文件 sha256。

| 列 | 类型 | 约束 | 说明 |
| --- | --- | --- | --- |
| `id` | TEXT | PK | `up_` + sha256 前 24 位（由项目、文件名与时间派生） |
| `project_id` | TEXT | NOT NULL, REFERENCES `projects(id)` | |
| `owner_id` | TEXT | NOT NULL, REFERENCES `users(id)` | 只有 owner 能往里写分片 |
| `name` | TEXT | NOT NULL | 目标素材的逻辑名 |
| `filename` | TEXT | NOT NULL | 最终对象键里的文件名 |
| `declared_size` | INTEGER | NOT NULL, CHECK ≥ 0 | 客户端声明的大小 |
| `declared_sha256` | TEXT | NOT NULL | 客户端声明的整文件哈希，完成时用来校验 |
| `chunk_size` | INTEGER | NOT NULL, CHECK > 0 | 每块字节数，默认 8 MiB |
| `received_json` | TEXT | NOT NULL DEFAULT `[]` | 已收到的分片下标（JSON 数组） |
| `status` | TEXT | NOT NULL DEFAULT `open`, CHECK ∈ (`open`,`completed`,`aborted`) | |
| `asset_id` | TEXT | NULL, REFERENCES `assets(id)` | 完成后指向新建的素材行 |
| `created_at` | TEXT | NOT NULL | |
| `updated_at` | TEXT | NOT NULL | 有分片写入或状态变化时更新 |

索引：`idx_upload_sessions_owner (owner_id, created_at)`。

不变式：

1. **完成前不写 `assets` 行**：素材表里不该出现「传了一半」的可用素材；
   完成后才在一个事务里建 `assets` 行（`lifecycle = 'ready'`）并把会话标成 `completed`。
2. 除最后一块外，每块长度必须正好等于 `chunk_size`；最后一块是
   `declared_size - chunk_size * (总块数 - 1)`。不符直接拒绝，避免拼出错位的文件。
3. 整文件哈希不符或大小不符时，**删掉分片目录并把会话标成 `aborted`**，绝不把对不上的素材交给后面的处理流程。
4. 同一 `index` 重复上传就是覆盖——断点续传就是「补 `missing` 里的块」，不需要额外的续传会话概念。
5. 分片上限 `MAX_CHUNKS = 10000`（8 MiB × 10000 ≈ 78 GB），超过时在开会话那一步就拒绝。

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
| `billing_quote_id` | TEXT | NULL | 走**本地** `billing.py` 账本时的报价 id |
| `credit_ref` | TEXT | NULL | 走 **commerce** 积分账本时的扣费引用（M5b 由 `0002` 补列） |
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

索引：`idx_tasks_state (state, priority, created_at)`——正好是 `claim()` 的排序键；
另有 `idx_tasks_credit_ref (credit_ref)`（`0002` 建的）。

`state` 上的 `CHECK` 是**兜底**：真正的合法性判定在 `tasks/state.py` 的 `ALLOWED_TRANSITIONS`，
`CHECK` 只是防止绕过状态机直接写库。

**`credit_ref` 与 `billing_quote_id` 是互斥的两条扣费路径**（`settle_task_billing`）：

| 列 | 走哪本账 | 结算方式 |
| --- | --- | --- |
| `credit_ref` 非空 | commerce 的 `credit_ledger`（服务端处理） | 用这个 ref 调 `/internal/credits/settle` 或 `/internal/credits/release` |
| `billing_quote_id` 非空 | 本地 `billing_quotes` / `billing_ledger` | `billing.settle_quote()` |
| 两者都空 | 不扣费 | 返回 `None`，什么都不做 |

`credit_ref` 的取值形如 `server_task:<幂等键或一次性 id>`，为空表示这个任务不扣 commerce 的积分
（例如老任务，或走本地 billing 的调用）。结算/退还失败时**不改任务状态**，只写审计
（`credit.settle` / `credit.release`，明细含 `ref`、`ok`、`detail`）——视频已经做出来了，
不能因为账没对上就把它判成失败。

### `task_attempts`

| 列 | 类型 | 约束 | 说明 |
| --- | --- | --- | --- |
| `id` | INTEGER | PK, `GENERATED ALWAYS AS IDENTITY` | 自增 |
| `task_id` | TEXT | NOT NULL, REFERENCES `tasks(id)` | |
| `attempt` | INTEGER | NOT NULL | 第几次尝试（从 1 开始） |
| `worker_id` | TEXT | NULL | |
| `started_at` | TEXT | NOT NULL | |
| `finished_at` | TEXT | NULL | 成功或失败时写入 |
| `error_class` | TEXT | NULL | |
| `error_message` | TEXT | NULL | 截断到 2000 字符 |

唯一约束：`(task_id, attempt)`。抢占时用 `INSERT ... ON CONFLICT (task_id, attempt) DO UPDATE`
（原来是 SQLite 的 `INSERT OR REPLACE`，PG 用 upsert 表达同一语义），所以重复抢占同一 attempt 不会产生重复行。

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
| `id` | INTEGER | PK, `GENERATED ALWAYS AS IDENTITY` |
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
conn = psycopg.connect(settings.dsn, row_factory=dict_row, autocommit=True)
```

- 连接串来自 `DRAMA_CLOUD_DSN`，没配时 `connect()` 直接抛 `RuntimeError` 并提示去配；
- `row_factory=dict_row`，所以到处都能写 `row["col"]`；
- **autocommit 是开着的**，事务要显式开：`with conn.transaction():`（`db.transaction()` 就是它的薄包装）。

事务语义：`conn.transaction()` 进入时 `BEGIN`、正常退出 `COMMIT`、异常 `ROLLBACK`；因为连接本身是 autocommit，
嵌套调用会变成 SAVEPOINT。抢占、创建任务、重试、分片写入、状态迁移等需要原子性的操作都包在里面；
`claim()` 的 `UPDATE ... WHERE state IN (...) AND id = %s` 的 rowcount 就是「谁抢到了」的答案。

**JSON 与时间仍是字符串**：JSON 一律 `json_dump` / `json_load`（`ensure_ascii=False`、`sort_keys=True`），
时间是 `now_iso()` 生成的 ISO-8601 UTC 字符串。换成 `jsonb` / `timestamptz` 时不需要改调用点，只是收益暂时为零。

## 5. 迁移机制

编号 SQL 文件放在 `src/drama_cloud/migrations/`，按文件名排序执行，**只跑一次**（`db.migrate`）：

| 文件 | 内容 |
| --- | --- |
| `0001_media_schema.sql` | 从原先 SQLite 的 `SCHEMA` 常量逐表翻译过来的全部基础表 |
| `0002_m5b_identity_uploads.sql` | M5b 新增：`media_admins` / `media_viewers`、`upload_sessions`、`assets.retain_until`、`tasks.credit_ref` |

规则：

1. **建表用 `CREATE TABLE IF NOT EXISTS`**：空库可一次建全，`initdb` 能重复执行。
2. **补列用 `ALTER TABLE ... ADD COLUMN IF NOT EXISTS`**：`IF NOT EXISTS` 只挡整表，不挡单列，
   所以 `0002` 里给 `assets` 与 `tasks` 补的两列都写了 `IF NOT EXISTS`——重复执行不会报错。
3. **每个迁移在独立事务里执行并记进 `schema_migrations`**（`version` 是文件名，`applied_at` 是 ISO 时间）。
   中途失败不会留下半套表：该文件既没生效也没记账，下次重跑。
4. **已应用的按 `version` 跳过**，所以 `drama-cloud initdb`、`serve`、`run-worker` 每次都调 `init_db()` 也是安全的。
5. **改 schema 写成新的编号文件**，不要改已经跑过的文件：`schema_migrations` 只记「跑过 0001」，
   不会发现 0001 的内容被改过。
6. `db.pending(settings)` 能列出还没跑的文件（给 CLI 与健康检查看）。

**幂等不等于可回滚**：这套机制保证「重复执行没有副作用」，但没有 down 迁移；
要退回去得自己写 SQL。学习原型接受这个取舍——`cloud/var/` 是可删目录，数据库也可以重建。

两个服务各有一套：commerce 的编号迁移在 `commerce/migrations/`（`0001` .. `0010`），
同样用 `schema_migrations` 记账，但由 `drama-commerce migrate` 执行，与这里互不干涉。

## 6. 与原 SQLite 实现的差异

M5b 之前这里用 SQLite（WAL + `BEGIN IMMEDIATE`）。迁移时保持的等价关系：

| 项 | 原 SQLite | 现在 PostgreSQL |
| --- | --- | --- |
| 连接 | `sqlite3.connect(path)` | `psycopg.connect(dsn, row_factory=dict_row)` |
| 占位符 | `?` | `%s` |
| 自增主键 | `INTEGER PRIMARY KEY AUTOINCREMENT` | `INTEGER GENERATED ALWAYS AS IDENTITY` |
| upsert | `INSERT OR REPLACE` | `INSERT ... ON CONFLICT (...) DO UPDATE` |
| JSON | TEXT + `json_dump`/`json_load` | 不变（仍是 TEXT） |
| 并发控制 | `BEGIN IMMEDIATE` + `busy_timeout` | `conn.transaction()`；抢占仍靠 `UPDATE ... WHERE state IN (...) AND id = %s` 的 rowcount |
| 补列 | `_ensure_columns()` 查 `PRAGMA table_info` | 编号迁移里的 `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` |
| 时间 | TEXT ISO-8601（UTC） | 不变（仍是 TEXT） |
| 布尔 | INTEGER 0/1 | 不变（代码里到处是 `= 0` / `= 1`，换 BOOLEAN 要改所有调用点） |

`db.py` 仍是唯一的方言集中点，替换时触达面就是这一个文件 + SQL 语句的字面量。

## 7. 保留期与清理

三类清理都由 `drama-cloud cleanup` 一次跑完（`cli.cmd_cleanup`），也可以挂 cron：

| 对象 | 策略 |
| --- | --- |
| `artifacts` | `retain_until` 早于当前时间即**连元数据一起删**（`cleanup_expired_artifacts`）。产物是「这次处理出来的东西」，删掉不影响任何对账（扣费记录在积分账本里） |
| `assets` | `lifecycle='ready' AND created_at < 现在 - asset_retention_days` 时**真删文件**、元数据保留（见 `assets` 一节）。`settings.asset_retention_days=0` 表示不清理，默认 30 天 |
| `upload_sessions` | `status='open' AND updated_at < 现在 - upload_retention_days` 时删掉分片目录并把会话标 `aborted`（`cleanup_abandoned_uploads`）。默认 7 天，0 表示不清理 |
| `tasks` 的租约 | `queue.reclaim_expired()` 把租约过期、仍处在执行态的任务回收重排队 |
| `task_checkpoints` | 随任务保留（复用判据） |
| `audit_events` | 只增不减（原型没有轮转策略） |
| `tasks` | 保留（终态任务的历史对排查很有用） |

`cleanup` 打印一行汇总：过期产物几个、过期素材几个、放弃的分片上传几个、回收的租约任务几个。
三类清理各自幂等：已经 `deleted` 的素材、已经 `aborted` 的会话都不会被再处理一次。
