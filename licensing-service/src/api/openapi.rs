//! OpenAPI 3.1 spec for agent / SDK discovery.
//!
//! Served unauthenticated at `GET /v1/openapi.json`. The spec is a curated
//! subset of the daemon's endpoints — not auto-derived from handler
//! signatures today, so consider it a stable agent surface rather than a
//! guarantee that every internal route is documented. Endpoints not in
//! the spec still work the same way for callers that already know about
//! them.
//!
//! Authentication: every `/v1/admin/*` endpoint takes
//! `Authorization: Bearer <token>` where the token is either the master
//! `admin_api_key` or a scoped key generated in the admin UI. Master key
//! works on every endpoint; scoped keys work on endpoints that have been
//! migrated to `require_scope` (see `crate::api::api_keys`).
//!
//! Storage: the spec is held as a static JSON string at the bottom of
//! this file, parsed once into a `serde_json::Value` (via `OnceLock`),
//! and re-served from that cached value on each request. Keeps the
//! `json!` macro recursion limit out of the way.

use axum::Json;
use serde_json::Value;
use std::sync::OnceLock;

static SPEC: OnceLock<Value> = OnceLock::new();

/// `GET /v1/openapi.json` — return the spec. Public, no auth.
pub async fn spec() -> Json<Value> {
    let v = SPEC.get_or_init(|| {
        serde_json::from_str(SPEC_JSON).expect("OpenAPI spec is valid JSON")
    });
    Json(v.clone())
}

