-- Migration 0023: add the 'merchant-onboard' scoped-API-key role.
--
-- 0016 created scoped_api_keys with a CHECK that pins `role` to the four
-- roles known then (read-only | license-issuer | support | full-admin).
-- SQLite can't ALTER or DROP a CHECK constraint in place, so adding a
-- fifth role means rebuilding the table with a widened CHECK.
--
-- scoped_api_keys has no foreign keys (inbound or outbound), so this is
-- the simple copy -> drop -> rename rebuild, without any of the FK
-- juggling that 0009 needed. sqlx-migrate wraps each file in a
-- transaction; we don't BEGIN here.
--
-- Idempotent: re-running produces the same end state. Existing rows (any
-- role, active or revoked) are preserved verbatim. The leading DROP IF
-- EXISTS clears a stray _new table from any partially-applied prior run
-- before we rebuild.

DROP TABLE IF EXISTS scoped_api_keys_new;

CREATE TABLE scoped_api_keys_new (
    id              TEXT PRIMARY KEY NOT NULL,
    label           TEXT NOT NULL,
    token_hash      TEXT NOT NULL UNIQUE,
    role            TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    last_used_at    TEXT,
    revoked_at      TEXT,
    CHECK (role IN ('read-only', 'license-issuer', 'support', 'merchant-onboard', 'full-admin'))
);

INSERT INTO scoped_api_keys_new
SELECT id, label, token_hash, role, created_at, last_used_at, revoked_at
FROM scoped_api_keys;

DROP TABLE scoped_api_keys;
ALTER TABLE scoped_api_keys_new RENAME TO scoped_api_keys;

CREATE INDEX IF NOT EXISTS idx_scoped_api_keys_token ON scoped_api_keys(token_hash);
CREATE INDEX IF NOT EXISTS idx_scoped_api_keys_active ON scoped_api_keys(revoked_at);
