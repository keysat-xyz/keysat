//! Scoped API keys — additional API keys with bounded permissions.
//!
//! Master credential is the env-configured `admin_api_key` (full access).
//! These scoped keys exist so operators can grant an agent / bot / partner
//! script a credential that does only what it needs to. Operator-friendly
//! flow:
//!
//!   1. Operator mints a new key via the Settings → "Scoped API keys" panel
//!      in the admin SPA (or directly via `POST /v1/admin/api-keys`), picking a
//!      role from a fixed list (Read-only / License issuer / Support /
//!      Merchant onboard / Full admin).
//!   2. The create response returns the raw token ONCE. The token never
//!      appears in any response afterward — only its sha256 hash is stored.
//!   3. Agent uses `Authorization: Bearer <token>` like the master key. Each
//!      scope-gated endpoint checks the agent's role grants the required
//!      scope; if not, 403.
//!   4. Operator can revoke any key (`DELETE /v1/admin/api-keys/:id`); revoked
//!      tokens stop working immediately.
//!
//! The master `admin_api_key` always works on every endpoint. Scoped keys are
//! honored across the catalog/license/support surface: every read endpoint
//! (`<resource>:read`), license writes (`licenses:write`), and the support
//! writes (`subscriptions:write`, `machines:write`). A deliberate set of
//! sensitive endpoints stays master-key-only — even a `full-admin` scoped key
//! gets 403 on them: rotating the issuer signing key, connecting/disconnecting
//! payment providers, setting the web-admin password, managing API keys
//! themselves, changing server settings or license tiers, and DB
//! introspection. When adding a new admin route, gate it with
//! `require_scope(state, headers, "<resource>:<read|write>")` unless it belongs
//! in that master-only set, in which case use `require_admin`.

use crate::api::admin::{request_context, require_admin};
use crate::api::AppState;
use crate::db::repo;
use crate::error::{AppError, AppResult};
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap},
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

/// Roles an operator can grant to a scoped API key.
///
/// Each role expands to a static set of scopes at auth time. Adding a
/// new role requires a migration check-constraint update plus a new arm
/// here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Every `:read` scope. Cannot mutate anything.
    ReadOnly,
    /// Read-only + license writes. Can issue / revoke / suspend / change
    /// tier on licenses, but can't touch products, policies, or codes.
    /// Right shape for an automation that gives out comp licenses.
    LicenseIssuer,
    /// License-issuer + subscription cancellation + machine deactivation.
    /// Right shape for a customer-support agent that resolves common
    /// requests without touching catalog or settings.
    Support,
    /// Read-only + catalog *and* license writes: create/edit products,
    /// define policies/tiers, and issue licenses against them. The
    /// least-privilege credential for end-to-end self-serve onboarding —
    /// a merchant (or an integrating agent) standing up a fresh catalog
    /// via the API without the master key. Deliberately excludes the
    /// support writes (subs/machines) and every master-only gate
    /// (settings, tiers, payment connect, key mgmt, signing-key, db).
    /// Tier caps still bound it: a Creator-tier box stays at 5 products /
    /// 5 policies-per-product regardless of credential.
    MerchantOnboard,
    /// Every scope. Equivalent to the master `admin_api_key` for endpoints
    /// that use `require_scope`; still rejected by endpoints that gate on
    /// settings-write or tier-write where the master key is required.
    FullAdmin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::ReadOnly => "read-only",
            Role::LicenseIssuer => "license-issuer",
            Role::Support => "support",
            Role::MerchantOnboard => "merchant-onboard",
            Role::FullAdmin => "full-admin",
        }
    }
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "read-only" => Some(Role::ReadOnly),
            "license-issuer" => Some(Role::LicenseIssuer),
            "support" => Some(Role::Support),
            "merchant-onboard" => Some(Role::MerchantOnboard),
            "full-admin" => Some(Role::FullAdmin),
            _ => None,
        }
    }
    /// Returns true if this role grants the named scope. Scope names are
    /// `<resource>:<read|write>`, e.g. `licenses:write`.
    pub fn grants(self, scope: &str) -> bool {
        match self {
            // Every scope EXCEPT the à-la-carte-only ones (e.g.
            // `payment_providers:write`). Those are never role-grantable — only
            // a per-key `extra_scopes` entry grants them — so even a full-admin
            // *scoped* key can't reach payment-connect through its role. (The
            // master key still passes `require_scope` ahead of this, via the
            // early constant-time compare, and may do anything.)
            Role::FullAdmin => !GRANTABLE_EXTRA_SCOPES.contains(&scope),
            Role::ReadOnly => scope.ends_with(":read"),
            Role::LicenseIssuer => {
                scope.ends_with(":read")
                    || matches!(scope, "licenses:write")
            }
            Role::Support => {
                scope.ends_with(":read")
                    || matches!(
                        scope,
                        "licenses:write"
                            | "subscriptions:write"
                            | "machines:write"
                    )
            }
            // Catalog + license writes only. Match scopes EXPLICITLY (never
            // by `:write` suffix) so this role can never widen into
            // settings:write / merchant_profiles:write / payment / webhooks
            // / rates — all of which would otherwise share the suffix. Adding
            // a write scope here is a deliberate per-string decision.
            Role::MerchantOnboard => {
                scope.ends_with(":read")
                    || matches!(
                        scope,
                        "products:write" | "policies:write" | "licenses:write"
                    )
            }
        }
    }
}

