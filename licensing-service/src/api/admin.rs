//! Admin endpoints — all require `Authorization: Bearer <admin_api_key>`.
//! The operator uses these to manage products and issue/revoke licenses.

use crate::api::AppState;
use crate::crypto::{encode_key, sign_payload, LicensePayload, KEY_VERSION_V2};
use crate::db::repo;
use crate::error::{AppError, AppResult};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap},
    Json,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Guards every admin handler: pulls the bearer token out of the header and
/// compares constant-time against the configured admin key. Returns the
/// SHA-256 hex of the token on success so handlers can write an audit row
/// that identifies *which* credential made the call without logging the raw
/// key.
pub fn require_admin(state: &AppState, headers: &HeaderMap) -> AppResult<String> {
    let header_val = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthorized)?;
    let token = header_val
        .strip_prefix("Bearer ")
        .ok_or(AppError::Unauthorized)?;
    if bool::from(
        token
            .as_bytes()
            .ct_eq(state.config.admin_api_key.as_bytes()),
    ) {
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        Ok(hex::encode(hasher.finalize()))
    } else {
        Err(AppError::Forbidden)
    }
}

/// Pull the best-effort client IP and User-Agent out of the request headers
/// for audit logging.
pub fn request_context(headers: &HeaderMap) -> (Option<String>, Option<String>) {
    let client_ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or("").trim().to_string())
        .filter(|s| !s.is_empty());
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    (client_ip, ua)
}

// ---------- Products ----------

#[derive(Debug, Deserialize)]
pub struct CreateProductReq {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub price_sats: i64,
    #[serde(default)]
    pub metadata: Value,
}

pub async fn create_product(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateProductReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    if req.price_sats <= 0 {
        return Err(AppError::BadRequest("price_sats must be positive".into()));
    }
    let metadata = if req.metadata.is_null() {
        json!({})
    } else {
        req.metadata
    };
    let product = repo::create_product(
        &state.db,
        &req.slug,
        &req.name,
        &req.description,
        req.price_sats,
        &metadata,
    )
    .await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "product.create",
        Some("product"),
        Some(&product.id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "slug": product.slug, "name": product.name, "price_sats": product.price_sats }),
    )
    .await;
    crate::webhooks::dispatch(
        &state,
        "product.created",
        &json!({ "product": product }),
    )
    .await;
    Ok(Json(json!(product)))
}

#[derive(Debug, Deserialize)]
pub struct SetActiveReq {
    pub active: bool,
}

pub async fn set_product_active(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<SetActiveReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    repo::set_product_active(&state.db, &id, req.active).await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "product.set_active",
        Some("product"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "active": req.active }),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

// ---------- Licenses ----------

#[derive(Debug, Deserialize)]
pub struct ListLicensesQuery {
    pub product_id: String,
}

pub async fn list_licenses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListLicensesQuery>,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let licenses = repo::list_licenses_by_product(&state.db, &q.product_id).await?;
    Ok(Json(json!({ "licenses": licenses })))
}

#[derive(Debug, Deserialize)]
pub struct SearchLicensesQuery {
    pub buyer_email: Option<String>,
    pub nostr_npub: Option<String>,
    pub invoice_id: Option<String>,
}

/// Free-form lookup used by the "lost key recovery" flow. Searches by email,
/// Nostr npub, or invoice id (whichever is supplied), returns up to 100
/// matching licenses.
pub async fn search_licenses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SearchLicensesQuery>,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let licenses = repo::search_licenses(
        &state.db,
        q.buyer_email.as_deref(),
        q.nostr_npub.as_deref(),
        q.invoice_id.as_deref(),
    )
    .await?;
    Ok(Json(json!({ "licenses": licenses })))
}

