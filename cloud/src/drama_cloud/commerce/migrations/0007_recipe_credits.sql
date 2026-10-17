-- 0007 处理配方带上集数与价格（M5a）。
--
-- 为什么要存这两列：
-- 1. `episode_count`：服务端原本只收到任务哈希，看不到集数，没法按集计价；
-- 2. `credits`：价格在**签发时**定死并写库，消费时直接读这个值扣款。
--    如果消费时重算价，中间改过价目表就会出现「签发时说 25、扣钱时扣 30」。
--
-- 两列都可空：0007 之前签发的旧配方没有这两个值，它们只剩审计用途
-- （配方一次性、24 小时过期），不参与扣费。
--
-- 已执行的迁移不能再改；新改动写成新的编号文件。

ALTER TABLE recipes
    ADD COLUMN episode_count integer CHECK (episode_count IS NULL OR episode_count BETWEEN 1 AND 500),
    ADD COLUMN credits       integer CHECK (credits IS NULL OR credits > 0),
    -- 这两列要一起有或一起没有：只存了一个说明写入路径有 bug。
    ADD CONSTRAINT recipes_credits_consistent CHECK (
        (episode_count IS NULL) = (credits IS NULL)
    );
