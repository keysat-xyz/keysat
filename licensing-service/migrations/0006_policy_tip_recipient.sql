-- Migration 0006: tip-recipient on policy.
--
-- Lets the operator configure a Lightning recipient + percentage on each
-- policy. When a license issued under that policy settles, the daemon
-- tries to send a Lightning tip of (license_price_sats * tip_pct_bps / 10000)
-- to tip_recipient via the operator's BTCPay Lightning node.
--
-- All three fields are nullable / zero-default. Existing policies are
-- unaffected: with NULL recipient the issuance hook is a no-op.
--
-- Recipient can be a Lightning Address (e.g. tip@keysat.xyz). LNURL-pay
-- support may be added later; the current implementation resolves only
-- Lightning Addresses via the .well-known/lnurlp/<user> endpoint.

ALTER TABLE policies ADD COLUMN tip_recipient TEXT;
ALTER TABLE policies ADD COLUMN tip_pct_bps INTEGER NOT NULL DEFAULT 0;
ALTER TABLE policies ADD COLUMN tip_label TEXT;

-- Audit log for tip attempts. Insert one row per try, success or failure.
-- Operators consult this for accounting and for debugging when a tip
-- doesn't fire as expected.
CREATE TABLE IF NOT EXISTS tip_attempts (
    id TEXT PRIMARY KEY,
    license_id TEXT NOT NULL,
    policy_id TEXT NOT NULL,
    recipient TEXT NOT NULL,
    amount_sats INTEGER NOT NULL,
    pct_bps INTEGER NOT NULL,
    label TEXT,
    -- 'sent' | 'failed' | 'skipped' (e.g. zero amount, no LN node)
    status TEXT NOT NULL,
    -- Error or success detail message.
    detail TEXT,
    -- Lightning payment hash on success, null on failure.
    payment_hash TEXT,
    created_at TEXT NOT NULL,
    FOREIGN KEY (license_id) REFERENCES licenses(id),
    FOREIGN KEY (policy_id)  REFERENCES policies(id)
);

CREATE INDEX IF NOT EXISTS idx_tip_attempts_license ON tip_attempts(license_id);
CREATE INDEX IF NOT EXISTS idx_tip_attempts_recipient ON tip_attempts(recipient);
CREATE INDEX IF NOT EXISTS idx_tip_attempts_created ON tip_attempts(created_at);