#[derive(Debug, Deserialize)]
pub struct IssueLicenseReq {
    pub product_slug: String,
    /// Optional policy slug (within the product). When set, the policy's
    /// duration, grace, entitlements, trial flag, and machine cap are used.
    #[serde(default)]
    pub policy_slug: Option<String>,
    /// Optional reason for audit — e.g. "comp", "press", "giveaway".
    #[serde(default)]
    pub note: Option<String>,
    /// Override expiry (ISO-8601 UTC). Ignored if `policy_slug` is set.
    #[serde(default)]
    pub expires_at: Option<String>,
    /// Override entitlements. Ignored if `policy_slug` is set.
    #[serde(default)]
    pub entitlements: Option<Vec<String>>,
    #[serde(default)]
    pub max_machines: Option<i64>,
    #[serde(default)]
    pub grace_seconds: Option<i64>,
    #[serde(default)]
    pub is_trial: Option<bool>,
    #[serde(default)]
    pub buyer_email: Option<String>,
    #[serde(default)]
    pub nostr_npub: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct IssueLicenseResp {
    pub license_id: String,
    pub product_id: String,
    pub license_key: String,
    pub issued_at: String,
    pub expires_at: Option<String>,
    pub entitlements: Vec<String>,
    pub is_trial: bool,
    pub max_machines: i64,
}

/// Manually issue a license outside the purchase flow. Useful for comps,
/// press keys, grandfathered users, trial keys, or developer testing.
pub async fn issue_license(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<IssueLicenseReq>,
) -> AppResult<Json<IssueLicenseResp>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);

    let product = repo::get_product_by_slug(&state.db, &req.product_slug)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("product '{}'", req.product_slug)))?;

    // Pull the policy (if any) and merge it with per-call overrides.
    let policy = if let Some(slug) = &req.policy_slug {
        Some(
            repo::get_policy_by_slug(&state.db, &product.id, slug)
                .await?
                .ok_or_else(|| {
                    AppError::NotFound(format!(
                        "policy '{slug}' for product '{}'",
                        req.product_slug
                    ))
                })?,
        )
    } else {
        None
    };

    // Compose effective values: explicit request fields take precedence over
    // the policy, which takes precedence over defaults.
    let now = Utc::now();
    let issued_at = now.to_rfc3339();
    let duration_seconds = policy.as_ref().map(|p| p.duration_seconds).unwrap_or(0);
    let expires_at = match (req.expires_at.clone(), duration_seconds) {
        (Some(explicit), _) => Some(explicit),
        (None, 0) => None, // perpetual
        (None, secs) => Some((now + Duration::seconds(secs)).to_rfc3339()),
    };
    let grace_seconds = req
        .grace_seconds
        .or_else(|| policy.as_ref().map(|p| p.grace_seconds))
        .unwrap_or(0);
    let max_machines = req
        .max_machines
        .or_else(|| policy.as_ref().map(|p| p.max_machines))
        .unwrap_or(1);
    let is_trial = req
        .is_trial
        .or_else(|| policy.as_ref().map(|p| p.is_trial))
        .unwrap_or(false);
    let entitlements = req
        .entitlements
        .clone()
        .or_else(|| policy.as_ref().map(|p| p.entitlements.clone()))
        .unwrap_or_default();

    let license_id = uuid::Uuid::new_v4().to_string();
    repo::create_license(
        &state.db,
        &license_id,
        &product.id,
        None,
        &issued_at,
        &json!({
            "source": "admin_issue",
            "note": req.note,
        }),
        policy.as_ref().map(|p| p.id.as_str()),
        expires_at.as_deref(),
        grace_seconds,
        max_machines,
        &entitlements,
        is_trial,
        req.buyer_email.as_deref(),
        req.nostr_npub.as_deref(),
    )
    .await?;

    // Build v2 signed payload.
    let mut flags = 0u8;
    if is_trial {
        flags |= crate::crypto::FLAG_TRIAL;
    }
    let payload = LicensePayload {
        version: KEY_VERSION_V2,
        flags,
        product_id: uuid::Uuid::parse_str(&product.id).unwrap(),
        license_id: uuid::Uuid::parse_str(&license_id).unwrap(),
        issued_at: now.timestamp(),
        expires_at: expires_at
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp())
            .unwrap_or(0),
        fingerprint_hash: [0u8; 32],
        entitlements: entitlements.clone(),
    };
    let sig = sign_payload(&state.keypair.signing, &payload);
    let license_key = encode_key(&payload, &sig);

    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "license.issue_manual",
        Some("license"),
        Some(&license_id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({
            "product_id": product.id,
            "policy_id": policy.as_ref().map(|p| &p.id),
            "is_trial": is_trial,
            "expires_at": expires_at,
            "entitlements": entitlements,
        }),
    )
    .await;

    crate::webhooks::dispatch(
        &state,
        "license.issued",
        &json!({
            "license_id": license_id,
            "product_id": product.id,
            "is_trial": is_trial,
            "expires_at": expires_at,
            "entitlements": entitlements,
            "source": "admin_issue",
        }),
    )
    .await;

    Ok(Json(IssueLicenseResp {
        license_id,
        product_id: product.id,
        license_key,
        issued_at,
        expires_at,
        entitlements,
        is_trial,
        max_machines,
    }))
}

