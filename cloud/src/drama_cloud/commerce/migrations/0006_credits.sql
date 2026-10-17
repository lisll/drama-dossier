-- 0006 积分账本与价目表（M5a）。
--
-- 设计要点：**账本只增不改**。不存 balance 字段，余额一律由 SUM(delta) 算出来。
-- 理由：余额是钱。存一个可变字段就意味着「余额」和「流水」是两份独立数据，
-- 算错一次之后再也无法从流水推回正确余额，对账时也说不清哪一边对。
-- 代价是查余额要扫一次索引；用户量级小，可接受。
--
-- 已执行的迁移不能再改；新改动写成新的编号文件。

-- 积分流水。只插入，不修改、不删除（应用层没有任何 UPDATE/DELETE 它的代码路径）。
CREATE TABLE credit_ledger (
    id              bigserial PRIMARY KEY,
    user_id         uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- 正数是加、负数是扣。单位是「积分」，只用整数（钱不用浮点）。
    delta           bigint NOT NULL CHECK (delta <> 0),
    -- grant 充值 / hold 预扣 / release 退还 / reward 邀请奖励 / expire 悬挂预扣回收
    kind            text NOT NULL CHECK (kind IN ('grant', 'hold', 'release', 'reward', 'expire')),
    -- 幂等键：同一个键只会入账一次。支付回调用 "order:<订单号>" 去重。
    idempotency_key text NOT NULL UNIQUE,
    -- 业务引用（订单号、配方编号、任务号等），只用于对账与展示。
    ref             text,
    -- 预扣的去向：只有 hold 才有；reserved 预扣中 / settled 已消费 / released 已退还。
    -- 用状态标记而不是再写一条「settle」流水，避免流水里出现「-5 然后 +5 -5」的噪声。
    settled_state   text CHECK (settled_state IN ('reserved', 'settled', 'released')),
    memo            text NOT NULL DEFAULT '',
    created_at      timestamptz NOT NULL DEFAULT now(),
    -- 只有 hold 能带状态，其它种类的流水没有「去向」这回事。
    CHECK ((kind = 'hold') = (settled_state IS NOT NULL))
);

-- 查余额、查流水都按「某个用户的最近记录」走这个索引。
CREATE INDEX credit_ledger_user_time ON credit_ledger (user_id, created_at DESC);
-- 回收悬挂预扣时要按「hold 且 state=reserved」筛。
CREATE INDEX credit_ledger_open_holds ON credit_ledger (settled_state, created_at)
    WHERE kind = 'hold' AND settled_state = 'reserved';

-- 价目表：每个计价对象一行。后台（M7）可改，这里只给初始值。
CREATE TABLE price_rules (
    subject          text PRIMARY KEY CHECK (subject IN ('version', 'package')),
    -- 计价单位：按集算。
    unit             text NOT NULL DEFAULT 'episode' CHECK (unit = 'episode'),
    -- 每个单位多少积分。用户定的是「一个视频 5 积分」。
    credits_per_unit integer NOT NULL CHECK (credits_per_unit > 0),
    -- 起扣下限：不足这么多单位也按这么多算（留一个「最少扣 1 集」的口子）。
    min_units        integer NOT NULL DEFAULT 1 CHECK (min_units > 0),
    updated_at       timestamptz NOT NULL DEFAULT now()
);

INSERT INTO price_rules (subject, credits_per_unit, min_units) VALUES
    ('version', 5, 1),
    ('package', 5, 1);
