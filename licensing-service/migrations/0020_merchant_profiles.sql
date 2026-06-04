-- Multi-merchant-profile + multi-provider model.
--
-- Replaces the singleton btcpay_config + zaprite_config + SETTING_ACTIVE_PROVIDER
-- pattern with a generalized two-table model:
--
--   merchant_profiles    — one row per business identity (brand, redirect,
--                          optional SMTP override). Creator tier: 1 profile.
--                          Pro/Patron: unlimited.
--   payment_providers    — one row per configured BTCPay/Zaprite account,
--                          attached to a merchant profile via FK. A profile
--                          can have multiple providers (BTCPay for Bitcoin
--                          AND Zaprite for card). Unique per (profile, kind).
--
-- Products and subscriptions both get a merchant_profile_id column;
-- subscriptions additionally snapshot the payment_provider_id at creation
-- so mid-cycle product edits don't redirect existing buyers to a different
-- merchant or payment account.
--
-- One-way migration: drops btcpay_config + zaprite_config + the
-- active_payment_provider setting after porting their data into the new
-- tables. The master operator (the only person running Keysat today) needs
-- one post-migration manual step: update the Zaprite webhook URL on the
-- Zaprite dashboard to the new `/v1/zaprite/webhook/{provider_id}` form,
-- or click "Reconnect Zaprite" in the new admin UI to have Keysat
-- re-register the webhook with the correct URL automatically.

PRAGMA foreign_keys = ON;

-- ---------------------------------------------------------------------------
-- merchant_profiles: business identity layer
-- ---------------------------------------------------------------------------
-- Each profile represents one "business" the operator is running on this
-- Keysat instance. Owns its own brand block, support contact, post-purchase
-- redirect URL, and optionally an SMTP override (paired with the
-- keysat-smtp-emails plan — the columns are added now so the SMTP work
-- layers on cleanly later without another schema migration).
--
-- Tier gating is enforced at the Rust layer (`merchant_profiles::create`
-- checks the operator's tier and refuses with AppError::TierCap if a
-- Creator already has one profile). No CHECK at the schema layer because
-- tier resolution requires reading the operator's signed license, not just
-- counting rows.
CREATE TABLE IF NOT EXISTS merchant_profiles (
    id                          TEXT PRIMARY KEY,           -- UUID v4
    name                        TEXT NOT NULL,              -- "Recaps", "Keysat"
    legal_name                  TEXT,                       -- optional, for receipts/tax
    support_url                 TEXT,
    support_email               TEXT,
    brand_color                 TEXT,                       -- hex, e.g. '#1E3A5F'
    post_purchase_redirect_url  TEXT,                       -- NULL = Keysat's /thank-you
    is_default                  INTEGER NOT NULL DEFAULT 0,

    -- Per-profile SMTP override. NULL = inherit StartOS-level SMTP config.
    -- See keysat-smtp-emails.md for the email-sending plan that consumes
    -- these. Added in this migration so the SMTP plan doesn't need its
    -- own migration to add per-profile branding fields.
    smtp_host                   TEXT,
    smtp_port                   INTEGER,
    smtp_username               TEXT,
    smtp_password               TEXT,                       -- TODO: encryption at rest
    smtp_from_address           TEXT,
    smtp_from_name              TEXT,
    smtp_use_starttls           INTEGER NOT NULL DEFAULT 1,

    created_at                  TEXT NOT NULL,
    updated_at                  TEXT NOT NULL,
    CHECK (is_default IN (0, 1)),
    CHECK (smtp_use_starttls IN (0, 1))
);

-- Exactly one default profile. Partial unique index enforces this without
-- needing a trigger; updates to is_default must clear the previous default
-- in the same transaction (Rust layer handles this).
CREATE UNIQUE INDEX IF NOT EXISTS idx_merchant_profiles_one_default
    ON merchant_profiles(is_default) WHERE is_default = 1;

