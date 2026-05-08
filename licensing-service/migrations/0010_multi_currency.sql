-- Multi-currency pricing foundation.
--
-- Adds the schema needed to price products + policies in something
-- other than satoshis (USD, EUR, BTC at higher denominations) while
-- keeping every existing operator's data behaviorally identical.
-- See MULTI_CURRENCY_DESIGN.md at the repo root for the full design;
-- this migration is its Phase 1.
--
-- Strategy: additive only. New columns get defaults that mean
-- "interpret me as SAT-priced, same as before." `price_sats` stays
-- as the canonical sat amount (dual-written from now on); the new
-- `price_currency` + `price_value` pair carries the operator-facing
-- intent. Daemon code reads either; new code prefers the new pair.
--
-- The new columns are intentionally not yet wired into the buy page
-- or admin UI. That's a v0.3 follow-up — this migration just gives
-- the daemon the storage shape so the read path can begin to use
-- it incrementally without further migrations.

PRAGMA foreign_keys = ON;

-- ---------------------------------------------------------------------------
-- products: native currency + value
-- ---------------------------------------------------------------------------
-- price_currency = ISO 4217 fiat code, 'SAT', or 'BTC'.
--   SAT  → smallest unit is 1 sat
--   BTC  → smallest unit is 1 sat (1 BTC = 100,000,000 sats)
--   USD  → smallest unit is 1 cent
--   EUR  → smallest unit is 1 cent
-- price_value is in the smallest indivisible unit of that currency.
-- For SAT-priced products, price_value == price_sats.
-- For USD-priced products, price_value is cents and `price_sats` is
-- a stale snapshot from purchase time (or 0 if the product has
-- never been migrated through dual-write).
ALTER TABLE products ADD COLUMN price_currency TEXT NOT NULL DEFAULT 'SAT';
ALTER TABLE products ADD COLUMN price_value INTEGER NOT NULL DEFAULT 0;

-- Backfill: every existing row is SAT-priced. Copy price_sats →
-- price_value so the new pair carries the same information.
UPDATE products SET price_value = price_sats WHERE price_currency = 'SAT';

-- ---------------------------------------------------------------------------
-- policies: optional per-tier currency override
-- ---------------------------------------------------------------------------
-- Mirrors the existing price_sats_override column. NULL on either
-- means "inherit from product"; both NULL is the common "this tier
-- uses the product's price as-is" case.
ALTER TABLE policies ADD COLUMN price_currency_override TEXT;
ALTER TABLE policies ADD COLUMN price_value_override INTEGER;

-- Backfill: existing policies that had a sat override get the new
-- pair filled in. Currency stays NULL (= use the parent product's
-- currency, which after the products backfill is SAT).
UPDATE policies SET price_value_override = price_sats_override
 WHERE price_sats_override IS NOT NULL;

-- ---------------------------------------------------------------------------
-- invoices: record listed price + exchange rate at creation
-- ---------------------------------------------------------------------------
-- For sat-priced flows, all four columns stay NULL.
-- For fiat-priced flows, listed_currency + listed_value carry what
-- the buyer SAW (e.g. USD 5000 cents = $50.00) and
-- exchange_rate_centibps + exchange_rate_source record HOW the
-- daemon converted it to the BTC amount the buyer was actually
-- billed (the existing amount_sats column).
--
-- exchange_rate_centibps stores the rate as
-- "<unit-of-listed-currency> per BTC, scaled by 10000".
-- For a $65,000/BTC market, listing a $50 product:
--   listed_currency = 'USD'
--   listed_value    = 5000 (cents)
--   exchange_rate_centibps = 650000000  (USD-cents per BTC, ×10000 = ×10^4)
--   amount_sats     = 76923  (5000 cents ÷ 65000 USD/BTC × 100M sats/BTC)
-- 10000-bp scaling gives ~6 decimal digits of rate precision — plenty
-- for fiat→BTC where rates are 5-6 figures. No floating-point ops.
ALTER TABLE invoices ADD COLUMN listed_currency TEXT;
ALTER TABLE invoices ADD COLUMN listed_value INTEGER;
ALTER TABLE invoices ADD COLUMN exchange_rate_centibps INTEGER;
ALTER TABLE invoices ADD COLUMN exchange_rate_source TEXT;
-- exchange_rate_source examples: 'btcpay' | 'kraken' | 'coinbase' |
-- 'coingecko' | 'manual_pin' (for testing). The actual source URL
-- + timestamp aren't stored here — the rate fetcher caches those
-- in-memory and exposes them via /v1/admin/rates (a v0.3+ surface).

-- ---------------------------------------------------------------------------
-- discount_codes: currency-aware fixed amounts
-- ---------------------------------------------------------------------------
-- 'percent' codes are currency-agnostic (basis points off whatever
-- the product is priced in) — no change needed.
-- 'fixed_sats' and 'set_price' need a currency tag to express
-- "$10 off" or "set price to $25" against fiat-priced products.
-- Add `discount_currency` with default 'SAT' so existing codes keep
-- their current semantics. v0.3 admin UI lets operators pick.
ALTER TABLE discount_codes ADD COLUMN discount_currency TEXT NOT NULL DEFAULT 'SAT';
-- amount column already exists; for SAT-currency codes it stays in
-- sats. For USD-currency codes it's cents. The kind+currency pair
-- determines interpretation:
--   kind=fixed_sats currency=SAT → amount sats off
--   kind=fixed_sats currency=USD → amount cents off (the column
--     name is now slightly stale but renaming requires a rebuild
--     and the existing value carries forward cleanly)
--   kind=set_price currency=SAT  → set price to amount sats
--   kind=set_price currency=USD  → set price to amount cents

-- ---------------------------------------------------------------------------
-- Indexes
-- ---------------------------------------------------------------------------
-- Operator search-by-currency is a future admin-UI feature; ship the
-- index now so it's available when the UI shows up.
CREATE INDEX IF NOT EXISTS idx_products_currency ON products(price_currency);
