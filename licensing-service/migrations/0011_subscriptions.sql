-- Recurring subscriptions: schema foundation.
--
-- This migration adds the storage shape needed for recurring-billing
-- licenses. Daemon code that USES these tables lands in subsequent
-- commits (renewal worker, validate-hot-path subscription branch,
-- admin endpoints, buy-page recurring rendering). Per the
-- RECURRING_SUBSCRIPTIONS_DESIGN.md doc at the repo root.
--
-- Strategy: additive only. Existing one-shot purchase flows are
-- untouched. A license becomes "subscription-backed" when its
-- `licenses` row gets a corresponding `subscriptions` row (1:1
-- via `subscriptions.license_id UNIQUE`). One-shot purchases just
-- never get a subscriptions row, behave exactly as before.
--
-- Decisions encoded here that depend on Grant's design-doc
-- answers (placeholders are best-guess defaults; can be tuned via
-- ALTER COLUMN DEFAULT in a follow-up if the answers differ):
--   - grace_period_days default: 7
--   - trial_days column on policies: included from day 1
--     (cheaper to ship now than to migrate later)
--   - cancellation refund: NOT a schema concern (no refund column;
--     mid-cycle cancellation just stops next charge, license
--     stays valid through current cycle, no DB change)

PRAGMA foreign_keys = ON;

-- ---------------------------------------------------------------------------
-- policies: recurring + trial flags
-- ---------------------------------------------------------------------------
-- A policy with `is_recurring = 1` issues subscription-backed
-- licenses. `renewal_period_days` is required when is_recurring=1
-- (CHECK enforced); NULL otherwise. `grace_period_days` applies
-- only to recurring policies.
--
-- `trial_days` is independent of is_recurring — even one-shot
-- products can have a "free 14 days, then key revokes" trial flow,
-- though most operators will use trials only on recurring policies.
ALTER TABLE policies ADD COLUMN is_recurring        INTEGER NOT NULL DEFAULT 0;
ALTER TABLE policies ADD COLUMN renewal_period_days INTEGER;
ALTER TABLE policies ADD COLUMN grace_period_days   INTEGER NOT NULL DEFAULT 7;
ALTER TABLE policies ADD COLUMN trial_days          INTEGER NOT NULL DEFAULT 0;

-- Helps the renewal worker filter policies cheaply.
CREATE INDEX IF NOT EXISTS idx_policies_recurring ON policies(is_recurring);

-- ---------------------------------------------------------------------------
-- subscriptions: one row per subscription-backed license
-- ---------------------------------------------------------------------------
-- The license row remains the source of truth for entitlements +
-- product/policy linkage. The subscription row tracks the cycle
-- state machine that determines whether the license is currently
-- valid (active / past_due-with-grace) or not (lapsed / cancelled).
--
-- Pricing snapshot fields (listed_currency / listed_value /
-- period_days) are frozen at subscription creation. Operators
-- changing the underlying policy's price doesn't affect existing
-- subscriptions; the next renewal still bills the snapshotted
-- amount. To migrate existing subscribers to a new price, the
-- operator either re-issues the license at the new policy
-- (admin action) or waits for natural cancellation + repurchase.
CREATE TABLE IF NOT EXISTS subscriptions (
    id                      TEXT PRIMARY KEY,           -- UUID v4
    license_id              TEXT NOT NULL UNIQUE,       -- 1:1 with licenses
    policy_id               TEXT NOT NULL,              -- denormalized; renewal worker reads it
    product_id              TEXT NOT NULL,              -- denormalized for cheap admin filters

    -- Cycle schedule. Frozen at subscription creation.
    period_days             INTEGER NOT NULL,

    -- Pricing snapshot. Frozen at subscription creation; see comment above.
    -- For SAT-currency subs: same value charged each cycle (no rate fluctuation).
    -- For USD/EUR subs: listed_value is stable in the listed currency, BUT
    -- the BTC amount drifts each cycle as the rate fetcher re-quotes
    -- (this is the USD-stable / re-quote-each-cycle decision from
    --  MULTI_CURRENCY_DESIGN.md).
    listed_currency         TEXT NOT NULL,
    listed_value            INTEGER NOT NULL,           -- smallest unit of listed_currency

    -- Lifecycle state machine. See RECURRING_SUBSCRIPTIONS_DESIGN.md
    -- for the full diagram. CHECK enforces only valid values can ever
    -- land in the column.
    status                  TEXT NOT NULL,
    started_at              TEXT NOT NULL,              -- ISO-8601 UTC
    next_renewal_at         TEXT,                       -- when the worker creates the next invoice; NULL once cancelled-and-past-cycle
    cancelled_at            TEXT,
    cancellation_reason     TEXT,

    -- Audit / dunning state. consecutive_failures backs off the renewal
    -- worker on repeated failure (see RECURRING_SUBSCRIPTIONS_DESIGN.md
    -- §"Renewal worker"). last_renewal_attempt_at lets the admin UI
    -- show "we tried 3 hours ago, will retry in 9 hours."
    last_renewal_attempt_at TEXT,
    consecutive_failures    INTEGER NOT NULL DEFAULT 0,

    created_at              TEXT NOT NULL,
    updated_at              TEXT NOT NULL,

    FOREIGN KEY (license_id) REFERENCES licenses(id),
    FOREIGN KEY (policy_id)  REFERENCES policies(id),
    FOREIGN KEY (product_id) REFERENCES products(id),
    CHECK (status IN ('active', 'past_due', 'cancelled', 'lapsed')),
    CHECK (period_days > 0),
    CHECK (consecutive_failures >= 0)
);

