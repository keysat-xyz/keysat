-- Discount / referral codes.
--
-- A `discount_code` is a redeemable token (e.g. "FOUNDERS50") that reduces
-- the price of a purchase. A code can be either a percentage off (basis
-- points: 5000 = 50%) or a fixed sats off, can target a specific product
-- or policy or be universal, can have an optional usage cap and expiry,
-- and carries an optional `referrer_label` for tracking purposes (campaign
-- name, partner email, npub — free-form, not a separate user record).
--
-- Atomicity: `used_count` is incremented at purchase-start time via a
-- conditional UPDATE that gates on the cap. A `discount_redemptions` row
-- is inserted with status='pending' alongside the increment. The
-- redemption transitions to 'redeemed' on invoice settlement, or
-- 'cancelled' on invoice expiry/invalid (with a corresponding decrement
-- of used_count so freed slots become available again).

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS discount_codes (
    id                      TEXT PRIMARY KEY,           -- UUID v4
    code                    TEXT NOT NULL UNIQUE,       -- normalized to UPPERCASE on insert; case-insensitive lookup
    kind                    TEXT NOT NULL,              -- 'percent' | 'fixed_sats' | 'free_license'
    amount                  INTEGER NOT NULL,           -- basis points if percent, sats if fixed_sats, ignored if free_license (set to 0)
    max_uses                INTEGER,                    -- NULL = unlimited
    used_count              INTEGER NOT NULL DEFAULT 0,
    expires_at              TEXT,                       -- ISO-8601 UTC; NULL = never
    applies_to_product_id   TEXT,                       -- NULL = any product
    applies_to_policy_id    TEXT,                       -- NULL = any policy
    referrer_label          TEXT,                       -- optional, e.g. 'twitter-launch', 'alice@example.com'
    description             TEXT NOT NULL DEFAULT '',
    active                  INTEGER NOT NULL DEFAULT 1,
    created_at              TEXT NOT NULL,
    updated_at              TEXT NOT NULL,
    FOREIGN KEY (applies_to_product_id) REFERENCES products(id),
    FOREIGN KEY (applies_to_policy_id)  REFERENCES policies(id),
    CHECK (kind IN ('percent', 'fixed_sats', 'free_license')),
    CHECK (amount >= 0),
    CHECK (used_count >= 0)
);

CREATE INDEX IF NOT EXISTS idx_discount_codes_active   ON discount_codes(active);
CREATE INDEX IF NOT EXISTS idx_discount_codes_product  ON discount_codes(applies_to_product_id);
CREATE INDEX IF NOT EXISTS idx_discount_codes_policy   ON discount_codes(applies_to_policy_id);
CREATE INDEX IF NOT EXISTS idx_discount_codes_expires  ON discount_codes(expires_at);

CREATE TABLE IF NOT EXISTS discount_redemptions (
    id                      TEXT PRIMARY KEY,           -- UUID v4
    code_id                 TEXT NOT NULL,
    invoice_id              TEXT NOT NULL,              -- references invoices(id)
    license_id              TEXT,                       -- populated when license is issued
    status                  TEXT NOT NULL,              -- 'pending' | 'redeemed' | 'cancelled'
    discount_applied_sats   INTEGER NOT NULL,           -- base - final
    base_price_sats         INTEGER NOT NULL,           -- snapshot of product price at reservation time
    final_price_sats        INTEGER NOT NULL,           -- what BTCPay was actually charged
    created_at              TEXT NOT NULL,
    updated_at              TEXT NOT NULL,
    FOREIGN KEY (code_id)    REFERENCES discount_codes(id),
    FOREIGN KEY (invoice_id) REFERENCES invoices(id),
    FOREIGN KEY (license_id) REFERENCES licenses(id),
    CHECK (status IN ('pending', 'redeemed', 'cancelled'))
);

CREATE INDEX IF NOT EXISTS idx_discount_redemptions_code    ON discount_redemptions(code_id);
CREATE INDEX IF NOT EXISTS idx_discount_redemptions_invoice ON discount_redemptions(invoice_id);
CREATE INDEX IF NOT EXISTS idx_discount_redemptions_license ON discount_redemptions(license_id);
CREATE INDEX IF NOT EXISTS idx_discount_redemptions_status  ON discount_redemptions(status);

-- One redemption per invoice — a buyer can apply at most one code per
-- purchase. If they want to layer codes, they'll need a v0.2 feature.
CREATE UNIQUE INDEX IF NOT EXISTS idx_discount_redemptions_one_per_invoice
    ON discount_redemptions(invoice_id);
