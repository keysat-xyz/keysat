-- Migration 0015: policies.archived_at
--
-- Adds a soft-archive flag to policies. An archived policy is hidden
-- from the admin grid (unless the operator opts to show archived) and
-- from the public /buy/<slug> page. Existing licenses keep validating
-- because their entitlements are signed into the LIC1 payload; the
-- policy row is not consulted at validate time. Active recurring
-- subscriptions tied to an archived policy stop renewing — the renewal
-- worker treats archived as a hard stop and surfaces a clear event.
--
-- Why a column instead of a status TEXT enum? Policies already have
-- two boolean toggles (active, public). A nullable timestamp is the
-- minimum-information shape: NULL = live, timestamp = when archived.
-- Useful for sorting "Archived (most recent first)" without an extra
-- column.

ALTER TABLE policies ADD COLUMN archived_at TEXT NULL;

CREATE INDEX IF NOT EXISTS idx_policies_archived_at ON policies(archived_at);
