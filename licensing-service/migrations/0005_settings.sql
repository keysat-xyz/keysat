-- Runtime-mutable settings, intentionally separated from the
-- startup-only env-var config in `Config::from_env`. Anything that
-- should be live-editable through admin actions or the future web UI —
-- and survive a daemon restart — goes here.
--
-- The table is a generic key/value store rather than dedicated columns
-- because the set of settings will grow over time, and the cost of a
-- key/value pattern with at most a few dozen rows is nil.

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS settings (
    key         TEXT PRIMARY KEY,
    value       TEXT,
    updated_at  TEXT NOT NULL
);
