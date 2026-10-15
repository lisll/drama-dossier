-- 0002 授权：卡密批次与卡密、使用权、设备绑定（含解绑历史）。
-- 卡密只存摘要（bytea）：明文只在生成时写进导出的 CSV，数据库泄露了也拿不到能兑换的卡密。
-- 「月卡」按 30 天计；兑换时从「当前到期时间」和「现在」中较晚的那个往后顺延，永久卡不计时。
-- 已执行的迁移不能再改；新改动写成新的编号文件。

CREATE TABLE card_batches (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    name        text NOT NULL CHECK (length(name) BETWEEN 1 AND 100),
    kind        text NOT NULL CHECK (kind IN ('timed', 'lifetime')),
    days        integer CHECK (days BETWEEN 1 AND 3650),
    created_by  text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    revoked_at  timestamptz,
    CHECK ((kind = 'timed') = (days IS NOT NULL))
);

CREATE TABLE cards (
    code_digest  bytea PRIMARY KEY CHECK (length(code_digest) = 32),
    batch_id     uuid NOT NULL REFERENCES card_batches (id),
    redeemed_by  uuid REFERENCES users (id),
    redeemed_at  timestamptz,
    CHECK ((redeemed_by IS NULL) = (redeemed_at IS NULL))
);
CREATE INDEX cards_batch ON cards (batch_id);
CREATE INDEX cards_redeemed_by ON cards (redeemed_by) WHERE redeemed_by IS NOT NULL;

-- 每个用户最多一行；没有行就是没有使用权。
CREATE TABLE entitlements (
    user_id     uuid PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    kind        text NOT NULL CHECK (kind IN ('timed', 'lifetime')),
    expires_at  timestamptz,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    CHECK ((kind = 'timed') = (expires_at IS NOT NULL))
);

-- 一台电脑每次绑定一行。解绑只填 unbound_at，不删行，解绑次数从这里数。
CREATE TABLE devices (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id        uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    device_id      text NOT NULL CHECK (device_id ~ '^[0-9a-f]{64}$'),
    name           text NOT NULL CHECK (length(name) BETWEEN 1 AND 64),
    platform       text NOT NULL CHECK (platform IN ('windows', 'macos', 'linux')),
    app_version    text NOT NULL CHECK (length(app_version) BETWEEN 1 AND 32),
    session_id     uuid REFERENCES sessions (id) ON DELETE SET NULL,
    bound_at       timestamptz NOT NULL DEFAULT now(),
    last_seen_at   timestamptz NOT NULL DEFAULT now(),
    unbound_at     timestamptz,
    unbind_reason  text CHECK (unbind_reason IN ('user', 'admin')),
    CHECK ((unbound_at IS NULL) = (unbind_reason IS NULL))
);
-- 同一个账号里，同一台电脑同时只能有一条有效绑定。
CREATE UNIQUE INDEX devices_active ON devices (user_id, device_id) WHERE unbound_at IS NULL;
CREATE INDEX devices_user_unbinds ON devices (user_id, unbound_at) WHERE unbind_reason = 'user';
