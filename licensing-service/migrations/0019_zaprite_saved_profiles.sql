-- Zaprite saved-payment-profile metadata for recurring subscriptions.
--
-- Wires up the auto-charge path that the v0.2.0:1+ subscriptions
-- module comment promised but never delivered: when a buyer pays the
-- FIRST cycle of a recurring subscription via Zaprite (Stripe card),
-- Keysat asks Zaprite to save the payment profile and persists the
-- profile id here. The renewal worker then calls
-- `POST /v1/orders/charge` against the saved profile instead of
-- waiting for the buyer to manually pay each renewal.
--
-- All four columns are nullable + nothing in the existing read path
-- requires them, so this migration is a pure additive drop-in:
--   - BTCPay subscriptions stay NULL on all four (BTCPay has no
--     equivalent concept; renewals continue to require manual pay).
--   - Pre-feature Zaprite subscriptions stay NULL — the renewal
--     worker falls through to the existing "buyer pays manually"
--     branch when `zaprite_payment_profile_id IS NULL`.
--   - Zaprite subscriptions whose buyer either paid with Bitcoin/
--     Lightning instead of card, OR declined the save-card prompt,
--     also stay NULL. Same fallback.
--
-- Decisions encoded here:
--   - `zaprite_contact_id`: needed because Zaprite's order endpoint
--     doesn't surface the profile id directly. After settle we fetch
--     the contact, find the profile whose `sourceOrder.externalUniqId`
--     matches our invoice id, and persist both.
--   - `zaprite_payment_profile_method` / `expires_at`: informational
--     only — the admin UI uses them to render "card ending 4242,
--     expires 03/27" on the subscription detail. The renewal worker
--     doesn't gate on either today; if Zaprite returns expired-card
--     errors on the auto-charge we fall through to manual pay and
--     log the failure, same as any other decline.

PRAGMA foreign_keys = ON;

ALTER TABLE subscriptions
    ADD COLUMN zaprite_contact_id TEXT;
ALTER TABLE subscriptions
    ADD COLUMN zaprite_payment_profile_id TEXT;
ALTER TABLE subscriptions
    ADD COLUMN zaprite_payment_profile_method TEXT;
ALTER TABLE subscriptions
    ADD COLUMN zaprite_payment_profile_expires_at TEXT;

-- Helps the admin-UI "subs with auto-charge configured" filter and
-- any future "subs whose saved card is about to expire" sweep.
CREATE INDEX IF NOT EXISTS idx_subs_zaprite_profile
    ON subscriptions(zaprite_payment_profile_id)
    WHERE zaprite_payment_profile_id IS NOT NULL;
