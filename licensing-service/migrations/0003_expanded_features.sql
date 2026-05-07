-- Expanded features: policies, machines, entitlements, expiry + grace,
-- suspension, outbound webhooks, admin audit log, and token-bucket rate
-- limiting. This migration is additive — v1 licenses issued before it was
-- applied still work, because the missing columns get sensible defaults.

PRAGMA foreign_keys = ON;

-- ---------------------------------------------------------------------------
-- Policies (Keygen-style license templates)
--
-- A policy encapsulates "how should licenses of this shape behave" so the
-- developer doesn't have to hand-pick values on every issuance. Example
-- policies for a single product: "Pro Perpetual", "Pro Annual",
-- "Pro 14-day Trial".
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS policies (
    id                  TEXT PRIMARY KEY,           -- UUID v4
    product_id          TEXT NOT NULL,
    name                TEXT NOT NULL,              -- human-readable, e.g. "Pro Perpetual"
    slug                TEXT NOT NULL,              -- short machine-id, unique within product
    duration_seconds    INTEGER NOT NULL DEFAULT 0, -- 0 = perpetual; else seconds from issuance to expiry
    grace_seconds       INTEGER NOT NULL DEFAULT 0, -- additional seconds after expiry where validate still returns ok with a warning
    max_machines        INTEGER NOT NULL DEFAULT 1, -- concurrent-activation cap; 1 mimics "one seat", 0 = unlimited
    is_trial            INTEGER NOT NULL DEFAULT 0, -- 0/1; trials get FLAG_TRIAL in signed payload
    price_sats_override INTEGER,                    -- if set, overrides product.price_sats for invoices using this policy
    entitlements_json   TEXT NOT NULL DEFAULT '[]', -- JSON array of feature slugs baked into every license
    metadata_json       TEXT NOT NULL DEFAULT '{}', -- free-form developer metadata
    active              INTEGER NOT NULL DEFAULT 1,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    FOREIGN KEY (product_id) REFERENCES products(id),
    UNIQUE (product_id, slug)
);

CREATE INDEX IF NOT EXISTS idx_policies_product ON policies(product_id);
CREATE INDEX IF NOT EXISTS idx_policies_active  ON policies(active);

-- ---------------------------------------------------------------------------
-- Licenses — extended
--
-- New columns for expiry, grace, suspension, entitlements cache, seat cap,
-- trial flag, and an optional Nostr npub (we'll use this later for DM-based
-- key delivery / recovery). None of these columns are required; older rows
-- get sensible defaults via DEFAULT clauses.
-- ---------------------------------------------------------------------------
ALTER TABLE licenses ADD COLUMN policy_id           TEXT REFERENCES policies(id);
ALTER TABLE licenses ADD COLUMN expires_at          TEXT;     -- ISO-8601 UTC; NULL = perpetual
ALTER TABLE licenses ADD COLUMN grace_seconds       INTEGER NOT NULL DEFAULT 0;
ALTER TABLE licenses ADD COLUMN max_machines        INTEGER NOT NULL DEFAULT 1;
ALTER TABLE licenses ADD COLUMN suspended_at        TEXT;
ALTER TABLE licenses ADD COLUMN suspension_reason   TEXT;
ALTER TABLE licenses ADD COLUMN entitlements_json   TEXT NOT NULL DEFAULT '[]';
ALTER TABLE licenses ADD COLUMN is_trial            INTEGER NOT NULL DEFAULT 0;
ALTER TABLE licenses ADD COLUMN nostr_npub          TEXT;
ALTER TABLE licenses ADD COLUMN buyer_email         TEXT;     -- denormalized from invoice for admin search; NULL for comps without email

CREATE INDEX IF NOT EXISTS idx_licenses_policy       ON licenses(policy_id);
CREATE INDEX IF NOT EXISTS idx_licenses_expires      ON licenses(expires_at);
CREATE INDEX IF NOT EXISTS idx_licenses_buyer_email  ON licenses(buyer_email);
CREATE INDEX IF NOT EXISTS idx_licenses_nostr_npub   ON licenses(nostr_npub);

