CREATE INDEX IF NOT EXISTS resolve_log_duration_idx
    ON resolve_log (duration_millis, id);

CREATE INDEX IF NOT EXISTS resolve_log_client_id_time_idx
    ON resolve_log (client_id, event_time_utc_millis, id);

CREATE INDEX IF NOT EXISTS resolve_log_client_ip_time_idx
    ON resolve_log (client_ip, event_time_utc_millis, id);

CREATE INDEX IF NOT EXISTS resolve_log_matched_client_time_idx
    ON resolve_log (matched_client_id, event_time_utc_millis, id);

CREATE INDEX IF NOT EXISTS resolve_log_qname_time_idx
    ON resolve_log (canonical_qname, event_time_utc_millis, id);
