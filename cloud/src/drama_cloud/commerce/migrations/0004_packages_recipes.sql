-- 0004 报审包与处理配方：只存元数据，不存文档内容。
-- packages：每生成一次报审包一行，用来审计、限流，以后接积分扣费（M5a）。
-- recipes：服务端签发的处理配方，只能消费一次（consumed_at 从空变成非空只发生一次）。
-- 已执行的迁移不能再改；新改动写成新的编号文件。

CREATE TABLE packages (
    id               uuid PRIMARY KEY,
    user_id          uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    device_id        text NOT NULL CHECK (device_id ~ '^[0-9a-f]{64}$'),
    created_at       timestamptz NOT NULL DEFAULT now(),
    signer           text NOT NULL CHECK (signer IN ('mock', 'scan')),
    episode_count    integer NOT NULL CHECK (episode_count BETWEEN 0 AND 500),
    -- manifest.json 原始字节的 SHA-256：报审包流出后，能对上是哪一次生成的。
    manifest_sha256  bytea NOT NULL CHECK (length(manifest_sha256) = 32)
);
CREATE INDEX packages_user ON packages (user_id, created_at DESC);

CREATE TABLE recipes (
    id           uuid PRIMARY KEY,
    user_id      uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    device_id    text NOT NULL CHECK (device_id ~ '^[0-9a-f]{64}$'),
    task         text NOT NULL CHECK (task ~ '^[0-9a-f]{64}$'),
    platform     text NOT NULL CHECK (platform IN ('douyin', 'kuaishou', 'channels')),
    strength     text NOT NULL CHECK (strength IN ('light', 'medium', 'heavy')),
    seed         bigint NOT NULL CHECK (seed >= 0),
    issued_at    timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL,
    consumed_at  timestamptz,
    CHECK (expires_at > issued_at),
    CHECK (consumed_at IS NULL OR consumed_at >= issued_at)
);
CREATE INDEX recipes_user ON recipes (user_id, issued_at DESC);
