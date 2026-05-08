-- Tiered pricing UX (v0.1.0:27).
--
-- Two changes, both additive:
--
-- 1. Mark policies as buyer-visible. Operators may have policies they don't
--    want to render on the public /buy/<slug> page (e.g. "Comp / press
--    giveaway", "Internal team seat"). Defaults to public=1 so existing
--    policies keep their current behaviour.
--
-- 2. Remember which policy the buyer chose at purchase time. Today,
--    `issue_license_for_invoice` picks the "default" policy (or first
--    active) for the product. With multi-tier pricing, the buyer's
--    explicit choice needs to round-trip from /buy → BTCPay invoice →
--    settlement webhook → license issuance. Storing it on the invoice is
--    the simplest place — it sticks even if the policy is later
--    deactivated, and the FK keeps integrity. NULL means "fall back to
--    the product's default policy" for backwards compatibility with
--    pre-:27 invoices.

PRAGMA foreign_keys = ON;

ALTER TABLE policies ADD COLUMN public INTEGER NOT NULL DEFAULT 1;
ALTER TABLE invoices ADD COLUMN policy_id TEXT REFERENCES policies(id);

-- Helps the public buy-page endpoint enumerate visible tiers cheaply.
CREATE INDEX IF NOT EXISTS idx_policies_public ON policies(public);