-- ---------------------------------------------------------------------------
-- Machines (multi-seat activation model)
--
-- Replaces the single-column `fingerprint` on licenses for licenses that
-- allow more than one concurrent machine. Older code paths that only look at
-- licenses.fingerprint still work for single-seat licenses, but validate.rs
-- now also consults this table when max_machines != 1.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS machines (
    id                  TEXT PRIMARY KEY,           -- UUID v4
    license_id          TEXT NOT NULL,
    fingerprint         TEXT NOT NULL,              -- raw client-supplied id (we never stored the hash server-side; we store raw to allow rebind)
    fingerprint_hash    TEXT NOT NULL,              -- hex of SHA-256(fingerprint); indexed for fast lookup
    hostname            TEXT,                       -- optional human-friendly label the client may supply
    platform            TEXT,                       -- optional "linux-x64", "darwin-arm64", etc.
    ip_last_seen        TEXT,
    activated_at        TEXT NOT NULL,
    last_heartbeat_at   TEXT,
    deactivated_at      TEXT,                       -- NULL = active
    deactivation_reason TEXT,
    FOREIGN KEY (license_id) REFERENCES licenses(id)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_machines_license_fp ON machines(license_id, fingerprint_hash) WHERE deactivated_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_machines_license        ON machines(license_id);
CREATE INDEX IF NOT EXISTS idx_machines_heartbeat      ON machines(last_heartbeat_at);

-- ---------------------------------------------------------------------------
-- Outbound webhooks
--
-- Mirror of BTCPay's model: an endpoint is a URL + signing secret; each
-- delivery gets logged so admins can debug and retry.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS webhook_endpoints (
    id              TEXT PRIMARY KEY,               -- UUID v4
    url             TEXT NOT NULL,
    secret          TEXT NOT NULL,                  -- HMAC-SHA256 key (random, 32 bytes, hex)
    event_types     TEXT NOT NULL DEFAULT '["*"]',  -- JSON array of subscribed event types; "*" = all
    active          INTEGER NOT NULL DEFAULT 1,
    description     TEXT NOT NULL DEFAULT '',
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_webhook_endpoints_active ON webhook_endpoints(active);

CREATE TABLE IF NOT EXISTS webhook_deliveries (
    id                  TEXT PRIMARY KEY,           -- UUID v4
    endpoint_id         TEXT NOT NULL,
    event_type          TEXT NOT NULL,              -- license.issued, license.revoked, license.suspended, machine.activated, invoice.settled, etc.
    payload_json        TEXT NOT NULL,
    attempt_count       INTEGER NOT NULL DEFAULT 0,
    next_attempt_at     TEXT,                       -- NULL once delivered or permanently failed
    last_status_code    INTEGER,
    last_error          TEXT,
    delivered_at        TEXT,                       -- NULL until success
    created_at          TEXT NOT NULL,
    FOREIGN KEY (endpoint_id) REFERENCES webhook_endpoints(id)
);

CREATE INDEX IF NOT EXISTS idx_webhook_deliveries_endpoint ON webhook_deliveries(endpoint_id);
CREATE INDEX IF NOT EXISTS idx_webhook_deliveries_next     ON webhook_deliveries(next_attempt_at) WHERE delivered_at IS NULL;

-- ---------------------------------------------------------------------------
-- Admin audit log
--
-- Every mutation initiated through the admin API (product create, license
-- revoke, suspension, policy change, webhook edit, BTCPay reconnect, manual
-- issuance, etc.) writes one row. The API key used is hashed before storage
-- so the log alone can't be used to recover the key.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS audit_log (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    actor_kind      TEXT NOT NULL,              -- 'admin_api_key' | 'system' | 'btcpay_webhook'
    actor_hash      TEXT,                       -- SHA-256 of the actor's credential, or NULL for system
    action          TEXT NOT NULL,              -- dotted event name: product.create, license.revoke, etc.
    target_kind     TEXT,                       -- 'product' | 'license' | 'policy' | 'machine' | 'webhook' | 'invoice' | NULL
    target_id       TEXT,
    request_ip      TEXT,
    user_agent      TEXT,
    details_json    TEXT NOT NULL DEFAULT '{}',
    occurred_at     TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_audit_occurred ON audit_log(occurred_at);
CREATE INDEX IF NOT EXISTS idx_audit_target   ON audit_log(target_kind, target_id);
CREATE INDEX IF NOT EXISTS idx_audit_action   ON audit_log(action);

-- ---------------------------------------------------------------------------
-- Token-bucket rate limiting
--
-- We keep one row per (bucket_kind, bucket_key) so that e.g. per-IP validate
-- buckets and per-license heartbeat buckets are stored in the same table.
-- The refill happens lazily on every hit (classic token-bucket algorithm)
-- so there's no background filler task to worry about.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS rate_buckets (
    bucket_kind         TEXT NOT NULL,          -- 'validate_ip', 'validate_license', 'heartbeat_license', 'admin_ip', ...
    bucket_key          TEXT NOT NULL,          -- the IP, license_id, etc.
    tokens_remaining    REAL NOT NULL,
    capacity            REAL NOT NULL,
    refill_per_second   REAL NOT NULL,
    last_refill_at      TEXT NOT NULL,          -- ISO-8601; refill math runs off this
    PRIMARY KEY (bucket_kind, bucket_key)
);

CREATE INDEX IF NOT EXISTS idx_rate_buckets_refill ON rate_buckets(last_refill_at);

-- ---------------------------------------------------------------------------
-- Validation log — extended
--
-- Add columns for the new reject reasons (expired, suspended, too_many_machines)
-- so admins can tell at a glance why a check failed. The `result` column was
-- already TEXT so we just start writing new values to it.
-- ---------------------------------------------------------------------------
ALTER TABLE validation_log ADD COLUMN machine_id TEXT;     -- the machines.id that was matched / created, if any
ALTER TABLE validation_log ADD COLUMN reason_detail TEXT;  -- optional extra string, e.g. "grace period remaining: 3d"
