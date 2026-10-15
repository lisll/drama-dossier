-- 0001 账号：用户、邮件验证码、会话与令牌、集中限流、审计日志。
-- 令牌和验证码只存摘要（bytea）：数据库泄露了，也拿不到能用的令牌或验证码。
-- 已执行的迁移不能再改；新改动写成新的编号文件。

CREATE TABLE users (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    email             text NOT NULL UNIQUE
                      CHECK (email = lower(btrim(email)) AND length(email) BETWEEN 3 AND 254),
    password_hash     text NOT NULL,
    terms_version     text NOT NULL,
    terms_accepted_at timestamptz NOT NULL,
    created_at        timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE email_codes (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email        text NOT NULL,
    purpose      text NOT NULL CHECK (purpose IN ('register', 'reset_password')),
    code_digest  bytea NOT NULL,
    attempts     smallint NOT NULL DEFAULT 0,
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL,
    consumed_at  timestamptz
);
CREATE INDEX email_codes_lookup ON email_codes (email, purpose, created_at DESC);

CREATE TABLE sessions (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id        uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at     timestamptz NOT NULL DEFAULT now(),
    expires_at     timestamptz NOT NULL,
    revoked_at     timestamptz,
    revoke_reason  text
);
CREATE INDEX sessions_user_active ON sessions (user_id) WHERE revoked_at IS NULL;

CREATE TABLE access_tokens (
    token_digest  bytea PRIMARY KEY,
    session_id    uuid NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    expires_at    timestamptz NOT NULL
);
CREATE INDEX access_tokens_session ON access_tokens (session_id);

CREATE TABLE refresh_tokens (
    token_digest  bytea PRIMARY KEY,
    session_id    uuid NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    created_at    timestamptz NOT NULL DEFAULT now(),
    expires_at    timestamptz NOT NULL,
    used_at       timestamptz
);
CREATE INDEX refresh_tokens_session ON refresh_tokens (session_id);

CREATE TABLE rate_limits (
    bucket        text NOT NULL,
    window_start  timestamptz NOT NULL,
    hits          integer NOT NULL,
    PRIMARY KEY (bucket, window_start)
);
CREATE INDEX rate_limits_window ON rate_limits (window_start);

CREATE TABLE audit_log (
    id       bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    at       timestamptz NOT NULL DEFAULT now(),
    actor    text NOT NULL,
    action   text NOT NULL,
    subject  text NOT NULL DEFAULT '',
    detail   jsonb NOT NULL DEFAULT '{}'::jsonb
);
CREATE INDEX audit_log_subject ON audit_log (subject, at);
