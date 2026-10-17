-- 0009 邀请码与防刷（M5a）。
--
-- 目标：让老用户能邀请新用户，双方有奖励；同时不能让刷号的人靠邀请码套奖励。
--
-- 防刷的四道闸（都在应用层实现，理由见 invites.py）：
-- 1. 一个账号只能被邀请一次（`invited_by` 只写一次）；
-- 2. 同一台电脑（机器码）最多给一个账号当「邀请来源」；
-- 3. 被邀请人必须**真的付过钱**（有过已支付订单）才发奖励；
-- 4. 每个邀请人的奖励次数有上限。
--
-- 已执行的迁移不能再改；新改动写成新的编号文件。

ALTER TABLE users
    -- 邀请人。只能写一次（应用层保证）；注销账号时置空但保留历史关系不必要，直接 SET NULL。
    ADD COLUMN invited_by uuid REFERENCES users (id) ON DELETE SET NULL,
    -- 自己的邀请码。注册时生成，可复现（由 id 派生），所以不必唯一索引也不需要随机数。
    ADD COLUMN invite_code text;

CREATE INDEX users_invite_code ON users (invite_code);
CREATE INDEX users_invited_by ON users (invited_by);

-- 邀请记录：谁邀请了谁、奖励发没发、什么时候发的。
--
-- 单独一张表而不是只在 users 上记一个字段，是因为「奖励发放」需要自己的状态与审计：
-- 被邀请人可能过几天才付费，那时才发奖励。
CREATE TABLE invite_rewards (
    id           bigserial PRIMARY KEY,
    inviter_id   uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- 一个被邀请人只有一条记录。
    invitee_id   uuid NOT NULL UNIQUE REFERENCES users (id) ON DELETE CASCADE,
    -- 发奖条件：被邀请人完成首次付费后才置 true 并发放。
    granted_at   timestamptz,
    -- 发奖时的积分数量（之后改配置不影响已发的）。
    credits      integer CHECK (credits IS NULL OR credits > 0),
    created_at   timestamptz NOT NULL DEFAULT now(),
    -- 要么都为空（还没发），要么都有值（已发）。不允许「发了但没记数量」。
    CHECK ((granted_at IS NULL) = (credits IS NULL))
);

CREATE INDEX invite_rewards_inviter ON invite_rewards (inviter_id, created_at DESC);

-- 邀请奖励的配置：双方各得多少积分、每人最多能因邀请得多少次。
CREATE TABLE invite_settings (
    id                boolean PRIMARY KEY DEFAULT true CHECK (id),
    inviter_credits   integer NOT NULL CHECK (inviter_credits >= 0),
    invitee_credits   integer NOT NULL CHECK (invitee_credits >= 0),
    max_rewards       integer NOT NULL CHECK (max_rewards > 0),
    updated_at        timestamptz NOT NULL DEFAULT now()
);

INSERT INTO invite_settings (inviter_credits, invitee_credits, max_rewards)
VALUES (50, 20, 20);