/// Scopes an operator may grant à-la-carte on a key (on top of its role), via
/// the `scopes` field on create. Deliberately tiny: only sensitive
/// capabilities that don't belong in any role. `payment_providers:write` is the
/// first — it is further gated at the endpoint (daemon sandbox mode + a
/// non-mainnet network check). See `plans/agent-payment-connect-scope.md`.
pub const GRANTABLE_EXTRA_SCOPES: &[&str] = &["payment_providers:write"];

/// Parse a key's `extra_scopes` JSON array and test membership. Tolerant of
/// NULL / malformed JSON (treated as "no extra scopes") so a bad row can never
/// widen access — it only ever fails closed.
fn extra_scopes_contains(json: Option<&str>, scope: &str) -> bool {
    json.and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
        .map(|v| v.iter().any(|s| s == scope))
        .unwrap_or(false)
}

/// Verify the request carries a credential that grants the named scope.
/// Order of acceptance:
///   1. Master `admin_api_key` — always passes.
///   2. Scoped API key whose role grants `scope`.
///
/// Returns the actor hash (sha256 of the token) for audit purposes. On
/// failure, 401 if no bearer header, 403 if the token is wrong or lacks
/// the scope.
pub async fn require_scope(
    state: &AppState,
    headers: &HeaderMap,
    scope: &str,
) -> AppResult<String> {
    let header_val = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthorized)?;
    let token = header_val
        .strip_prefix("Bearer ")
        .ok_or(AppError::Unauthorized)?;

    // Master admin key — constant-time compare against the configured value.
    if bool::from(
        token
            .as_bytes()
            .ct_eq(state.config.admin_api_key.as_bytes()),
    ) {
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        return Ok(hex::encode(hasher.finalize()));
    }

    // Scoped API key — hash the candidate, look up, verify not revoked,
    // confirm role grants the scope.
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let token_hash = hex::encode(hasher.finalize());

    let row: Option<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT id, role, revoked_at, extra_scopes FROM scoped_api_keys WHERE token_hash = ?",
    )
    .bind(&token_hash)
    .fetch_optional(&state.db)
    .await?;

    let (key_id, role_str, revoked_at, extra_scopes_json) = match row {
        Some(r) => r,
        None => return Err(AppError::Forbidden),
    };
    if revoked_at.is_some() {
        return Err(AppError::Forbidden);
    }
    let role = Role::parse(&role_str).ok_or(AppError::Forbidden)?;
    // A key grants a scope via its role OR via an à-la-carte `extra_scopes`
    // entry (e.g. `payment_providers:write`, which is in no role).
    let granted =
        role.grants(scope) || extra_scopes_contains(extra_scopes_json.as_deref(), scope);
    if !granted {
        return Err(AppError::Forbidden);
    }

    // Best-effort touch. Ignored on failure (clock skew, lock contention).
    let now = Utc::now().to_rfc3339();
    let _ = sqlx::query("UPDATE scoped_api_keys SET last_used_at = ? WHERE id = ?")
        .bind(&now)
        .bind(&key_id)
        .execute(&state.db)
        .await;

    Ok(token_hash)
}

