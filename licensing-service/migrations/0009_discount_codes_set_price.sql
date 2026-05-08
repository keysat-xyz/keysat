-- Allow `kind = 'set_price'` on discount_codes (added at the daemon
-- level in v0.1.0:26 but the migration that created the CHECK constraint
-- in 0004 didn't include it, so existing instances reject the new kind
-- with "CHECK constraint failed").
--
-- SQLite doesn't support ALTER TABLE ... DROP/ALTER CONSTRAINT, so we
-- rebuild discount_codes: copy → drop old → rename. sqlx-migrate already
-- wraps each .sql file in a transaction; nested transactions aren't
-- allowed, so we don't BEGIN here.
--
-- Earlier revisions of this migration relied on `PRAGMA defer_foreign_keys`
-- alone to let DROP TABLE discount_codes succeed while discount_redemptions
-- still had FK references back into it. That fails at COMMIT on any
-- instance with even one row in discount_redemptions: SQLite's deferred
-- FK check sees the dropped parent's row-deletion bookkeeping as
-- unsatisfied, regardless of whether discount_codes_new (now renamed
-- back to discount_codes) contains the same IDs. SQLite error 787,
-- whole transaction rolls back, daemon won't boot.
--
-- The robust fix is to rebuild discount_redemptions inside the same
-- transaction so its FK is freshly bound to the new discount_codes:
--   1. heal any pre-existing orphan rows
--   2. stash discount_redemptions into a TEMP table
--   3. drop discount_redemptions (eliminates the inbound FK)
--   4. rebuild discount_codes with the new CHECK
--   5. recreate discount_redemptions and restore data
-- defer_foreign_keys still postpones intra-transaction FK firing; the
-- COMMIT-time check passes because both tables are clean and consistent.
--
-- This migration is idempotent: re-running it produces the same end state.
-- That matters because operators who hit the broken first revision and
-- worked around it (manually deleting redemptions then booting) will get
-- a checksum mismatch on the next .s9pk update; the recovery is to
-- DELETE FROM _sqlx_migrations WHERE version = 9 and let this fixed
-- version re-apply.

PRAGMA defer_foreign_keys = 1;

-- ---------------------------------------------------------------------------
-- 1. Heal orphan FK references inherited from earlier manual SQL wipes.
-- ---------------------------------------------------------------------------
UPDATE discount_codes
   SET applies_to_product_id = NULL
 WHERE applies_to_product_id IS NOT NULL
   AND applies_to_product_id NOT IN (SELECT id FROM products);

UPDATE discount_codes
   SET applies_to_policy_id = NULL
 WHERE applies_to_policy_id IS NOT NULL
   AND applies_to_policy_id NOT IN (SELECT id FROM policies);

-- A redemption pointing at a no-longer-existing discount_code can't be
-- meaningful — drop it. The first revision of this migration didn't
-- handle this case at all, so any leftover orphans have to go.
DELETE FROM discount_redemptions
 WHERE code_id NOT IN (SELECT id FROM discount_codes);

-- ---------------------------------------------------------------------------
-- 2. Stash discount_redemptions, drop the table to break the inbound FK.
-- ---------------------------------------------------------------------------
CREATE TEMP TABLE _dr_stash AS SELECT * FROM discount_redemptions;
DROP TABLE discount_redemptions;

-- ---------------------------------------------------------------------------
-- 3. Rebuild discount_codes with the new CHECK constraint.
-- ---------------------------------------------------------------------------
CREATE TABLE discount_codes_new (
    id                      TEXT PRIMARY KEY,
    code                    TEXT NOT NULL UNIQUE,
    kind                    TEXT NOT NULL,              -- 'percent' | 'fixed_sats' | 'set_price' | 'free_license'
    amount                  INTEGER NOT NULL,
    max_uses                INTEGER,
    used_count              INTEGER NOT NULL DEFAULT 0,
    expires_at              TEXT,
    applies_to_product_id   TEXT,
    applies_to_policy_id    TEXT,
    referrer_label          TEXT,
    description             TEXT NOT NULL DEFAULT '',
    active                  INTEGER NOT NULL DEFAULT 1,
    created_at              TEXT NOT NULL,
    updated_at              TEXT NOT NULL,
    FOREIGN KEY (applies_to_product_id) REFERENCES products(id),
    FOREIGN KEY (applies_to_policy_id)  REFERENCES policies(id),
    CHECK (kind IN ('percent', 'fixed_sats', 'set_price', 'free_license')),
    CHECK (amount >= 0),
    CHECK (used_count >= 0)
);

INSERT INTO discount_codes_new
SELECT id, code, kind, amount, max_uses, used_count, expires_at,
       applies_to_product_id, applies_to_policy_id, referrer_label,
       description, active, created_at, updated_at
FROM discount_codes;

DROP TABLE discount_codes;
ALTER TABLE discount_codes_new RENAME TO discount_codes;

CREATE INDEX IF NOT EXISTS idx_discount_codes_active   ON discount_codes(active);
CREATE INDEX IF NOT EXISTS idx_discount_codes_product  ON discount_codes(applies_to_product_id);
CREATE INDEX IF NOT EXISTS idx_discount_codes_policy   ON discount_codes(applies_to_policy_id);
CREATE INDEX IF NOT EXISTS idx_discount_codes_expires  ON discount_codes(expires_at);

-- ---------------------------------------------------------------------------
-- 4. Recreate discount_redemptions (same shape as 0004) and restore data.
-- ---------------------------------------------------------------------------
CREATE TABLE discount_redemptions (
    id                      TEXT PRIMARY KEY,
    code_id                 TEXT NOT NULL,
    invoice_id              TEXT NOT NULL,
    license_id              TEXT,
    status                  TEXT NOT NULL,
    discount_applied_sats   INTEGER NOT NULL,
    base_price_sats         INTEGER NOT NULL,
    final_price_sats        INTEGER NOT NULL,
    created_at              TEXT NOT NULL,
    updated_at              TEXT NOT NULL,
    FOREIGN KEY (code_id)    REFERENCES discount_codes(id),
    FOREIGN KEY (invoice_id) REFERENCES invoices(id),
    FOREIGN KEY (license_id) REFERENCES licenses(id),
    CHECK (status IN ('pending', 'redeemed', 'cancelled'))
);

INSERT INTO discount_redemptions SELECT * FROM _dr_stash;
DROP TABLE _dr_stash;

CREATE INDEX IF NOT EXISTS idx_discount_redemptions_code    ON discount_redemptions(code_id);
CREATE INDEX IF NOT EXISTS idx_discount_redemptions_invoice ON discount_redemptions(invoice_id);
CREATE INDEX IF NOT EXISTS idx_discount_redemptions_license ON discount_redemptions(license_id);
CREATE INDEX IF NOT EXISTS idx_discount_redemptions_status  ON discount_redemptions(status);
CREATE UNIQUE INDEX IF NOT EXISTS idx_discount_redemptions_one_per_invoice
    ON discount_redemptions(invoice_id);
