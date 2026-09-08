-- 冻结 transport 原始身份与请求期匹配结果；管理名称仍仅作为旧查询过渡列保留。

ALTER TABLE resolve_log ADD COLUMN client_id TEXT;

ALTER TABLE resolve_log ADD COLUMN client_match_source TEXT CHECK (
    client_match_source IS NULL OR client_match_source IN ('id', 'ip')
);

ALTER TABLE resolve_log ADD COLUMN matched_client_id TEXT;
