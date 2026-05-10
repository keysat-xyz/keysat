-- Product-level entitlements catalog.
--
-- Before this migration, entitlements were free-text strings stored
-- per-policy as a JSON array. There was no shared vocabulary across
-- the policies of a single product, no display-name metadata, and no
-- way for the buy page to render anything but the raw slug
-- (`ai_summaries` instead of "AI summaries").
--
-- This migration introduces a per-product catalog: each product
-- declares the entitlements it offers, with slug + display name +
-- optional description. Policies still store entitlements as a JSON
-- array of slugs (no schema change to that side); the catalog is the
-- source of truth for the human-readable rendering and the
-- closed-list validation that policy entitlements must reference
-- catalog entries.
--
-- Strategy: additive only. The new column is nullable. Existing
-- products are auto-backfilled with a catalog derived from the union
-- of entitlements across their existing policies — operator can edit
-- afterward to add display names + descriptions. Products with zero
-- existing policy entitlements get NULL (legacy "free-text" mode
-- continues to work; operator opts in by editing the product).

PRAGMA foreign_keys = ON;

-- ---------------------------------------------------------------------------
-- products: entitlements_catalog_json
-- ---------------------------------------------------------------------------
-- Stored shape (when non-null):
--   [
--     { "slug": "core",         "name": "Core",         "description": "..." },
--     { "slug": "ai_summaries", "name": "AI summaries", "description": "..." },
--     ...
--   ]
--
-- Nullable so existing products that don't want a catalog stay clean,
-- and so operators can opt out by clearing the field.
ALTER TABLE products ADD COLUMN entitlements_catalog_json TEXT;

-- ---------------------------------------------------------------------------
-- Backfill: derive an initial catalog from existing policy entitlements
-- ---------------------------------------------------------------------------
-- For each product, collect the union of distinct entitlement slugs
-- across all its policies. Build a catalog row for each:
--   - slug = the slug as found in policies.entitlements_json
--   - name = slug with underscores replaced by spaces (so "ai_summaries"
--            becomes "ai summaries"; operator can title-case via the
--            admin UI)
--   - description = empty string (operator fills in)
--
-- Products with NO entitlements anywhere across their policies get
-- left with NULL — they're presumed not to use entitlements yet, and
-- forcing an empty catalog on them just adds a row to delete later.
UPDATE products SET entitlements_catalog_json = (
    SELECT json_group_array(
        json_object(
            'slug', uniq_slug,
            'name', replace(uniq_slug, '_', ' '),
            'description', ''
        )
    )
    FROM (
        SELECT DISTINCT je.value AS uniq_slug
        FROM policies p, json_each(p.entitlements_json) je
        WHERE p.product_id = products.id
    )
)
WHERE EXISTS (
    SELECT 1
    FROM policies p, json_each(p.entitlements_json) je
    WHERE p.product_id = products.id
);

-- ---------------------------------------------------------------------------
-- Note: no CHECK constraint enforcing that policy entitlements
-- reference catalog slugs. SQLite doesn't easily express this kind of
-- cross-row JSON validation as a CHECK, and even if it did, the
-- validation needs to be conditional on the catalog being non-NULL
-- (legacy mode = no catalog = anything goes). The API layer enforces
-- the closed-list rule at write time:
--
--   - On policy create/update: if the parent product has a non-NULL,
--     non-empty catalog, every entitlement slug must appear in the
--     catalog. Otherwise (NULL or empty catalog), free-text accepted.
--
-- See api/policies.rs::validate_entitlements_against_catalog.
