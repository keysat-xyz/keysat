-- Migration 0017: featured discount codes
--
-- Adds a `featured` flag to discount_codes. A "featured" discount is one
-- the operator wants prominently displayed on the buy page — typically a
-- launch promotion or a time-limited deal — rather than a normal discount
-- code that requires the buyer to type it in.
--
-- Effect when featured = true:
--   - The buy page renders the policy's original price struck through
--     plus the discounted price, with a "LAUNCH SPECIAL" diagonal
--     corner ribbon and the discount tagline.
--   - The purchase endpoint auto-applies the discount when no `code`
--     query param / body field is supplied. Buyers who type a different
--     code in the form get that code instead (operator-typed codes win
--     over auto-featured codes).
--
-- Activation/eligibility rules are unchanged from non-featured codes:
-- `active = 1` AND not expired AND below max_uses. So when a featured
-- code exhausts its 100-use cap, the buy page automatically stops
-- showing the launch-special ribbon and reverts to the standard
-- non-discounted price.

ALTER TABLE discount_codes ADD COLUMN featured INTEGER NOT NULL DEFAULT 0;

-- Partial index: featured codes only. Lookups for "the active featured
-- discount that applies to this policy" hit this index instead of
-- scanning every discount code. Tiny table either way today, but the
-- pattern scales.
CREATE INDEX IF NOT EXISTS idx_discount_codes_featured
  ON discount_codes(applies_to_policy_id, applies_to_product_id, featured)
  WHERE featured = 1 AND active = 1;
