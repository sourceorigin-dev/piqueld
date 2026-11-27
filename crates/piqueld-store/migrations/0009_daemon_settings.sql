-- Daemon settings set through the API, by key, such as
-- `retention.event_days`. Each value is JSON, which the daemon checks against
-- its setting when it is set and again whenever it loads them: one this
-- version doesn't take, such as one a newer version stored before a
-- downgrade, keeps the setting's default. Resetting a setting deletes its
-- row. The audit trail records who changed each one, and when.
CREATE TABLE daemon_settings (
    key TEXT PRIMARY KEY,
    value_json TEXT NOT NULL CHECK (json_valid(value_json))
) STRICT;
