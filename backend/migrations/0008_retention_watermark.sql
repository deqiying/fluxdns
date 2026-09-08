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