#[derive(Debug, Deserialize)]
pub struct RevokeReq {
    #[serde(default)]
    pub reason: String,
}

pub async fn revoke_license(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(license_id): Path<String>,
    Json(req): Json<RevokeReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    let reason = if req.reason.is_empty() {
        "admin revoke".to_string()
    } else {
        req.reason
    };
    repo::revoke_license(&state.db, &license_id, &reason).await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "license.revoke",
        Some("license"),
        Some(&license_id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "reason": reason }),
    )
    .await;
    crate::webhooks::dispatch(
        &state,
        "license.revoked",
        &json!({ "license_id": license_id, "reason": reason }),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

// ---------- Suspension / un-suspension ----------

#[derive(Debug, Deserialize)]
pub struct SuspendReq {
    #[serde(default)]
    pub reason: String,
}

pub async fn suspend_license(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(license_id): Path<String>,
    Json(req): Json<SuspendReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    let reason = if req.reason.is_empty() {
        "admin suspend".to_string()
    } else {
        req.reason
    };
    repo::suspend_license(&state.db, &license_id, &reason).await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "license.suspend",
        Some("license"),
        Some(&license_id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "reason": reason }),
    )
    .await;
    crate::webhooks::dispatch(
        &state,
        "license.suspended",
        &json!({ "license_id": license_id, "reason": reason }),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

pub async fn unsuspend_license(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(license_id): Path<String>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    repo::unsuspend_license(&state.db, &license_id).await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "license.unsuspend",
        Some("license"),
        Some(&license_id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({}),
    )
    .await;
    crate::webhooks::dispatch(
        &state,
        "license.unsuspended",
        &json!({ "license_id": license_id }),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

// ---------- Audit log viewer ----------

#[derive(Debug, Deserialize)]
pub struct ListAuditQuery {
    #[serde(default = "default_audit_limit")]
    pub limit: i64,
    pub action: Option<String>,
}

fn default_audit_limit() -> i64 {
    200
}

pub async fn list_audit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListAuditQuery>,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let rows = repo::list_audit(&state.db, q.limit.min(1000).max(1), q.action.as_deref()).await?;
    Ok(Json(json!({ "entries": rows })))
}

// ---------- Settings (live-mutable runtime config) ----------

/// Settings key for the operator's public-facing display name. Read by
/// the `/` index handler on every request, so updates take effect
/// immediately — no daemon restart needed.
pub const SETTING_OPERATOR_NAME: &str = "operator_name";

#[derive(Debug, Deserialize)]
pub struct SetOperatorNameReq {
    /// New operator name. Empty string clears the setting (reverts to
    /// the daemon's startup-time fallback from KEYSAT_OPERATOR_NAME).
    pub name: String,
}

pub async fn set_operator_name(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SetOperatorNameReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    let trimmed = req.name.trim();
    let stored: Option<&str> = if trimmed.is_empty() { None } else { Some(trimmed) };
    repo::settings_set(&state.db, SETTING_OPERATOR_NAME, stored).await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "operator_name.set",
        Some("setting"),
        Some(SETTING_OPERATOR_NAME),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "value": stored }),
    )
    .await;
    Ok(Json(json!({ "ok": true, "operator_name": stored })))
}

pub async fn get_operator_name(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let stored = repo::settings_get(&state.db, SETTING_OPERATOR_NAME).await?;
    let effective = stored
        .clone()
        .or_else(|| state.config.operator_name.clone());
    Ok(Json(json!({
        "stored": stored,
        "effective": effective,
        "fallback_env": state.config.operator_name,
    })))
}
