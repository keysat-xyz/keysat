-- Allow `kind = 'set_price'` on discount_codes (added at the daemon
-- level in v0.1.0:26 but the migration that created the CHECK constraint
-- in 0004 didn't include it, so existing instances reject the new kind
-- with "CHECK constraint failed").
--
-- SQLite doesn't support ALTER TABLE ... DROP CONSTRAINT, so we rebuild
-- the table: copy → drop old → rename. sqlx-migrate already wraps each
-- .sql file in a transaction, so we DON'T do BEGIN/COMMIT here (nested
-- transactions are not supported in SQLite).
--
-- `PRAGMA defer_foreign_keys = 1` is the transaction-local equivalent
-- of `foreign_keys = OFF`: it postpones FK constraint checks until
-- COMMIT time. This lets us drop the old discount_codes table without
-- the immediate FK check from discount_redemptions.code_id failing.
-- The IDs are preserved across the rebuild, so when the FK check runs
-- at COMMIT, every referencing row still resolves cleanly.

PRAGMA defer_foreign_keys = 1;

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
