-- Zaprite payment-provider config storage.
--
-- Mirror of btcpay_config from migration 0002, scoped to what
-- Zaprite actually requires:
--   - api_key: bearer token from app.zaprite.com/.../settings/api,
--     scoped per-organization. One key per Keysat instance.
--   - base_url: defaults to https://api.zaprite.com but kept
--     overridable for sandbox / future regional endpoints.
--   - webhook_id: nullable. Operators configure the webhook on
--     Zaprite's side (their dashboard); we record the id we get
--     back so we can list/delete it during a Disconnect.
--   - No webhook_secret column — Zaprite's webhook delivery model
--     doesn't expose HMAC signatures. Authentication is the
--     externalUniqId round-trip pattern instead (see
--     ZAPRITE_INTEGRATION_SPEC.md "Open questions resolved" §2).
--
-- Singleton row (id = 1) like btcpay_config — Keysat connects to
-- exactly one Zaprite organization per instance.

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS zaprite_config (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    api_key         TEXT NOT NULL,
    base_url        TEXT NOT NULL DEFAULT 'https://api.zaprite.com',
    webhook_id      TEXT,
    connected_at    TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);
