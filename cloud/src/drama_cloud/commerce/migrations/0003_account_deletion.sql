-- 0003 账号注销：注销后立即匿名化，保留用户行。
-- 保留这一行，是因为卡密的 redeemed_by、审计日志（以后还有积分流水）都要引用账号 ID：
-- 邮箱和密码哈希清空以后，这个 ID 就对不上具体的人了。
-- PostgreSQL 的 UNIQUE 允许多个 NULL，所以同一个邮箱注销后可以重新注册。
-- 已执行的迁移不能再改；新改动写成新的编号文件。

ALTER TABLE users
    ADD COLUMN deleted_at timestamptz,
    ALTER COLUMN email DROP NOT NULL,
    ALTER COLUMN password_hash DROP NOT NULL,
    -- 要么是正常账号（邮箱、密码哈希都在），要么是注销过的账号（两者都清空并记下注销时间），
    -- 不允许「只清了邮箱」这种不完整的状态。
    ADD CONSTRAINT users_deleted_consistent CHECK (
        (deleted_at IS NULL AND email IS NOT NULL AND password_hash IS NOT NULL)
        OR (deleted_at IS NOT NULL AND email IS NULL AND password_hash IS NULL)
    );
