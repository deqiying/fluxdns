-- 为 Management 查询安全投影补充传输类型；历史记录保持 NULL，不做推测性回填。

ALTER TABLE resolve_log ADD COLUMN transport TEXT CHECK (
    transport IS NULL OR transport IN ('udp', 'tcp', 'doh')
);
