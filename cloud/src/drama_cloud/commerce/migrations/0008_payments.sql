-- 0008 支付订单与充值档位（M5a）。
--
-- 订单表是「钱从哪来」的凭据：积分流水只记结果（+N），订单表记原因
-- （谁、什么时候、买了多少积分、渠道侧的单号是多少）。对账时两边要能对上。
--
-- 已执行的迁移不能再改；新改动写成新的编号文件。

CREATE TABLE payment_orders (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id          uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- 我方订单号：全局唯一，回调按它去重。
    out_trade_no     text NOT NULL UNIQUE CHECK (length(out_trade_no) BETWEEN 8 AND 64),
    -- 渠道：mock / alipay / wechat
    channel          text NOT NULL CHECK (channel IN ('mock', 'alipay', 'wechat')),
    -- 金额，单位是分。只有整数——这是钱。
    amount_fen       bigint NOT NULL CHECK (amount_fen > 0),
    -- 这笔订单买多少积分（下单时按档位定死，之后改档位不影响已有订单）。
    credits          bigint NOT NULL CHECK (credits > 0),
    status           text NOT NULL CHECK (status IN ('created', 'paid', 'closed', 'refunded')),
    -- 渠道侧单号（支付宝 trade_no / 微信 transaction_id）。
    channel_trade_no text,
    paid_at          timestamptz,
    -- 充值档位（可空：手工开单或以后的活动单没有档位）。
    tier_id          text,
    created_at       timestamptz NOT NULL DEFAULT now(),
    expires_at       timestamptz NOT NULL,
    CHECK (expires_at > created_at),
    -- 已支付的订单必须有付款时间和渠道单号：否则这一行说不清钱到底到没到。
    CHECK (status <> 'paid' OR (paid_at IS NOT NULL AND channel_trade_no IS NOT NULL))
);

CREATE INDEX payment_orders_user ON payment_orders (user_id, created_at DESC);
-- 查单兜底要按「创建了但还没付」筛。
CREATE INDEX payment_orders_pending ON payment_orders (created_at)
    WHERE status = 'created';

-- 充值档位：金额 ↔ 积分。后台（M7）可改，这里只给默认值。
--
-- 用户定的价是「一个视频 5 积分」；档位按这个价换算成「花多少钱买多少积分」，
-- 具体金额是示例值，正式上线前按实际定价改。
CREATE TABLE recharge_tiers (
    id         text PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 32),
    -- 售价，单位是分。
    amount_fen bigint NOT NULL CHECK (amount_fen > 0),
    -- 这一档给多少积分。
    credits    bigint NOT NULL CHECK (credits > 0),
    -- 界面上显示的顺序，小的在前。
    sort_order integer NOT NULL DEFAULT 0,
    -- 下架后不再出现在列表里，但历史订单仍然能对上。
    active     boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now()
);

INSERT INTO recharge_tiers (id, amount_fen, credits, sort_order) VALUES
    -- 示例价：1 积分 = 1 分钱（即 100 积分 = 1 元），再给一点量大优惠。
    ('tier-100',   1000,   100, 10),
    ('tier-500',   5000,   550, 20),
    ('tier-2000',  20000, 2400, 30);
