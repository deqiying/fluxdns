-- v2 统计主库的完整初始化；详情仅存在于 UTC 日分片。

CREATE TABLE storage_meta (
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
    committed_at_utc_millis INTEGER NOT NULL CHECK (
        typeof(committed_at_utc_millis) = 'integer' AND committed_at_utc_millis >= 0
    ),
    payload_hash BLOB NOT NULL
);

-- 统计和详情共用的单调保留水位，以及待物理回收的详情日 manifest。

CREATE TABLE retention_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    revision INTEGER NOT NULL CHECK (revision > 0),
    watermark_revision INTEGER NOT NULL CHECK (watermark_revision > 0),
    retired_before_day_utc INTEGER NOT NULL,
    reference_day_utc INTEGER NOT NULL,
    target_days INTEGER NOT NULL CHECK (target_days > 0),
    sampled_detail_bytes INTEGER NOT NULL CHECK (
        typeof(sampled_detail_bytes) = 'integer' AND sampled_detail_bytes >= 0
    ),
    reference_size_bytes INTEGER NOT NULL CHECK (
        typeof(reference_size_bytes) = 'integer' AND reference_size_bytes > 0
    ),
    replay_floor_batch_id INTEGER NOT NULL CHECK (replay_floor_batch_id > 0),
    published_at_utc_millis INTEGER NOT NULL CHECK (
        typeof(published_at_utc_millis) = 'integer' AND published_at_utc_millis >= 0
    )
);

CREATE TABLE retention_detail_manifest (
    day_utc INTEGER PRIMARY KEY,
    retired_revision INTEGER NOT NULL CHECK (retired_revision > 0),
    state TEXT NOT NULL CHECK (state IN ('pending', 'reclaimed', 'failed')),
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    last_error_code TEXT,
    updated_at_utc_millis INTEGER NOT NULL CHECK (
        typeof(updated_at_utc_millis) = 'integer' AND updated_at_utc_millis >= 0
    )
);

CREATE INDEX retention_detail_manifest_state_day_idx
    ON retention_detail_manifest (state, day_utc);

-- 每日保留 owner 的本地任务日与最近一次运行结果。

CREATE TABLE retention_run_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    last_attempt_local_day INTEGER,
    last_success_local_day INTEGER,
    last_attempted_at_utc_millis INTEGER CHECK (
        last_attempted_at_utc_millis IS NULL OR (
            typeof(last_attempted_at_utc_millis) = 'integer'
            AND last_attempted_at_utc_millis >= 0
        )
    ),
    last_succeeded_at_utc_millis INTEGER CHECK (
        last_succeeded_at_utc_millis IS NULL OR (
            typeof(last_succeeded_at_utc_millis) = 'integer'
            AND last_succeeded_at_utc_millis >= 0
        )
    ),
    consecutive_failures INTEGER NOT NULL DEFAULT 0 CHECK (consecutive_failures >= 0),
    last_error_code TEXT
);

INSERT INTO retention_run_state (singleton) VALUES (1);
