-- FluxDNS v2 resolve-detail daily shard layout.

CREATE TABLE detail_meta (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    layout_version INTEGER NOT NULL CHECK (layout_version > 0),
    day_utc INTEGER NOT NULL,
    created_at_utc_millis INTEGER NOT NULL CHECK (
        typeof(created_at_utc_millis) = 'integer' AND created_at_utc_millis >= 0
    )
);

CREATE TABLE resolve_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_time_utc_millis INTEGER NOT NULL CHECK (
        typeof(event_time_utc_millis) = 'integer' AND event_time_utc_millis >= 0
    ),
    duration_millis INTEGER NOT NULL CHECK (duration_millis >= 0),
    dns_core_duration_micros INTEGER CHECK (
        dns_core_duration_micros IS NULL OR dns_core_duration_micros >= 0
    ),
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
    upstream_member_id TEXT,
    matched_rule_source TEXT CHECK (
        matched_rule_source IS NULL OR matched_rule_source IN (
            'listener_hosts', 'strategy_hosts', 'rule_set'
        )
    ),
    matched_resource_id TEXT,
    matched_rule_ordinal INTEGER CHECK (
        matched_rule_ordinal IS NULL OR matched_rule_ordinal >= 0
    ),
    rcode INTEGER NOT NULL CHECK (rcode >= 0 AND rcode <= 15),
    cache_status TEXT NOT NULL,
    failure_class TEXT,
    cancellation_reason TEXT,
    runtime_revision INTEGER NOT NULL CHECK (runtime_revision >= 0),
    resource_revision TEXT,
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
    client_id TEXT,
    client_match_source TEXT CHECK (
        client_match_source IS NULL OR client_match_source IN ('id', 'ip')
    ),
    matched_client_id TEXT
);

CREATE INDEX resolve_log_event_time_idx
    ON resolve_log (event_time_utc_millis, id);
