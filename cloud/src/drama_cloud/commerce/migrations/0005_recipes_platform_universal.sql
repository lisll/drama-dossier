-- 0005 处理配方只生成一套通用版本：platform 只允许 'universal'（M4b）。
-- 0004 允许的 douyin / kuaishou / channels 不再签发。
-- 约束加 NOT VALID：只约束今后写入的行，开发库里已有的旧配方行保留原样（配方一次性、24 小时过期，旧行只剩审计用途）。
-- 已执行的迁移不能再改；新改动写成新的编号文件。

ALTER TABLE recipes DROP CONSTRAINT recipes_platform_check;
ALTER TABLE recipes ADD CONSTRAINT recipes_platform_check CHECK (platform = 'universal') NOT VALID;
