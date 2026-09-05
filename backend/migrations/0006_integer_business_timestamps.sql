-- 业务绝对时间统一为 Unix epoch 起算的 UTC 毫秒 INTEGER。
-- 旧 writer 生成非负十进制整数字符串，不能无损往返的值以 NOT NULL 失败回滚。
-- 本文件由既有 migration runner 包裹在同一事务中，保留记录 ID、ledger 和自增高水位。

CREATE TABLE storage_meta_v6 (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    schema_version INTEGER NOT NULL CHECK (schema_version > 0),
    database_id TEXT NOT NULL CHECK (length(database_id) > 0),
    created_at_utc_millis INTEGER NOT NULL CHECK (
        typeof(created_at_utc_millis) = 'integer' AND created_at_utc_millis >= 0
    ),
    migrated_at_utc_millis INTEGER NOT NULL CHECK (
        typeof(migrated_at_utc_millis) = 'integer' AND migrated_at_utc_millis >= 0
    )
);

INSERT INTO storage_meta_v6 (
    singleton, schema_version, database_id, created_at_utc_millis, migrated_at_utc_millis
)
SELECT singleton, schema_version, database_id,
    CASE WHEN CAST(CAST(created_at_utc AS INTEGER) AS TEXT) = created_at_utc
        THEN CAST(created_at_utc AS INTEGER) END,
    CASE WHEN CAST(CAST(migrated_at_utc AS INTEGER) AS TEXT) = migrated_at_utc
        THEN CAST(migrated_at_utc AS INTEGER) END
FROM storage_meta;

DROP TABLE storage_meta;
ALTER TABLE storage_meta_v6 RENAME TO storage_meta;

CREATE TABLE stats_batch_ledger_v6 (
    batch_id INTEGER PRIMARY KEY,
    max_event_seq INTEGER NOT NULL CHECK (max_event_seq >= 0),
    counter_epoch INTEGER NOT NULL CHECK (counter_epoch >= 0),
    committed_at_utc_millis INTEGER NOT NULL CHECK (
        typeof(committed_at_utc_millis) = 'integer' AND committed_at_utc_millis >= 0
    ),
    payload_hash BLOB NOT NULL
);

INSERT INTO stats_batch_ledger_v6 (
    batch_id, max_event_seq, counter_epoch, committed_at_utc_millis, payload_hash
)
SELECT batch_id, max_event_seq, counter_epoch,
    CASE WHEN CAST(CAST(committed_at_utc AS INTEGER) AS TEXT) = committed_at_utc
        THEN CAST(committed_at_utc AS INTEGER) END,
    payload_hash
FROM stats_batch_ledger;

DROP TABLE stats_batch_ledger;
ALTER TABLE stats_batch_ledger_v6 RENAME TO stats_batch_ledger;

CREATE TABLE resolve_log_v6 (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_time_utc_millis INTEGER NOT NULL CHECK (
        typeof(event_time_utc_millis) = 'integer' AND event_time_utc_millis >= 0
    ),
    duration_millis INTEGER NOT NULL CHECK (duration_millis >= 0),
    request_id_digest TEXT NOT NULL,
    listener_id TEXT NOT NULL,
    route_id TEXT,
    client_bucket TEXT,
    strategy_id TEXT,
    canonical_qname TEXT NOT NULL,
    qtype INTEGER NOT NULL CHECK (qtype >= 0 AND qtype <= 65535),
    qclass INTEGER NOT NULL CHECK (qclass >= 0 AND qclass <= 65535),
    source TEXT,
    upstream_id TEXT,
    rcode INTEGER NOT NULL CHECK (rcode >= 0 AND rcode <= 15),
    cache_status TEXT NOT NULL,
    failure_class TEXT,
    cancellation_reason TEXT,
    runtime_revision INTEGER NOT NULL CHECK (runtime_revision >= 0),
    resource_revision TEXT,
    upstream_member_id TEXT,
    matched_rule_source TEXT CHECK (
        matched_rule_source IN ('listener_hosts', 'strategy_hosts', 'rule_set')
    ),
    matched_resource_id TEXT,
    matched_rule_ordinal INTEGER CHECK (
        matched_rule_ordinal IS NULL OR matched_rule_ordinal >= 0
    ),
    transport TEXT CHECK (
        transport IS NULL OR transport IN ('udp', 'tcp', 'doh')
    ),
    client_ip TEXT,
    upstream_used_id TEXT,
    answer_count INTEGER CHECK (
        answer_count IS NULL OR answer_count >= 0
    ),
    answers_truncated INTEGER CHECK (
        answers_truncated IS NULL OR answers_truncated IN (0, 1)
    ),
    answer_summary_json TEXT,
    dns_core_duration_micros INTEGER CHECK (
        dns_core_duration_micros IS NULL OR dns_core_duration_micros >= 0
    )
);

-- 不能只取当前 MAX(id)，已删除记录留下的 AUTOINCREMENT 高水位也必须保留。
INSERT INTO sqlite_sequence (name, seq)
SELECT 'resolve_log_v6', seq FROM sqlite_sequence WHERE name = 'resolve_log';

INSERT INTO resolve_log_v6 (
    id, event_time_utc_millis, duration_millis, request_id_digest, listener_id,
    route_id, client_bucket, strategy_id, canonical_qname, qtype, qclass,
    source, upstream_id, rcode, cache_status, failure_class, cancellation_reason,
    runtime_revision, resource_revision, upstream_member_id, matched_rule_source,
    matched_resource_id, matched_rule_ordinal, transport, client_ip,
    upstream_used_id, answer_count, answers_truncated, answer_summary_json,
    dns_core_duration_micros
)
SELECT id,
    CASE WHEN CAST(CAST(event_time_utc AS INTEGER) AS TEXT) = event_time_utc
        THEN CAST(event_time_utc AS INTEGER) END,
    duration_millis, request_id_digest, listener_id, route_id, client_bucket,
    strategy_id, canonical_qname, qtype, qclass, source, upstream_id, rcode,
    cache_status, failure_class, cancellation_reason, runtime_revision, resource_revision,
    upstream_member_id, matched_rule_source, matched_resource_id, matched_rule_ordinal,
    transport, client_ip, upstream_used_id, answer_count, answers_truncated,
    answer_summary_json, dns_core_duration_micros
FROM resolve_log;

DROP TABLE resolve_log;
ALTER TABLE resolve_log_v6 RENAME TO resolve_log;
CREATE INDEX resolve_log_event_time_idx ON resolve_log (event_time_utc_millis, id);