// ---------- CRUD endpoints (gated on master admin only) ----------

#[derive(Debug, Deserialize)]
pub struct CreateApiKeyReq {
    pub label: String,
    pub role: String,
    /// Optional à-la-carte scopes granted on top of the role. Each must be in
    /// `GRANTABLE_EXTRA_SCOPES`. Omitted / empty = role scopes only.
    #[serde(default)]
    pub scopes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateApiKeyResp {
    pub id: String,
    pub label: String,
    pub role: String,
    /// À-la-carte scopes granted on top of the role (echoed back).
    pub scopes: Vec<String>,
    pub created_at: String,
    /// The raw token. Returned ONCE on create and never again — operator
    /// must copy it now or generate a new key.
    pub token: String,
}

/// `POST /v1/admin/api-keys` — generate a new scoped key. Master-only.
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateApiKeyReq>,
) -> AppResult<Json<CreateApiKeyResp>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);

    let label = req.label.trim();
    if label.is_empty() || label.len() > 80 {
        return Err(AppError::BadRequest(
            "label is required and must be at most 80 characters".into(),
        ));
    }
    let role = Role::parse(req.role.trim()).ok_or_else(|| {
        AppError::BadRequest(
            "role must be one of: read-only, license-issuer, support, merchant-onboard, full-admin"
                .into(),
        )
    })?;

    // Validate à-la-carte extra scopes (granted on top of the role). Only the
    // capabilities in GRANTABLE_EXTRA_SCOPES may be granted this way; anything
    // else is rejected so a typo can't silently grant nothing (or something).
    let mut extra_scopes: Vec<String> = req
        .scopes
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    extra_scopes.sort();
    extra_scopes.dedup();
    for s in &extra_scopes {
        if !GRANTABLE_EXTRA_SCOPES.contains(&s.as_str()) {
            return Err(AppError::BadRequest(format!(
                "scope '{s}' is not grantable on a key; allowed à-la-carte scopes: {}",
                GRANTABLE_EXTRA_SCOPES.join(", ")
            )));
        }
    }
    let extra_scopes_json = if extra_scopes.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&extra_scopes).expect("Vec<String> serializes"))
    };

    // 32 bytes of secure random, base64-url-encoded (no padding) → 43 chars.
    // Prefix `ks_` so it's recognizable in logs as a Keysat-style token.
    use rand::RngCore;
    let mut raw = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut raw);
    let token = format!(
        "ks_{}",
        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, raw)
    );

    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let token_hash = hex::encode(hasher.finalize());

    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO scoped_api_keys (id, label, token_hash, role, created_at, extra_scopes)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(label)
    .bind(&token_hash)
    .bind(role.as_str())
    .bind(&now)
    .bind(&extra_scopes_json)
    .execute(&state.db)
    .await?;

    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "api_key.create",
        Some("api_key"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "label": label, "role": role.as_str(), "scopes": extra_scopes.clone() }),
    )
    .await;

    Ok(Json(CreateApiKeyResp {
        id,
        label: label.to_string(),
        role: role.as_str().to_string(),
        scopes: extra_scopes,
        created_at: now,
        token,
    }))
}

