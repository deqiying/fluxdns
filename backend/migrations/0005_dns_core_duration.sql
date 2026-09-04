-- 为新产生的解析记录补充服务端 DNS 主链耗时；历史记录保持 NULL，不做推测性回填。

ALTER TABLE resolve_log ADD COLUMN dns_core_duration_micros INTEGER CHECK (
    dns_core_duration_micros IS NULL OR dns_core_duration_micros >= 0
);
