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
