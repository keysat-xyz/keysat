-- Carry the connect *initiator* through the BTCPay OAuth round trip.
--
-- agent-payment-connect (plans/agent-payment-connect-scope.md): a scoped key
-- bearing `payment_providers:write` may start a BTCPay connect, but only on a
-- sandbox daemon (outer gate) AND only for a non-mainnet store (inner gate).
-- The inner gate can only be evaluated at callback time — that's the first
-- moment we know the store and can resolve its network. So the connect handler
-- must remember, across the operator's browser round-trip to BTCPay, whether
-- the initiator was the master key (may connect any network) or a scoped key
-- (restricted to non-mainnet).
--
-- `scoped_initiator`: 0 = master (no network restriction), 1 = scoped key
--   (callback enforces non-mainnet, fail-closed). Default 0 keeps any in-flight
--   pre-upgrade state token behaving as a master connect (the only kind that
--   existed before this migration).
-- `initiator_actor_hash`: sha256 of the initiating credential, so the callback
--   can write an audit row attributing the scoped connect without a header.
--
-- Additive, one-way (consistent with 0020-0022). The table is also pruned by
-- timestamp, so any pre-migration rows expire within 30 minutes regardless.

PRAGMA foreign_keys = ON;

ALTER TABLE btcpay_authorize_state
    ADD COLUMN scoped_initiator INTEGER NOT NULL DEFAULT 0;

ALTER TABLE btcpay_authorize_state
    ADD COLUMN initiator_actor_hash TEXT;
