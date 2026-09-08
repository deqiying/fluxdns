CREATE TRIGGER resolve_log_day_guard
BEFORE INSERT ON resolve_log
WHEN (NEW.event_time_utc_millis / 86400000) != (
    SELECT day_utc FROM detail_meta WHERE singleton = 1
)
BEGIN
    SELECT RAISE(ABORT, 'resolve detail event does not belong to this UTC day');
END;