#[derive(Debug, Serialize)]
pub struct ApiKeyListEntry {
    pub id: String,
    pub label: String,
    pub role: String,
    /// À-la-carte scopes granted on top of the role (empty for most keys).
    pub scopes: Vec<String>,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
}

/// `GET /v1/admin/api-keys` — list every key (active + revoked). Master-only.
/// Never returns the raw token — only metadata.
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let rows: Vec<(
        String,
        String,
        String,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT id, label, role, extra_scopes, created_at, last_used_at, revoked_at
             FROM scoped_api_keys ORDER BY created_at DESC",
    )
    .fetch_all(&state.db)
    .await?;
    let out: Vec<ApiKeyListEntry> = rows
        .into_iter()
        .map(
            |(id, label, role, extra_scopes, created_at, last_used_at, revoked_at)| ApiKeyListEntry {
                id,
                label,
                role,
                scopes: extra_scopes
                    .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
                    .unwrap_or_default(),
                created_at,
                last_used_at,
                revoked_at,
            },
        )
        .collect();
    Ok(Json(json!({ "api_keys": out })))
}

/// `DELETE /v1/admin/api-keys/:id` — soft-revoke. Master-only. Idempotent:
/// revoking an already-revoked key returns ok with no state change.
pub async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    let now = Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE scoped_api_keys SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL",
    )
    .bind(&now)
    .bind(&id)
    .execute(&state.db)
    .await?
    .rows_affected();
    if rows == 0 {
        // Either not found, or already revoked. Distinguish for the response.
        let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM scoped_api_keys WHERE id = ?")
            .bind(&id)
            .fetch_optional(&state.db)
            .await?;
        if exists.is_none() {
            return Err(AppError::NotFound(format!("api_key '{id}'")));
        }
        return Ok(Json(json!({ "ok": true, "already_revoked": true })));
    }

    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "api_key.revoke",
        Some("api_key"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({}),
    )
    .await;
    Ok(Json(json!({ "ok": true, "revoked_at": now })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariant: à-la-carte-only scopes (e.g. `payment_providers:write`)
    /// are NEVER grantable by any role — not even `full-admin`. Only a per-key
    /// `extra_scopes` entry grants them. Guards the P1 regression where
    /// `FullAdmin => true` would let a scoped full-admin key reach
    /// payment-connect through its role.
    #[test]
    fn no_role_grants_alacarte_only_scopes() {
        let roles = [
            Role::ReadOnly,
            Role::LicenseIssuer,
            Role::Support,
            Role::MerchantOnboard,
            Role::FullAdmin,
        ];
        for role in roles {
            for scope in GRANTABLE_EXTRA_SCOPES {
                assert!(
                    !role.grants(scope),
                    "role {} must NOT grant à-la-carte-only scope {scope}",
                    role.as_str()
                );
            }
        }
    }

    /// Full-admin still grants every *role* scope — the fix only carves out the
    /// à-la-carte-only set, nothing else.
    #[test]
    fn full_admin_still_grants_ordinary_scopes() {
        assert!(Role::FullAdmin.grants("products:write"));
        assert!(Role::FullAdmin.grants("policies:write"));
        assert!(Role::FullAdmin.grants("settings:read"));
        assert!(Role::FullAdmin.grants("payment_providers:read"));
    }

    /// `extra_scopes` parsing fails closed: NULL / malformed / wrong-shape JSON
    /// grants nothing and never errors open.
    #[test]
    fn extra_scopes_contains_fails_closed() {
        let json = r#"["payment_providers:write"]"#;
        assert!(extra_scopes_contains(Some(json), "payment_providers:write"));
        assert!(!extra_scopes_contains(Some(json), "products:write"));
        assert!(!extra_scopes_contains(None, "payment_providers:write")); // NULL
        assert!(!extra_scopes_contains(Some("not json"), "payment_providers:write")); // malformed
        assert!(!extra_scopes_contains(Some("{}"), "payment_providers:write")); // wrong shape
        assert!(!extra_scopes_contains(Some("[]"), "payment_providers:write")); // empty
    }
}
