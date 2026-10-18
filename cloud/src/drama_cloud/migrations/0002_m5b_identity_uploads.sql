-- M5b 新增：角色白名单、分片上传会话、素材保留期。
--
-- 为什么要单独一个迁移：0001 是从 SQLite 原有的 SCHEMA 逐表翻译过来的，
-- 保持「与迁移前完全等价」便于对照；M5b 自己新增的东西放这里，
-- 将来回看时能一眼分清「原本就有」与「M5b 加的」。

-- 角色白名单：身份来自 commerce（邮箱），谁能当管理员 / 只读观察员由这里决定。
-- 不在两张表里的人一律按 operator。
CREATE TABLE IF NOT EXISTS media_admins (
    email      TEXT PRIMARY KEY,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS media_viewers (
    email      TEXT PRIMARY KEY,
    created_at TEXT NOT NULL
);

-- 分片上传会话（M5b-2）。一个会话对应一次文件上传：
-- 客户端按 chunk_size 切块上传，服务端把每块落到 uploads/<id>/<index>.part，
-- 完成时按序拼接并校验整文件 sha256。
CREATE TABLE IF NOT EXISTS upload_sessions (
    id            TEXT PRIMARY KEY,
    project_id    TEXT NOT NULL REFERENCES projects(id),
    owner_id      TEXT NOT NULL REFERENCES users(id),
    -- 目标素材名与最终要落到的对象键（完成时才真正写入 assets 行）。
    name          TEXT NOT NULL,
    filename      TEXT NOT NULL,
    declared_size INTEGER NOT NULL CHECK (declared_size >= 0),
    declared_sha256 TEXT NOT NULL,
    chunk_size    INTEGER NOT NULL CHECK (chunk_size > 0),
    -- 已收到的分片下标（JSON 数组）。断点续传靠它告诉客户端还缺哪些块。
    received_json TEXT NOT NULL DEFAULT '[]',
    status        TEXT NOT NULL DEFAULT 'open'
                  CHECK (status IN ('open', 'completed', 'aborted')),
    -- 完成后指向 assets 行。
    asset_id      TEXT REFERENCES assets(id),
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_upload_sessions_owner ON upload_sessions(owner_id, created_at);

-- 素材保留期（M5b-5）：到期由 cleanup 真删文件，元数据行保留。
-- 单独一列而不是全局配置，方便将来按用户/按项目设不同期限。
ALTER TABLE assets ADD COLUMN IF NOT EXISTS retain_until TEXT;

-- M5b-4：任务在 commerce 积分账本里的扣费引用。
-- 为空表示这个任务不扣 commerce 的积分（例如老任务、或走本地 billing 的调用）。
-- 非空时，任务成功要结算、失败/取消要退还，结算/退还都用这个 ref 去 commerce 找预扣记录。
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS credit_ref TEXT;
CREATE INDEX IF NOT EXISTS idx_tasks_credit_ref ON tasks(credit_ref);
