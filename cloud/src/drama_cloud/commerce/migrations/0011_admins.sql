-- 0011 管理后台：管理员账号与其会话。
--
-- 为什么管理员是**独立的表**，不给 users 加 is_admin 字段：
-- 管理员是运营人员，不一定是付费用户；两种身份的生命周期也不同
-- （用户注销不该带走管理员账号，反之亦然）。混在一张表里迟早会出现
-- 「这个管理员为什么有积分流水」这类说不清的状态。
--
-- 管理员会话也独立成表：令牌前缀不同（adm_），所以管理员令牌即使被拿去当
-- 用户令牌用，也过不了 users 侧的校验——用途隔离，与签名用途前缀同一个思路。

CREATE TABLE admins (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    email         text NOT NULL UNIQUE
                  CHECK (email = lower(btrim(email)) AND length(email) BETWEEN 3 AND 254),
    password_hash text NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    -- 停用而不是删除：审计日志里会引用管理员 id，删掉就断了线索。
    disabled_at   timestamptz
);

CREATE TABLE admin_sessions (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    admin_id    uuid NOT NULL REFERENCES admins (id) ON DELETE CASCADE,
    created_at  timestamptz NOT NULL DEFAULT now(),
    expires_at  timestamptz NOT NULL,
    revoked_at  timestamptz,
    revoke_reason text
);
CREATE INDEX idx_admin_sessions_admin ON admin_sessions (admin_id, created_at DESC);

-- 管理员令牌只存摘要（bytea）：数据库泄露了也拿不到能用的令牌，与用户令牌一致。
CREATE TABLE admin_tokens (
    token_digest bytea PRIMARY KEY,
    session_id   uuid NOT NULL REFERENCES admin_sessions (id) ON DELETE CASCADE,
    expires_at   timestamptz NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX idx_admin_tokens_session ON admin_tokens (session_id);
