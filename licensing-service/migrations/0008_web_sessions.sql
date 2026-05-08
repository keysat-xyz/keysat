-- Web UI password + session-based authentication.
--
-- Until v0.1.0:28 the only credential was the admin API key, which the
-- SPA stored in localStorage every login. This migration sets up the
-- alternate path: the operator sets a password (argon2id-hashed in the
-- settings table under key 'web_ui_password_hash'); successful login
-- issues a session token stored as an HttpOnly cookie. The API key
-- continues to work for automation; admin endpoints accept either
-- credential.
--
-- A future migration may add per-user accounts. For v0.1 there's a
-- single admin password — the StartOS service is single-tenant by
-- design and an operator's StartOS already gates physical access.

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS sessions (
    token         TEXT PRIMARY KEY,         -- random 32-byte URL-safe base64
    created_at    TEXT NOT NULL,
    expires_at    TEXT NOT NULL,            -- ISO-8601 UTC
    last_seen_at  TEXT NOT NULL,
    ip            TEXT,
    user_agent    TEXT
);

CREATE INDEX IF NOT EXISTS idx_sessions_expires ON sessions(expires_at);
