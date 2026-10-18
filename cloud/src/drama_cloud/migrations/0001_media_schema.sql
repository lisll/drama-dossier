-- 媒体控制面（drama-cloud）的表结构。
--
-- 从 `db.py` 里原先的 SQLite `SCHEMA` 常量逐表翻译而来（M5b-1）。
-- 翻译原则：
--
-- * 类型保持不变，仍是 TEXT/INTEGER（本仓库主键都是应用层生成的字符串 id，
--   不用自增；JSON 字段仍以 TEXT 存，读写都过 json_dump/json_load，
--   将来换 jsonb 不需要改调用点）。
-- * `INTEGER PRIMARY KEY AUTOINCREMENT` → `GENERATED ALWAYS AS IDENTITY`：
--   只有审计日志与任务尝试记录用自增，其余都是应用层 id。
-- * 布尔用 `INTEGER`（0/1）而不是 BOOLEAN：代码里到处是 `= 0` / `= 1` 的比较，
--   换类型要改所有调用点，收益为零。
-- * 时间仍是 ISO 8601 字符串（TEXT），不换 timestamptz：本仓库全程用字符串比较，
--   且 SQLite 版本的数据也是这个格式，不在这件事上做无收益的改动。
-- * `CREATE TABLE IF NOT EXISTS` 保留：`initdb` 要能重复执行。

