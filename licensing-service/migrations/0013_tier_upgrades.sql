-- Tier upgrades: schema foundation.
--
-- This migration adds the storage shape needed for in-place tier
-- upgrades + downgrades on existing licenses (Standard → Pro,
-- Trial → Standard, etc.). Daemon code that USES these columns +
-- table lands in subsequent commits per TIER_UPGRADES_DESIGN.md
-- Phases 2-6.
--
-- Strategy: additive only. Existing licenses + policies are
-- untouched. A policy becomes "part of the tier ladder" by getting
-- a `tier_rank` value; policies with NULL tier_rank are excluded
-- from buyer-facing upgrade flows (admin can still force-change
-- to/from any policy). This means existing operators who don't
-- want tier upgrades can ignore the feature entirely — none of
-- their policies are in any ladder until they opt in by setting
-- a rank.

PRAGMA foreign_keys = ON;

-- ---------------------------------------------------------------------------
-- policies: tier_rank for ladder ordering
-- ---------------------------------------------------------------------------
-- Operator-defined ordering. Higher rank = better tier. A product
-- can have policies "free" (rank 0), "standard" (rank 1), "pro"
-- (rank 2), "patron" (rank 3). The tier-upgrade endpoint validates
-- that target.tier_rank > current.tier_rank for upgrades, and the
-- reverse for downgrades. NULL = excluded from the buyer-facing
-- ladder (e.g. limited-edition promo policy that shouldn't appear
-- as an upgrade target).
--
-- We don't enforce uniqueness within a product — operators can
-- legitimately have two policies at the same rank (e.g. "Pro
-- Monthly" and "Pro Annual" both at rank=2 — same entitlements,
-- different cadence). Sideways changes between same-rank policies
-- are admin-only; the buyer endpoint rejects them.
ALTER TABLE policies ADD COLUMN tier_rank INTEGER;

-- Index supports the common "list this product's policies in
-- ladder order" query used by both the admin tier-rank picker and
-- the buyer-side tier listing.
CREATE INDEX IF NOT EXISTS idx_policies_tier_rank
    ON policies(product_id, tier_rank);

-- ---------------------------------------------------------------------------
-- tier_changes: audit trail of every tier change ever applied
-- ---------------------------------------------------------------------------
-- One row per upgrade or downgrade. The `licenses.policy_id` column
-- still holds the CURRENT tier; this table is the history. Operators
-- can answer "what tier was this license on as of date X" by walking
-- tier_changes ordered by created_at; combined with
-- effective_at, "is the license currently entitled to <X>" is also a
-- cheap lookup against licenses.policy_id alone (no walk needed).
--
-- effective_at is decoupled from created_at for downgrades on
-- recurring subs: the downgrade is RECORDED immediately (created_at)
-- but doesn't TAKE EFFECT until the end of the current cycle
-- (effective_at = cycle_end). For upgrades, effective_at usually
-- equals created_at (immediate on payment settle).
CREATE TABLE IF NOT EXISTS tier_changes (
    id              TEXT PRIMARY KEY,           -- UUID v4
    license_id      TEXT NOT NULL,
    from_policy_id  TEXT NOT NULL,
    to_policy_id    TEXT NOT NULL,
    direction       TEXT NOT NULL,              -- 'upgrade' | 'downgrade'

    -- Pricing snapshot. The proration math (and the rate fetcher
    -- for fiat conversions) runs at quote time and is frozen here
    -- once the change is applied. For comp-mode admin changes
    -- (skip_payment=true), proration_charge_value is 0 and
    -- invoice_id is NULL.
    listed_currency        TEXT NOT NULL,       -- 'SAT' | 'USD' | 'EUR'
    proration_charge_value INTEGER NOT NULL DEFAULT 0,  -- smallest unit of listed_currency
    invoice_id             TEXT,                 -- nullable: 0-charge changes have no invoice

    -- When the new entitlements take effect. For upgrades on
    -- recurring subs OR perpetual: typically same as created_at.
    -- For downgrades on recurring subs: end of current cycle.
    effective_at    TEXT NOT NULL,

    -- Audit. 'buyer' = self-service via /v1/upgrade.
    --        'admin' = operator action via /v1/admin/licenses/:id/change-tier.
    actor           TEXT NOT NULL,
    -- Optional free-form note. Audit-only; not user-visible. The
    -- admin endpoint accepts a `reason` field that lands here.
    reason          TEXT,

    created_at      TEXT NOT NULL,

    FOREIGN KEY (license_id)     REFERENCES licenses(id),
    FOREIGN KEY (from_policy_id) REFERENCES policies(id),
    FOREIGN KEY (to_policy_id)   REFERENCES policies(id),
    FOREIGN KEY (invoice_id)     REFERENCES invoices(id),
    CHECK (direction IN ('upgrade', 'downgrade')),
    CHECK (actor IN ('buyer', 'admin')),
    CHECK (proration_charge_value >= 0)
);

-- Admin-UI "show me this license's tier history" query path.
CREATE INDEX IF NOT EXISTS idx_tier_changes_license
    ON tier_changes(license_id, created_at);
-- Operator analytics: "how many upgrades happened this month?"
CREATE INDEX IF NOT EXISTS idx_tier_changes_created
    ON tier_changes(created_at);
-- Webhook-handler lookup: an invoice settles, we need to know
-- whether it's a tier-change invoice (vs a fresh purchase or a
-- subscription renewal).
CREATE INDEX IF NOT EXISTS idx_tier_changes_invoice
    ON tier_changes(invoice_id) WHERE invoice_id IS NOT NULL;

-- ---------------------------------------------------------------------------
-- Note: no CHECK constraint enforcing that tier_rank is set on
-- policies that participate in upgrade flows. The check lives in
-- the API handler (api/upgrade.rs, future commit) because:
--   1. SQLite ALTER TABLE doesn't support adding CHECKs.
--   2. NULL tier_rank is a valid state for "this policy isn't in
--      any ladder" — there's nothing to enforce at the row level.
--   3. The semantic check ("you can't upgrade to a policy with
--      NULL tier_rank") is a cross-row invariant the API layer
--      handles cleanly with a single SELECT.
