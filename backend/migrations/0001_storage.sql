-- FluxDNS business storage schema. Cache persistence uses a separate database.

CREATE TABLE storage_meta (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    schema_version INTEGER NOT NULL CHECK (schema_version > 0),
    database_id TEXT NOT NULL CHECK (length(database_id) > 0),
    created_at_utc TEXT NOT NULL,
    migrated_at_utc TEXT NOT NULL
);

CREATE TABLE stats_daily_total (
    day_utc INTEGER PRIMARY KEY,
    total_requests INTEGER NOT NULL CHECK (total_requests >= 0)
);

CREATE TABLE stats_daily_dimension (
    day_utc INTEGER NOT NULL,
    dimension_kind TEXT NOT NULL CHECK (
        dimension_kind IN (
            'client_bucket', 'transport', 'strategy', 'source',
            'upstream', 'rcode', 'cache_status', 'attempt_outcome'
        )
    ),
    dimension_value TEXT NOT NULL CHECK (length(dimension_value) > 0),
    count INTEGER NOT NULL CHECK (count >= 0),
    PRIMARY KEY (day_utc, dimension_kind, dimension_value)
);

CREATE TABLE stats_batch_ledger (
    batch_id INTEGER PRIMARY KEY,
    max_event_seq INTEGER NOT NULL CHECK (max_event_seq >= 0),
    counter_epoch INTEGER NOT NULL CHECK (counter_epoch >= 0),
    committed_at_utc TEXT NOT NULL,
    payload_hash BLOB NOT NULL
);

CREATE TABLE resolve_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_time_utc TEXT NOT NULL,
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
    resource_revision TEXT
);

CREATE INDEX resolve_log_event_time_idx ON resolve_log (event_time_utc, id);
