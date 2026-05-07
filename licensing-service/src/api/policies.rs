//! Policies — reusable license templates.
//!
//! A policy captures "when I issue a license under this shape, what are the
//! defaults?" (duration, grace period, entitlements, machine cap, trial flag,
//! optional price override). Callers to `/v1/admin/licenses` can reference a
//! policy by slug instead of specifying every field.
//!
//! Policies are per-product. The system looks up a "default" policy for a
//! product when a customer buys it through the normal purchase flow — so most
//! products should have at least one policy slugged `default`.

use crate::api::admin::{request_context, require_admin};
use crate::api::AppState;
use crate::db::repo;
use crate::error::{AppError, AppResult};
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
pub struct CreatePolicyReq {
    pub product_slug: String,
    pub name: String,
    pub slug: String,
    /// 0 = perpetual.
    #[serde(default)]
    pub duration_seconds: i64,
    #[serde(default)]
    pub grace_seconds: i64,
    /// 0 = unlimited, 1 = single-seat, n>1 = n-seat.
    #[serde(default = "default_max_machines")]
    pub max_machines: i64,
    #[serde(default)]
    pub is_trial: bool,
    #[serde(default)]
    pub price_sats_override: Option<i64>,
    #[serde(default)]
    pub entitlements: Vec<String>,
    #[serde(default)]
    pub metadata: Value,
    /// Optional Lightning recipient (e.g. "tip@keysat.xyz") to tip a percentage
    /// of each successful issuance to. None = no tipping.
    #[serde(default)]
    pub tip_recipient: Option<String>,
    /// Tip percentage in basis points. 100 = 1%. Capped at 10000 (=100%).
    #[serde(default)]
    pub tip_pct_bps: i64,
    /// Free-form label for the tip recipient (audit/UI).
    #[serde(default)]
    pub tip_label: Option<String>,
}

fn default_max_machines() -> i64 {
    1
}

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreatePolicyReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    let product = repo::get_product_by_slug(&state.db, &req.product_slug)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("product '{}'", req.product_slug)))?;

    if req.duration_seconds < 0 {
        return Err(AppError::BadRequest("duration_seconds must be >= 0".into()));
    }
    if req.grace_seconds < 0 {
        return Err(AppError::BadRequest("grace_seconds must be >= 0".into()));
    }
    if req.max_machines < 0 {
        return Err(AppError::BadRequest("max_machines must be >= 0".into()));
    }

    let metadata = if req.metadata.is_null() {
        json!({})
    } else {
        req.metadata
    };
    if req.tip_pct_bps < 0 || req.tip_pct_bps > 10_000 {
        return Err(AppError::BadRequest(
            "tip_pct_bps must be between 0 and 10000 (100%)".into(),
        ));
    }
    let tip_recipient = req.tip_recipient.as_deref().filter(|s| !s.trim().is_empty());
    if tip_recipient.is_some() && req.tip_pct_bps == 0 {
        return Err(AppError::BadRequest(
            "tip_pct_bps must be > 0 when tip_recipient is set".into(),
        ));
    }
    if tip_recipient.is_none() && req.tip_pct_bps > 0 {
        return Err(AppError::BadRequest(
            "tip_recipient must be set when tip_pct_bps > 0".into(),
        ));
    }
    let tip_label = req.tip_label.as_deref().filter(|s| !s.trim().is_empty());
    let policy = repo::create_policy(
        &state.db,
        &product.id,
        &req.name,
        &req.slug,
        req.duration_seconds,
        req.grace_seconds,
        req.max_machines,
        req.is_trial,
        req.price_sats_override,
        &req.entitlements,
        &metadata,
        tip_recipient,
        req.tip_pct_bps,
        tip_label,
    )
    .await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "policy.create",
        Some("policy"),
        Some(&policy.id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "product_id": product.id, "slug": policy.slug }),
    )
    .await;
    Ok(Json(json!(policy)))
}

#[derive(Debug, Deserialize)]
pub struct ListPoliciesQuery {
    pub product_slug: String,
    #[serde(default)]
    pub include_inactive: bool,
}

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListPoliciesQuery>,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let product = repo::get_product_by_slug(&state.db, &q.product_slug)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("product '{}'", q.product_slug)))?;
    let rows = repo::list_policies_by_product(&state.db, &product.id, !q.include_inactive).await?;
    Ok(Json(json!({ "policies": rows })))
}

#[derive(Debug, Deserialize)]
pub struct SetActiveReq {
    pub active: bool,
}

pub async fn set_active(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<SetActiveReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    repo::set_policy_active(&state.db, &id, req.active).await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "policy.set_active",
        Some("policy"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "active": req.active }),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
pub struct SetTipReq {
    /// Lightning Address (`user@domain`). Pass `null` to disable tipping.
    pub tip_recipient: Option<String>,
    /// Basis points: 0–10000. 0 = disabled.
    pub tip_pct_bps: i64,
    /// Optional free-form label (audit / UI).
    #[serde(default)]
    pub tip_label: Option<String>,
}

pub async fn set_tip(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<SetTipReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    if req.tip_pct_bps < 0 || req.tip_pct_bps > 10_000 {
        return Err(AppError::BadRequest(
            "tip_pct_bps must be between 0 and 10000".into(),
        ));
    }
    let recipient = req.tip_recipient.as_deref().filter(|s| !s.trim().is_empty());
    if recipient.is_some() && req.tip_pct_bps == 0 {
        return Err(AppError::BadRequest(
            "tip_pct_bps must be > 0 when tip_recipient is set".into(),
        ));
    }
    if recipient.is_none() && req.tip_pct_bps > 0 {
        return Err(AppError::BadRequest(
            "tip_recipient must be set when tip_pct_bps > 0".into(),
        ));
    }
    let label = req.tip_label.as_deref().filter(|s| !s.trim().is_empty());
    let updated =
        repo::set_policy_tip_config(&state.db, &id, recipient, req.tip_pct_bps, label).await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "policy.set_tip",
        Some("policy"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({
            "tip_recipient": updated.tip_recipient,
            "tip_pct_bps": updated.tip_pct_bps,
            "tip_label": updated.tip_label,
        }),
    )
    .await;
    Ok(Json(json!(updated)))
}

#[derive(Debug, Deserialize)]
pub struct ListTipsQuery {
    #[serde(default)]
    pub license_id: Option<String>,
    #[serde(default)]
    pub recipient: Option<String>,
    #[serde(default = "default_tip_limit")]
    pub limit: i64,
}

fn default_tip_limit() -> i64 {
    100
}

pub async fn list_tips(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListTipsQuery>,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let entries = repo::list_tip_attempts(
        &state.db,
        q.license_id.as_deref(),
        q.recipient.as_deref(),
        q.limit,
    )
    .await?;
    Ok(Json(json!({ "tips": entries })))
}
