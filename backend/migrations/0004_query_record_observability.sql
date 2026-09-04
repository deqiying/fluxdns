-- 为 authenticated WebUI 查询记录补充实际客户端、上游来源和有界 answer 摘要。
-- 历史记录保持 NULL，由 Management 投影显式标记为 legacy_redacted。

ALTER TABLE resolve_log ADD COLUMN client_ip TEXT;

ALTER TABLE resolve_log ADD COLUMN upstream_used_id TEXT;

ALTER TABLE resolve_log ADD COLUMN answer_count INTEGER CHECK (
    answer_count IS NULL OR answer_count >= 0
);

ALTER TABLE resolve_log ADD COLUMN answers_truncated INTEGER CHECK (
    answers_truncated IS NULL OR answers_truncated IN (0, 1)
);

ALTER TABLE resolve_log ADD COLUMN answer_summary_json TEXT;
