-- BTCPay connection state.
--
-- Before v0.1 this lived purely in environment variables; now it's persisted
-- in the DB so the operator can connect to BTCPay via the one-click authorize
-- flow instead of pasting an API key into an env file.
--
-- A single row (id = 1). Rows are upserted on connect / reset.

CREATE TABLE IF NOT EXISTS btcpay_config (
    id                  INTEGER PRIMARY KEY CHECK (id = 1),  -- singleton
    base_url            TEXT NOT NULL,                       -- BTCPay base URL
    api_key             TEXT NOT NULL,                       -- issued by authorize flow
    store_id            TEXT NOT NULL,                       -- selected store id
    webhook_id          TEXT,                                -- BTCPay webhook id (for update/delete)
    webhook_secret      TEXT NOT NULL,                       -- HMAC-SHA256 secret shared with BTCPay
    connected_at        TEXT NOT NULL                        -- ISO-8601 UTC
);

-- CSRF tokens for an in-flight authorize round trip. The service generates one
-- when the operator clicks "Connect BTCPay", then validates it on the redirect
-- callback. Short-lived; pruned by timestamp.
CREATE TABLE IF NOT EXISTS btcpay_authorize_state (
    state_token     TEXT PRIMARY KEY,
    created_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_btcpay_authorize_state_time
    ON btcpay_authorize_state(created_at);
