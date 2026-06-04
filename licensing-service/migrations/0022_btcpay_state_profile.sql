-- Carry merchant_profile_id through the BTCPay OAuth round trip.
--
-- Operator hits POST /v1/admin/btcpay/connect with a merchant_profile_id,
-- daemon generates a CSRF state token and stores it; operator opens
-- BTCPay's authorize URL in their browser; BTCPay POSTs back to our
-- callback with the apiKey + the state token; daemon consumes the state
-- token and uses it to look up which merchant profile the new provider
-- row should attach to.
--
-- Pre-multi-provider, `btcpay_authorize_state` was a singleton-ish
-- pattern (one in-flight authorize at a time) and the resulting provider
-- always attached to "the singleton btcpay_config row." With multi-
-- profile, the operator might want to authorize a SECOND BTCPay store
-- onto a different profile (Pro/Patron); the state token has to
-- remember which profile they kicked off the flow from.
--
-- Additive: nullable column, NULL = "attach to the default profile"
-- (back-compat for any pre-:52 state tokens that survived a daemon
-- restart mid-flow, though the table is also pruned by timestamp).

PRAGMA foreign_keys = ON;

ALTER TABLE btcpay_authorize_state
    ADD COLUMN merchant_profile_id TEXT REFERENCES merchant_profiles(id);
