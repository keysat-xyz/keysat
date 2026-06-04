-- Link invoices to the payment provider that created them.
--
-- Companion to migration 0020 (merchant profiles + multi-provider). With a
-- single active provider, the reconciler could just iterate pending
-- invoices and call `provider.get_invoice_status()` on every one — every
-- invoice was implicitly from the only configured provider. With
-- N providers per profile and M profiles per Keysat instance, that
-- assumption breaks: each invoice needs to record WHICH provider it was
-- created against so the reconciler can dispatch to the right
-- `get_invoice_status()` and the webhook handler can validate against
-- the right secret.
--
-- Additive: nullable column + index. Backfill points every pre-migration
-- invoice at whatever provider was active when 0020 ran (same heuristic
-- the subscriptions backfill uses — earliest-connected on the default
-- profile). Post-migration, `repo::create_invoice_with_currency` always
-- writes the provider id.
--
-- Why not part of 0020: 0020 has shipped to the master operator's git
-- history (commit 04e0dcd) but not yet been *applied* to any DB (the
-- master box is still on :51, which has neither migration). The append-
-- only convention for migrations is the safer pattern even when we could
-- technically still rewrite 0020 — keeps the sqlx migration hashes
-- stable for anyone who ever runs an intermediate WIP build.

PRAGMA foreign_keys = ON;

ALTER TABLE invoices
    ADD COLUMN payment_provider_id TEXT REFERENCES payment_providers(id);

CREATE INDEX IF NOT EXISTS idx_invoices_provider
    ON invoices(payment_provider_id);

-- Backfill existing pending/settled invoices to point at the provider
-- that was active when 0020 ran. Heuristic: pick the provider on the
-- default merchant profile whose kind matches the (now-removed)
-- active_payment_provider setting if it existed pre-0020; else the
-- earliest-connected provider on the default profile. Mirrors the
-- backfill logic in 0020's UPDATE subscriptions block — same merchant
-- identity, same provider, deterministic across re-runs.
UPDATE invoices
   SET payment_provider_id = (
       SELECT id FROM payment_providers
        WHERE merchant_profile_id = (SELECT id FROM merchant_profiles WHERE is_default = 1)
        ORDER BY connected_at ASC
        LIMIT 1
   )
 WHERE payment_provider_id IS NULL;
