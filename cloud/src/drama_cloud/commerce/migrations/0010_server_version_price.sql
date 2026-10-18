-- M5b-4：「服务端处理」的计价。
--
-- 与本地处理版本同价（一集 5 积分）：服务端只是替用户跑同一件事，
-- 差别在谁的机器出力。等真有机房/GPU 成本要摊，再按实际情况调这一行。
--
-- 用独立 subject 而不是复用 `version`：两者将来完全可能分开定价
-- （服务端要算力成本，本机不要），拆成两行比以后拆表容易。

-- 0006 的 CHECK 把 subject 钉死在两个值上，新增 subject 必须先放开它。
-- 换成「非空」约束：subject 的合法取值由 `credits.SUBJECTS` 与 `cost_for` 把关，
-- 数据库这层只挡住空串（在数据库里维护一份取值清单，每次加价目都要改迁移，
-- 不值当，而且容易与代码里的清单不同步）。
ALTER TABLE price_rules DROP CONSTRAINT IF EXISTS price_rules_subject_check;
ALTER TABLE price_rules ADD CONSTRAINT price_rules_subject_not_empty CHECK (subject <> '');

INSERT INTO price_rules (subject, credits_per_unit, min_units) VALUES
    ('server_version', 5, 1)
ON CONFLICT (subject) DO NOTHING;
