-- Migration 0016: scoped API keys
--
-- The master `admin_api_key` (set in config) is a full-access credential;
-- it's the right thing for the operator to hold but the wrong thing to
-- hand to an agent or a third-party tool. This table stores additional
-- API keys with operator-chosen roles that bound what they can do.
--
-- Storage model:
--   - `token_hash` is the sha256 of the raw token. We NEVER store the
--     raw token after generation — the create endpoint returns it once
--     in the response body and that's the operator's only chance to
--     copy it.
--   - `role` is a string tag (read-only | license-issuer | support |
--     full-admin). Scope sets per role are computed at auth time, not
--     stored here — that way we can extend the scope mapping without a
--     migration.
--   - `revoked_at` flips this row from valid → permanently rejected.
--     Soft-revoke rather than DELETE so the audit log keeps a stable
--     reference and the operator can see "this key existed but is
--     gone now" in the admin UI.
--   - `last_used_at` is touched best-effort on every successful auth.
--     Bounded write rate (one update per minute per key) is the next
--     iteration; for v1 we update every call.

CREATE TABLE IF NOT EXISTS scoped_api_keys (
    id              TEXT PRIMARY KEY NOT NULL,
    label           TEXT NOT NULL,
    token_hash      TEXT NOT NULL UNIQUE,
    role            TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    last_used_at    TEXT,
    revoked_at      TEXT,
    CHECK (role IN ('read-only', 'license-issuer', 'support', 'full-admin'))
);

CREATE INDEX IF NOT EXISTS idx_scoped_api_keys_token ON scoped_api_keys(token_hash);
CREATE INDEX IF NOT EXISTS idx_scoped_api_keys_active ON scoped_api_keys(revoked_at);