-- ---------------------------------------------------------------------------
-- payment_providers: replaces btcpay_config + zaprite_config singletons
-- ---------------------------------------------------------------------------
-- One row per configured payment account. Multiple rows allowed per
-- profile, but at most one of each `kind` (no two BTCPay stores on the
-- same business — operators wanting that should split into two profiles).
CREATE TABLE IF NOT EXISTS payment_providers (
    id                  TEXT PRIMARY KEY,                   -- UUID v4
    merchant_profile_id TEXT NOT NULL REFERENCES merchant_profiles(id),
    kind                TEXT NOT NULL,                      -- 'btcpay' | 'zaprite'
    label               TEXT NOT NULL,                      -- operator-set, e.g. "Recaps BTCPay"
    api_key             TEXT NOT NULL,
    base_url            TEXT NOT NULL,
    webhook_id          TEXT,                               -- provider-side webhook id, for delete on disconnect
    webhook_secret      TEXT,                               -- BTCPay HMAC secret; NULL for Zaprite
    store_id            TEXT,                               -- BTCPay only
    connected_at        TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    CHECK (kind IN ('btcpay', 'zaprite'))
);

CREATE INDEX IF NOT EXISTS idx_payment_providers_profile
    ON payment_providers(merchant_profile_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_payment_providers_profile_kind
    ON payment_providers(merchant_profile_id, kind);

-- ---------------------------------------------------------------------------
-- merchant_profile_rail_preferences: tie-breaker for multi-provider profiles
-- ---------------------------------------------------------------------------
-- When a profile has 2 providers that BOTH serve the same payment rail
-- (e.g., both BTCPay and Zaprite can settle Lightning), the operator picks
-- which provider serves that rail for THIS profile here. Without an entry,
-- the routing layer picks the provider with the earliest connected_at
-- (deterministic but warns in the admin UI).
--
-- Rails-per-kind are inherent (BTCPay → Lightning + OnChain; Zaprite →
-- Card + Lightning + OnChain) — declared via the trait method
-- `served_rails()` in Rust, not stored per provider row. This table
-- is purely the ambiguity resolver.
CREATE TABLE IF NOT EXISTS merchant_profile_rail_preferences (
    merchant_profile_id TEXT NOT NULL REFERENCES merchant_profiles(id),
    rail                TEXT NOT NULL,                      -- 'lightning' | 'onchain' | 'card'
    payment_provider_id TEXT NOT NULL REFERENCES payment_providers(id),
    PRIMARY KEY (merchant_profile_id, rail),
    CHECK (rail IN ('lightning', 'onchain', 'card'))
);

-- ---------------------------------------------------------------------------
-- products: attach to a merchant profile
-- ---------------------------------------------------------------------------
-- Nullable during the data-port window (we set it in the UPDATE below).
-- After backfill the Rust create_product path requires it (enforced at
-- the application layer; can't add NOT NULL via ALTER on SQLite).
ALTER TABLE products
    ADD COLUMN merchant_profile_id TEXT REFERENCES merchant_profiles(id);
CREATE INDEX IF NOT EXISTS idx_products_profile
    ON products(merchant_profile_id);

-- ---------------------------------------------------------------------------
-- subscriptions: snapshot profile + provider at creation
-- ---------------------------------------------------------------------------
-- The snapshot semantics matter: if an operator later edits a product to
-- attach a different profile / point at a different provider, existing
-- subscriptions keep renewing through their ORIGINAL profile + provider.
-- Re-routing an existing sub to a new merchant is a deliberate admin
-- action, never an automatic consequence of editing a product.
ALTER TABLE subscriptions
    ADD COLUMN merchant_profile_id TEXT REFERENCES merchant_profiles(id);
ALTER TABLE subscriptions
    ADD COLUMN payment_provider_id TEXT REFERENCES payment_providers(id);
CREATE INDEX IF NOT EXISTS idx_subs_profile
    ON subscriptions(merchant_profile_id);
CREATE INDEX IF NOT EXISTS idx_subs_provider
    ON subscriptions(payment_provider_id);

-- ---------------------------------------------------------------------------
-- Data port: singletons → multi-row tables
-- ---------------------------------------------------------------------------

-- 1. Create the default merchant profile. Name = the operator_name setting
--    if present; else 'Keysat'. UUID-style id via SQLite's randomblob hex.
INSERT INTO merchant_profiles(
    id, name, support_url, support_email, brand_color,
    post_purchase_redirect_url, is_default, created_at, updated_at
)
SELECT
    lower(hex(randomblob(16))),
    COALESCE((SELECT value FROM settings WHERE key = 'operator_name'), 'Keysat'),
    NULL, NULL, NULL, NULL,
    1,
    datetime('now'),
    datetime('now')
WHERE NOT EXISTS (SELECT 1 FROM merchant_profiles WHERE is_default = 1);

-- 2. Port btcpay_config (if a row exists) into payment_providers, attached
--    to the default profile.
INSERT INTO payment_providers(
    id, merchant_profile_id, kind, label,
    api_key, base_url, webhook_id, webhook_secret, store_id,
    connected_at, updated_at
)
SELECT
    lower(hex(randomblob(16))),
    (SELECT id FROM merchant_profiles WHERE is_default = 1),
    'btcpay',
    'BTCPay (migrated)',
    api_key, base_url, webhook_id, webhook_secret, store_id,
    connected_at, connected_at
FROM btcpay_config;

-- 3. Port zaprite_config (if a row exists). Zaprite has no webhook_secret
--    or store_id; map both to NULL.
INSERT INTO payment_providers(
    id, merchant_profile_id, kind, label,
    api_key, base_url, webhook_id, webhook_secret, store_id,
    connected_at, updated_at
)
SELECT
    lower(hex(randomblob(16))),
    (SELECT id FROM merchant_profiles WHERE is_default = 1),
    'zaprite',
    'Zaprite (migrated)',
    api_key, base_url, webhook_id, NULL, NULL,
    connected_at, connected_at
FROM zaprite_config;

-- 4. Backfill existing products to point at the default profile.
UPDATE products
   SET merchant_profile_id = (SELECT id FROM merchant_profiles WHERE is_default = 1)
 WHERE merchant_profile_id IS NULL;

-- 5. Backfill existing subscriptions. Pick the provider whose kind matches
--    SETTING_ACTIVE_PROVIDER if set; otherwise pick the earliest-connected
--    provider on the default profile (deterministic). Subs sitting on a
--    provider that no longer exists in payment_providers (extremely
--    unlikely — would require corrupted singleton data) are left NULL
--    and the operator's admin UI will flag them.
UPDATE subscriptions
   SET merchant_profile_id = (SELECT id FROM merchant_profiles WHERE is_default = 1),
       payment_provider_id = (
           SELECT id FROM payment_providers
           WHERE merchant_profile_id = (SELECT id FROM merchant_profiles WHERE is_default = 1)
             AND kind = COALESCE(
                 (SELECT value FROM settings WHERE key = 'active_payment_provider'),
                 (SELECT kind FROM payment_providers
                  WHERE merchant_profile_id = (SELECT id FROM merchant_profiles WHERE is_default = 1)
                  ORDER BY connected_at ASC
                  LIMIT 1)
             )
       )
 WHERE merchant_profile_id IS NULL OR payment_provider_id IS NULL;

-- 6. Drop the singleton tables + the active-provider setting. Now the only
--    source of truth for payment configuration is payment_providers +
--    merchant_profiles.
DROP TABLE IF EXISTS btcpay_config;
DROP TABLE IF EXISTS zaprite_config;
DELETE FROM settings WHERE key = 'active_payment_provider';

-- Note: btcpay_authorize_state stays (it's the in-flight OAuth CSRF
-- token table from migration 0002; nothing to migrate, just continues
-- to scope per-attempt). Its `state_token` rows will now carry a
-- `merchant_profile_id` in their associated payload — see the
-- btcpay_authorize.rs changes that add this column in a future
-- micro-migration if needed (today the state token is opaque to the
-- DB and the profile id is round-tripped via the OAuth state param).
