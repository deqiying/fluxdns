-- 为解析详情补充策略目标、组成员和规则资源摘要；历史记录保持 NULL，不做回填。

ALTER TABLE resolve_log ADD COLUMN upstream_member_id TEXT;

ALTER TABLE resolve_log ADD COLUMN matched_rule_source TEXT CHECK (
    matched_rule_source IN ('listener_hosts', 'strategy_hosts', 'rule_set')
);

ALTER TABLE resolve_log ADD COLUMN matched_resource_id TEXT;

ALTER TABLE resolve_log ADD COLUMN matched_rule_ordinal INTEGER CHECK (
    matched_rule_ordinal IS NULL OR matched_rule_ordinal >= 0
);
