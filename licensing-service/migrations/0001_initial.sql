-- Initial schema for the licensing service.
--
-- SQLite is used in WAL mode; all tables are intentionally flat and indexed
-- for the common query paths (validate by key_id, list by product, look up by
-- invoice_id from BTCPay webhooks).

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS products (
    id              TEXT PRIMARY KEY,           -- UUID v4
    slug            TEXT NOT NULL UNIQUE,       -- human-friendly id used in URLs
    name            TEXT NOT NULL,
    description     TEXT NOT NULL DEFAULT '',
    price_sats      INTEGER NOT NULL,           -- price in satoshis
    active          INTEGER NOT NULL DEFAULT 1, -- boolean; 0 hides from listings
    metadata_json   TEXT NOT NULL DEFAULT '{}', -- arbitrary developer metadata
    created_at      TEXT NOT NULL,              -- ISO-8601 UTC
    updated_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_products_active ON products(active);

-- Invoices track BTCPay payment attempts. One invoice maps to at most one
-- license. If payment never completes, the invoice just sits in 'pending' /
-- 'expired' and no license is ever issued.
CREATE TABLE IF NOT EXISTS invoices (
    id                  TEXT PRIMARY KEY,           -- UUID v4 (our id)
    btcpay_invoice_id   TEXT NOT NULL UNIQUE,       -- id from BTCPay Server
    product_id          TEXT NOT NULL,
    status              TEXT NOT NULL,              -- pending | settled | expired | invalid
    buyer_email         TEXT,                       -- optional, supplied at purchase
    buyer_note          TEXT,                       -- optional purchase note
    amount_sats         INTEGER NOT NULL,
    checkout_url        TEXT NOT NULL,              -- BTCPay checkout URL
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    FOREIGN KEY (product_id) REFERENCES products(id)
);

CREATE INDEX IF NOT EXISTS idx_invoices_btcpay_id ON invoices(btcpay_invoice_id);
CREATE INDEX IF NOT EXISTS idx_invoices_status    ON invoices(status);

-- Licenses are the issued proofs-of-purchase. The `key_id` is what a client
-- presents when validating; the actual user-facing license key string is a
-- signed envelope containing this id plus metadata (see crypto module).
CREATE TABLE IF NOT EXISTS licenses (
    id                  TEXT PRIMARY KEY,           -- UUID v4, also the `license_id` in the signed payload
    product_id          TEXT NOT NULL,
    invoice_id          TEXT UNIQUE,                -- NULL for manually-issued / comped licenses
    status              TEXT NOT NULL,              -- active | revoked
    fingerprint         TEXT,                       -- optional machine fingerprint locked on first validation
    bound_identity      TEXT,                       -- optional user identity (email, pubkey, etc.) locked on first use
    issued_at           TEXT NOT NULL,
    revoked_at          TEXT,
    revocation_reason   TEXT,
    metadata_json       TEXT NOT NULL DEFAULT '{}',
    FOREIGN KEY (product_id) REFERENCES products(id),
    FOREIGN KEY (invoice_id) REFERENCES invoices(id)
);

CREATE INDEX IF NOT EXISTS idx_licenses_product ON licenses(product_id);
CREATE INDEX IF NOT EXISTS idx_licenses_status  ON licenses(status);

-- Audit log of validation attempts. Useful for abuse detection and for
-- developers building rate-limiting policies on top.
CREATE TABLE IF NOT EXISTS validation_log (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    license_id      TEXT,
    product_id      TEXT,
    fingerprint     TEXT,
    result          TEXT NOT NULL,      -- ok | bad_signature | revoked | product_mismatch | fingerprint_mismatch | not_found
    client_ip       TEXT,
    user_agent      TEXT,
    occurred_at     TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_validation_license ON validation_log(license_id);
CREATE INDEX IF NOT EXISTS idx_validation_time    ON validation_log(occurred_at);

-- Server-wide signing key. Stored here (rather than on disk) so a SQLite
-- backup captures the full server state. The private key is PEM-encoded.
-- Generated on first boot if no row exists.
CREATE TABLE IF NOT EXISTS server_keys (
    id              INTEGER PRIMARY KEY CHECK (id = 1),  -- singleton
    algorithm       TEXT NOT NULL,                       -- 'ed25519'
    public_key_pem  TEXT NOT NULL,
    private_key_pem TEXT NOT NULL,
    created_at      TEXT NOT NULL
);