const SPEC_JSON: &str = r##"{
  "openapi": "3.1.0",
  "info": {
    "title": "Keysat",
    "description": "Bitcoin-native self-hosted software licensing service. This spec documents the operator-side admin API plus the buyer-facing validate / purchase / recover endpoints. Authentication: Bearer token. Master admin_api_key works on every endpoint; scoped API keys (generated in Settings → API keys) work on endpoints with bounded scopes.",
    "version": "0.2.0",
    "contact": { "name": "Keysat", "url": "https://keysat.xyz" }
  },
  "servers": [
    { "url": "https://licensing.keysat.xyz", "description": "Keysat's master instance" },
    { "url": "https://{your-keysat-host}", "description": "Your own Keysat instance" }
  ],
  "security": [ { "bearerAuth": [] } ],
  "components": {
    "securitySchemes": {
      "bearerAuth": {
        "type": "http",
        "scheme": "bearer",
        "description": "Master admin_api_key OR a scoped API key (ks_...). Scoped keys are gated on a role: read-only, license-issuer, support, merchant-onboard, or full-admin."
      }
    },
    "schemas": {
      "Error": {
        "type": "object",
        "properties": {
          "error":       { "type": "string", "description": "Stable machine-readable error code (e.g. tier_cap, license_revoked, not_found)" },
          "message":     { "type": "string", "description": "Human-readable detail; safe to surface to operators" },
          "upgrade_url": { "type": "string", "description": "Present on 402 tier-cap errors", "nullable": true }
        },
        "required": ["error"]
      },
      "License": {
        "type": "object",
        "properties": {
          "id":            { "type": "string", "format": "uuid" },
          "product_id":    { "type": "string", "format": "uuid" },
          "product_slug":  { "type": "string" },
          "policy_id":     { "type": "string", "format": "uuid", "nullable": true },
          "buyer_email":   { "type": "string", "nullable": true },
          "issued_at":     { "type": "string", "format": "date-time" },
          "expires_at":    { "type": "string", "format": "date-time", "nullable": true },
          "status":        { "type": "string", "enum": ["active", "revoked", "suspended"] },
          "max_machines":  { "type": "integer" },
          "entitlements":  { "type": "array", "items": { "type": "string" } },
          "license_key":   { "type": "string", "description": "The LIC1... bearer credential. Returned on issue / recover only; never on list." }
        }
      },
      "Product": {
        "type": "object",
        "properties": {
          "id":             { "type": "string", "format": "uuid" },
          "slug":           { "type": "string" },
          "name":           { "type": "string" },
          "description":    { "type": "string" },
          "price_sats":     { "type": "integer", "nullable": true },
          "price_currency": { "type": "string", "enum": ["SAT", "USD", "EUR"], "nullable": true },
          "price_value":    { "type": "integer", "nullable": true },
          "active":         { "type": "boolean" },
          "entitlements_catalog": {
            "type": "array",
            "nullable": true,
            "items": {
              "type": "object",
              "properties": {
                "slug":        { "type": "string" },
                "name":        { "type": "string" },
                "description": { "type": "string" }
              }
            }
          }
        }
      },
      "Policy": {
        "type": "object",
        "properties": {
          "id":                  { "type": "string", "format": "uuid" },
          "product_id":          { "type": "string", "format": "uuid" },
          "slug":                { "type": "string" },
          "name":                { "type": "string" },
          "duration_seconds":    { "type": "integer", "description": "0 = perpetual" },
          "max_machines":        { "type": "integer" },
          "is_trial":            { "type": "boolean" },
          "price_sats_override": { "type": "integer", "nullable": true },
          "entitlements":        { "type": "array", "items": { "type": "string" } },
          "active":              { "type": "boolean" },
          "public":              { "type": "boolean" },
          "is_recurring":        { "type": "boolean" },
          "renewal_period_days": { "type": "integer" },
          "trial_days":          { "type": "integer" },
          "tier_rank":           { "type": "integer", "nullable": true },
          "archived_at":         { "type": "string", "format": "date-time", "nullable": true }
        }
      },
      "ValidateResponse": {
        "type": "object",
        "properties": {
          "ok":           { "type": "boolean" },
          "reason":       { "type": "string", "description": "Machine-readable; one of: bad_signature, not_found, revoked, suspended, expired, fingerprint_mismatch, product_mismatch, machine_cap_exceeded" },
          "license_id":   { "type": "string", "nullable": true },
          "product_slug": { "type": "string", "nullable": true },
          "policy_slug":  { "type": "string", "nullable": true },
          "expires_at":   { "type": "string", "format": "date-time", "nullable": true },
          "entitlements": { "type": "array", "items": { "type": "string" } }
        }
      }
    }
  },
  "paths": {
    "/v1/openapi.json": {
      "get": {
        "summary": "This spec",
        "description": "Serves the OpenAPI 3.1 spec. Public, no auth.",
        "security": [],
        "responses": { "200": { "description": "The spec." } }
      }
    },
    "/v1/issuer/public-key": {
      "get": {
        "summary": "Get the daemon's signing public key",
        "description": "Returns the PEM-encoded Ed25519 public key the daemon uses to sign licenses. Public, no auth. SDK consumers can embed this for offline verification.",
        "security": [],
        "responses": {
          "200": {
            "description": "Public key",
            "content": { "application/json": { "schema": {
              "type": "object",
              "properties": { "public_key_pem": { "type": "string" } }
            } } }
          }
        }
      }
    },
    "/v1/validate": {
      "post": {
        "summary": "Validate a license key",
        "description": "Buyer-facing endpoint called by SDKs at app boot. Verifies signature, checks revocation/suspension/expiry, and (when product_slug is supplied) refuses keys issued for a different product. Always returns 200; ok=false with a stable reason on rejection.",
        "security": [],
        "requestBody": {
          "required": true,
          "content": { "application/json": { "schema": {
            "type": "object",
            "properties": {
              "key":          { "type": "string", "description": "The LIC1... license key" },
              "product_slug": { "type": "string", "description": "When supplied, the daemon refuses keys issued for a different product. Recommended." },
              "fingerprint":  { "type": "string", "description": "Machine fingerprint for cap enforcement. SHA-256 hashed daemon-side." },
              "hostname":     { "type": "string" },
              "platform":     { "type": "string" }
            },
            "required": ["key"]
          } } }
        },
        "responses": {
          "200": { "description": "Validation result", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/ValidateResponse" } } } }
        }
      }
    },
    "/v1/products/{slug}/policies": {
      "get": {
        "summary": "List a product's public tiers",
        "description": "Buyer-facing tier listing — same data /buy/<slug> renders. Use this in your app's in-app tier picker. Public, no auth.",
        "security": [],
        "parameters": [ { "name": "slug", "in": "path", "required": true, "schema": { "type": "string" } } ],
        "responses": { "200": { "description": "Tier list" } }
      }
    },
    "/v1/purchase": {
      "post": {
        "summary": "Start a buyer purchase",
        "description": "Opens an invoice with the active payment provider. The buyer opens the returned checkout_url; once payment settles, the license is available via /v1/purchase/{invoice_id} or the corresponding webhook.",
        "security": [],
        "requestBody": {
          "required": true,
          "content": { "application/json": { "schema": {
            "type": "object",
            "properties": {
              "product":      { "type": "string", "description": "Product slug" },
              "policy_slug":  { "type": "string", "description": "Optional. Specifies which tier; falls back to the product's default policy." },
              "buyer_email":  { "type": "string" },
              "redirect_url": { "type": "string" },
              "code":         { "type": "string", "description": "Optional discount code" }
            },
            "required": ["product"]
          } } }
        },
        "responses": { "200": { "description": "Purchase session created" } }
      }
    },
    "/v1/purchase/{invoice_id}": {
      "get": {
        "summary": "Poll for license issuance",
        "description": "Polled by the buyer's app until the license is issued (status=settled and license_key present). Public, no auth.",
        "security": [],
        "parameters": [ { "name": "invoice_id", "in": "path", "required": true, "schema": { "type": "string" } } ],
        "responses": { "200": { "description": "Current invoice status" } }
      }
    },
    "/v1/upgrade-quote": {
      "post": {
        "summary": "Quote a tier upgrade",
        "description": "Buyer-facing: given a license key and a target policy slug, compute the proration charge. No DB writes. Auth is by signed license_key in the body.",
        "security": [],
        "responses": { "200": { "description": "Quote" } }
      }
    },
    "/v1/upgrade": {
      "post": {
        "summary": "Start a tier upgrade",
        "description": "Creates an invoice for the prorated charge. On settle, the license's entitlements + expiry flip to the target tier without rotating the license key.",
        "security": [],
        "responses": { "200": { "description": "Upgrade invoice started" } }
      }
    },
    "/v1/subscriptions/cancel": {
      "post": {
        "summary": "Buyer self-service subscription cancellation",
        "description": "Cancels recurring renewals on the subscription tied to this license. Auth by signed license_key in the body. License stays valid through current cycle's expires_at.",
        "security": [],
        "responses": { "200": { "description": "Cancelled" } }
      }
    },
    "/v1/recover": {
      "post": {
        "summary": "Recover a lost license key",
        "description": "Given (invoice_id, email), returns the license_key for that purchase. Generic 404 on any mismatch. Rate-limited 10/min/IP.",
        "security": [],
        "responses": { "200": { "description": "License" } }
      }
    },
    "/v1/admin/licenses": {
      "get": {
        "summary": "List licenses",
        "description": "Scope required: `licenses:read`. Filter by status, product_slug, buyer_email, expiring soon, etc. via query params.",
        "responses": { "200": { "description": "License list" } }
      },
      "post": {
        "summary": "Issue a license manually",
        "description": "Scope required: `licenses:write`. Mints a fresh license without going through purchase. Useful for comping, manual support workflows.",
        "responses": { "200": { "description": "Issued license" } }
      }
    },
    "/v1/admin/licenses/{id}/revoke": {
      "post": {
        "summary": "Revoke a license",
        "description": "Scope required: `licenses:write`. Idempotent. Online validate calls immediately return reason=revoked.",
        "responses": { "200": { "description": "Revoked" } }
      }
    },
    "/v1/admin/licenses/{id}/suspend": {
      "post": {
        "summary": "Suspend a license",
        "description": "Scope required: `licenses:write`. Like revoke but reversible (see /unsuspend).",
        "responses": { "200": { "description": "Suspended" } }
      }
    },
    "/v1/admin/licenses/{id}/unsuspend": {
      "post": {
        "summary": "Unsuspend a license",
        "description": "Scope required: `licenses:write`. Reverses suspend.",
        "responses": { "200": { "description": "Unsuspended" } }
      }
    },
    "/v1/admin/licenses/{id}/change-tier": {
      "post": {
        "summary": "Admin tier change (comp)",
        "description": "Scope required: `licenses:write`. Always applies as a comp from the admin path — no invoice. Use for support workflows where a buyer should get a different tier without payment.",
        "responses": { "200": { "description": "Tier changed" } }
      }
    },
    "/v1/admin/products": {
      "get": {
        "summary": "List products",
        "description": "Scope required: `products:read`.",
        "responses": { "200": { "description": "Product list" } }
      },
      "post": {
        "summary": "Create a product",
        "description": "Scope required: `products:write`.",
        "responses": { "200": { "description": "Created" }, "402": { "description": "tier_cap — Creator tier limited to 5 products" } }
      }
    },
    "/v1/admin/policies": {
      "get": {
        "summary": "List policies",
        "description": "Scope required: `policies:read`. Filter by product_slug. Include archived with include_archived=true.",
        "responses": { "200": { "description": "Policy list" } }
      },
      "post": {
        "summary": "Create a policy (tier)",
        "description": "Scope required: `policies:write`. Recurring policies require the `recurring_billing` self-tier entitlement.",
        "responses": { "200": { "description": "Created" } }
      }
    },
    "/v1/admin/policies/{id}/archived": {
      "patch": {
        "summary": "Archive or unarchive a policy",
        "description": "Scope required: `policies:write`. Soft-archive: hides from admin grid and buy page, refuses new purchases + renewals. Existing licenses keep validating.",
        "responses": { "200": { "description": "Toggled" } }
      }
    },
    "/v1/admin/subscriptions": {
      "get": {
        "summary": "List subscriptions",
        "description": "Scope required: `subscriptions:read`. Filter by status.",
        "responses": { "200": { "description": "Subscription list" } }
      }
    },
    "/v1/admin/subscriptions/{id}/cancel": {
      "post": {
        "summary": "Admin cancel a subscription",
        "description": "Scope required: `subscriptions:write`. License stays valid through end of current cycle.",
        "responses": { "200": { "description": "Cancelled" } }
      }
    },
    "/v1/admin/machines": {
      "get": {
        "summary": "List machines",
        "description": "Scope required: `machines:read`. One row per (license_id, fingerprint) seen by /v1/validate.",
        "responses": { "200": { "description": "Machine list" } }
      }
    },
    "/v1/admin/machines/{id}/deactivate": {
      "post": {
        "summary": "Force-deactivate a machine",
        "description": "Scope required: `machines:write`. Frees the seat under that license. Validate calls from that fingerprint get fingerprint_mismatch.",
        "responses": { "200": { "description": "Deactivated" } }
      }
    },
    "/v1/admin/discount-codes": {
      "get": {
        "summary": "List discount codes",
        "description": "Scope required: `codes:read`.",
        "responses": { "200": { "description": "Code list" } }
      },
      "post": {
        "summary": "Create a discount code",
        "description": "Scope required: `codes:write`. Creator tier caps at 10 active codes.",
        "responses": { "200": { "description": "Created" } }
      }
    },
    "/v1/admin/webhook-endpoints": {
      "get": {
        "summary": "List webhook endpoints",
        "description": "Scope required: `webhooks:read`.",
        "responses": { "200": { "description": "Endpoint list" } }
      },
      "post": {
        "summary": "Create a webhook endpoint",
        "description": "Scope required: `webhooks:write`. URL + secret + event filter. Outbound deliveries are HMAC-SHA256 signed.",
        "responses": { "200": { "description": "Created" } }
      }
    },
    "/v1/admin/api-keys": {
      "get": {
        "summary": "List scoped API keys",
        "description": "Master admin key required. Never returns the raw token.",
        "responses": { "200": { "description": "Key metadata list" } }
      },
      "post": {
        "summary": "Create a scoped API key",
        "description": "Master admin key required. Token returned ONCE in the response.",
        "requestBody": {
          "required": true,
          "content": { "application/json": { "schema": {
            "type": "object",
            "properties": {
              "label": { "type": "string", "description": "Operator-friendly name, e.g. 'Recap support bot'" },
              "role":  { "type": "string", "enum": ["read-only", "license-issuer", "support", "merchant-onboard", "full-admin"] }
            },
            "required": ["label", "role"]
          } } }
        },
        "responses": { "200": { "description": "Created with raw token (returned once)" } }
      }
    },
    "/v1/admin/api-keys/{id}": {
      "delete": {
        "summary": "Revoke a scoped API key",
        "description": "Master admin key required. Soft-revoke; rows are kept for audit. Idempotent.",
        "responses": { "200": { "description": "Revoked" } }
      }
    },
    "/v1/admin/tier": {
      "get": {
        "summary": "Get this daemon's tier + usage + caps",
        "description": "Master admin key required. Returns current self-tier label + entitlements, current product/code usage, and the caps that apply at this tier.",
        "responses": { "200": { "description": "Tier info" } }
      }
    }
  }
}"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_json_parses() {
        let v: Value = serde_json::from_str(SPEC_JSON).expect("spec parses as JSON");
        // Sanity checks: top-level openapi field, at least one path, at least one schema.
        assert_eq!(v.get("openapi").and_then(|x| x.as_str()), Some("3.1.0"));
        assert!(v.get("paths").and_then(|p| p.as_object()).map(|m| !m.is_empty()).unwrap_or(false));
        assert!(v.pointer("/components/schemas/License").is_some());
    }
}