-- Renewal-worker query path: "find subs that are due to bill next."
-- Partial index on the only states that ever produce work.
CREATE INDEX IF NOT EXISTS idx_subs_next_renewal
    ON subscriptions(next_renewal_at)
    WHERE status IN ('active', 'past_due');

-- Admin-UI search paths.
CREATE INDEX IF NOT EXISTS idx_subs_status   ON subscriptions(status);
CREATE INDEX IF NOT EXISTS idx_subs_license  ON subscriptions(license_id);
CREATE INDEX IF NOT EXISTS idx_subs_policy   ON subscriptions(policy_id);

-- ---------------------------------------------------------------------------
-- subscription_invoices: one row per renewal-cycle invoice
-- ---------------------------------------------------------------------------
-- Joins subscriptions to the existing invoices table so we can ask
-- "show me all the invoices for this subscription." Why a separate
-- table rather than a column on `invoices`: most invoices are NOT
-- subscription-related (one-shot purchases). A nullable FK column
-- on invoices works, but a separate join table keeps subscription-
-- specific metadata (cycle_number, cycle_start_at, cycle_end_at) out
-- of the main invoices schema and makes "list all sub invoices for
-- a license" a clean two-table join.
CREATE TABLE IF NOT EXISTS subscription_invoices (
    id              TEXT PRIMARY KEY,           -- UUID v4
    subscription_id TEXT NOT NULL,
    invoice_id      TEXT NOT NULL,              -- FK into invoices(id)
    cycle_number    INTEGER NOT NULL,           -- 1, 2, 3, ... per subscription
    cycle_start_at  TEXT NOT NULL,              -- ISO-8601 UTC; begin of the period this invoice covers
    cycle_end_at    TEXT NOT NULL,              -- ISO-8601 UTC; end of the period
    created_at      TEXT NOT NULL,
    FOREIGN KEY (subscription_id) REFERENCES subscriptions(id),
    FOREIGN KEY (invoice_id)      REFERENCES invoices(id),
    UNIQUE (subscription_id, cycle_number),
    CHECK (cycle_number >= 1)
);

CREATE INDEX IF NOT EXISTS idx_sub_invoices_sub
    ON subscription_invoices(subscription_id);
CREATE INDEX IF NOT EXISTS idx_sub_invoices_invoice
    ON subscription_invoices(invoice_id);

-- ---------------------------------------------------------------------------
-- Validation: recurring policies need a renewal period.
-- ---------------------------------------------------------------------------
-- We can't add this as a CHECK on the policies table because SQLite
-- ALTER TABLE doesn't support adding CHECKs (and rebuilding the
-- table here just to add one constraint is overkill given how
-- rarely operators flip is_recurring on existing rows). The repo
-- helper `create_policy_recurring` enforces it at write time
-- instead. A hypothetical future migration that rebuilds the
-- policies table for some other reason can add it then.