CREATE TABLE IF NOT EXISTS users (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    role          TEXT NOT NULL CHECK (role IN ('admin', 'operator', 'viewer')),
    token_hash    TEXT NOT NULL UNIQUE,
    created_at    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS projects (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    owner_id      TEXT NOT NULL REFERENCES users(id),
    created_at    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS assets (
    id                 TEXT PRIMARY KEY,
    project_id         TEXT NOT NULL REFERENCES projects(id),
    name               TEXT NOT NULL,
    storage_key        TEXT NOT NULL,
    sha256             TEXT NOT NULL,
    size_bytes         INTEGER NOT NULL,
    probe_json         TEXT,
    source_declaration TEXT,
    lifecycle          TEXT NOT NULL DEFAULT 'ready'
                       CHECK (lifecycle IN ('uploading', 'ready', 'rejected', 'deleted')),
    created_at         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_assets_project ON assets(project_id);

CREATE TABLE IF NOT EXISTS recipes (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL,
    purpose           TEXT NOT NULL,
    published_version INTEGER,
    created_at        TEXT NOT NULL
);

-- 每次发布生成不可变版本：同一 (recipe_id, version) 不允许被覆盖。
CREATE TABLE IF NOT EXISTS recipe_versions (
    recipe_id            TEXT NOT NULL REFERENCES recipes(id),
    version              INTEGER NOT NULL,
    status               TEXT NOT NULL
                         CHECK (status IN ('draft', 'reviewing', 'published', 'deprecated', 'revoked')),
    content_json         TEXT NOT NULL,
    content_sha256       TEXT NOT NULL,
    schema_version       TEXT NOT NULL,
    min_worker_version   TEXT NOT NULL,
    required_capabilities TEXT NOT NULL,
    rollout_percent      INTEGER NOT NULL DEFAULT 100,
    reviewed_by          TEXT,
    published_at         TEXT,
    created_at           TEXT NOT NULL,
    PRIMARY KEY (recipe_id, version)
);

-- 状态取值见 tasks/state.py 的 TaskState；这里用 CHECK 兜底，防止绕过状态机写入。
CREATE TABLE IF NOT EXISTS tasks (
    id                     TEXT PRIMARY KEY,
    project_id             TEXT NOT NULL REFERENCES projects(id),
    state                  TEXT NOT NULL CHECK (state IN (
                               'PENDING', 'QUEUED', 'RUNNING', 'VERIFYING', 'SUCCEEDED',
                               'RETRY_WAITING', 'CANCEL_REQUESTED', 'CANCELLED', 'FAILED')),
    priority               INTEGER NOT NULL DEFAULT 100,
    idempotency_key        TEXT UNIQUE,
    billing_quote_id       TEXT,
    input_asset_ids        TEXT NOT NULL,
    recipe_id              TEXT NOT NULL,
    recipe_version         INTEGER NOT NULL,
    recipe_sha256          TEXT NOT NULL,
    -- 配方快照：任务创建时冻结，执行期间不跟随发布指针变化（§6.1）。
    recipe_snapshot_json   TEXT NOT NULL DEFAULT '{}',
    -- 解析后的 ProcessingPlan（步骤、参数、输入角色），Worker 直接照它执行。
    plan_json              TEXT NOT NULL DEFAULT '{}',
    required_capabilities  TEXT NOT NULL DEFAULT '[]',
    target_json            TEXT NOT NULL,
    user_params_json       TEXT NOT NULL DEFAULT '{}',
    cancel_requested       INTEGER NOT NULL DEFAULT 0,
    attempts               INTEGER NOT NULL DEFAULT 0,
    progress_json          TEXT NOT NULL DEFAULT '{}',
    next_run_at            TEXT,
    lease_owner            TEXT,
    lease_expires_at       TEXT,
    error_class            TEXT,
    error_message          TEXT,
    result_json            TEXT,
    created_at             TEXT NOT NULL,
    started_at             TEXT,
    finished_at            TEXT,
    updated_at             TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tasks_state ON tasks(state, priority, created_at);

CREATE TABLE IF NOT EXISTS task_attempts (
    id            INTEGER GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    task_id       TEXT NOT NULL REFERENCES tasks(id),
    attempt       INTEGER NOT NULL,
    worker_id     TEXT,
    started_at    TEXT NOT NULL,
    finished_at   TEXT,
    error_class   TEXT,
    error_message TEXT,
    UNIQUE (task_id, attempt)
);

-- 阶段级检查点：第一版不恢复任意 FFmpeg 进程的内部状态，只记录阶段产物与指纹。
CREATE TABLE IF NOT EXISTS task_checkpoints (
    task_id        TEXT NOT NULL REFERENCES tasks(id),
    stage          TEXT NOT NULL,
    input_sha256   TEXT NOT NULL,
    recipe_version INTEGER NOT NULL,
    engine_version TEXT NOT NULL,
    params_digest  TEXT NOT NULL,
    artifact_key   TEXT,
    artifact_sha256 TEXT,
    completed      INTEGER NOT NULL DEFAULT 0,
    updated_at     TEXT NOT NULL,
    PRIMARY KEY (task_id, stage)
);

CREATE TABLE IF NOT EXISTS artifacts (
    id            TEXT PRIMARY KEY,
    task_id       TEXT NOT NULL REFERENCES tasks(id),
    kind          TEXT NOT NULL,
    storage_key   TEXT NOT NULL,
    sha256        TEXT NOT NULL,
    size_bytes    INTEGER NOT NULL,
    media_json    TEXT,
    quality_json  TEXT,
    retain_until  TEXT,
    created_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_artifacts_task ON artifacts(task_id);

-- 确认流的人工复核队列（分支B）。
-- 自动校验失败（且自动重试已用尽）或只拿到 warn 结论时，任务会在这里排一条待复核记录；
-- 复核者可以标记通过/失败，也可以修正 mask 后重跑（重跑会产生一个新任务，本记录转 superseded）。
CREATE TABLE IF NOT EXISTS review_queue (
    id            TEXT PRIMARY KEY,
    task_id       TEXT NOT NULL REFERENCES tasks(id),
    -- 关联的可交付产物；自动校验失败时通常为空（没有可放行的成品）。
    artifact_id   TEXT,
    reason        TEXT NOT NULL,
    severity      TEXT NOT NULL CHECK (severity IN ('info', 'warn', 'high')),
    status        TEXT NOT NULL DEFAULT 'pending'
                  CHECK (status IN ('pending', 'approved', 'rejected', 'superseded')),
    detail_json   TEXT,
    claimed_by    TEXT,
    created_at    TEXT NOT NULL,
    decided_by    TEXT,
    decided_at    TEXT,
    decision_note TEXT
);
CREATE INDEX IF NOT EXISTS idx_review_status ON review_queue(status, created_at);
CREATE INDEX IF NOT EXISTS idx_review_task ON review_queue(task_id);

CREATE TABLE IF NOT EXISTS audit_events (
    id         INTEGER GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    ts         TEXT NOT NULL,
    actor      TEXT NOT NULL,
    action     TEXT NOT NULL,
    subject    TEXT,
    detail_json TEXT
);
CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_events(ts);

-- 本地积分账本（历史遗留）：只记录本地 quote→task→settle。
-- M5b 起「服务端处理」的扣费统一走 commerce 的积分账本，这套表保留为
-- 可独立运行的账本（CLI 与既有测试在用），文档里写明分工。
CREATE TABLE IF NOT EXISTS billing_accounts (
    user_id      TEXT PRIMARY KEY REFERENCES users(id),
    balance      INTEGER NOT NULL DEFAULT 0 CHECK (balance >= 0),
    updated_at   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS billing_quotes (
    id           TEXT PRIMARY KEY,
    user_id      TEXT NOT NULL REFERENCES users(id),
    units        INTEGER NOT NULL CHECK (units > 0),
    unit_cost    INTEGER NOT NULL CHECK (unit_cost > 0),
    amount       INTEGER NOT NULL CHECK (amount > 0),
    status       TEXT NOT NULL CHECK (status IN ('issued', 'reserved', 'settled', 'released', 'expired')),
    task_id      TEXT,
    expires_at   TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_billing_quotes_user ON billing_quotes(user_id, created_at);

CREATE TABLE IF NOT EXISTS billing_ledger (
    idempotency_key TEXT PRIMARY KEY,
    user_id         TEXT NOT NULL REFERENCES users(id),
    quote_id        TEXT,
    task_id         TEXT,
    delta           INTEGER NOT NULL,
    kind            TEXT NOT NULL CHECK (kind IN ('grant', 'hold', 'release')),
    created_at      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_billing_ledger_user ON billing_ledger(user_id, created_at);
