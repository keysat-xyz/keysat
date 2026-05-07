//! Admin endpoints for discount / referral codes.
//!
//! Operators create codes, list them with usage stats, and disable them.
//! The public purchase flow consumes codes via the `code` field on
//! `POST /v1/purchase`; that path is handled in `crate::api::purchase`.

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
pub struct CreateDiscountCodeReq {
    /// e.g. "FOUNDERS50". Normalized to uppercase. ASCII alphanumerics + '-' '_'.
    pub code: String,
    /// 'percent' | 'fixed_sats'.
    pub kind: String,
    /// Basis points if kind == 'percent' (0..=10000); sats if kind == 'fixed_sats'.
    pub amount: i64,
    #[serde(default)]
    pub max_uses: Option<i64>,
    /// ISO-8601 RFC3339 UTC timestamp.
    #[serde(default)]
    pub expires_at: Option<String>,
    /// Restrict to a single product (by slug). Omit for any product.
    #[serde(default)]
    pub product_slug: Option<String>,
    /// Restrict to a single policy (by slug + product_slug). Omit for any policy.
    /// Requires `product_slug` to be set if specified.
    #[serde(default)]
    pub policy_slug: Option<String>,
    /// Optional free-form tag for tracking, e.g. 'launch-twitter', 'alice@example.com'.
    #[serde(default)]
    pub referrer_label: Option<String>,
    #[serde(default)]
    pub description: String,
}

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateDiscountCodeReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);

    // Resolve product/policy slugs to ids if supplied.
    let product_id = if let Some(slug) = req.product_slug.as_deref() {
        let p = repo::get_product_by_slug(&state.db, slug)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("product '{slug}'")))?;
        Some(p.id)
    } else {
        None
    };
    let policy_id = if let Some(slug) = req.policy_slug.as_deref() {
        let pid = product_id.as_deref().ok_or_else(|| {
            AppError::BadRequest("policy_slug requires product_slug".into())
        })?;
        let policy = repo::get_policy_by_slug(&state.db, pid, slug)
            .await?
            .ok_or_else(|| {
                AppError::NotFound(format!(
                    "policy '{slug}' for product '{}'",
                    req.product_slug.as_deref().unwrap_or("")
                ))
            })?;
        Some(policy.id)
    } else {
        None
    };

    let code = repo::create_discount_code(
        &state.db,
        &req.code,
        &req.kind,
        req.amount,
        req.max_uses,
        req.expires_at.as_deref(),
        product_id.as_deref(),
        policy_id.as_deref(),
        req.referrer_label.as_deref(),
        &req.description,
    )
    .await?;

    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "discount_code.create",
        Some("discount_code"),
        Some(&code.id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({
            "code": code.code,
            "kind": code.kind,
            "amount": code.amount,
            "max_uses": code.max_uses,
            "expires_at": code.expires_at,
            "product_id": product_id,
            "policy_id": policy_id,
            "referrer_label": code.referrer_label,
        }),
    )
    .await;

    Ok(Json(json!(code)))
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub include_inactive: bool,
}

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let codes = repo::list_discount_codes(&state.db, !q.include_inactive).await?;
    Ok(Json(json!({ "codes": codes })))
}

pub async fn get_one(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let code = repo::get_discount_code_by_id(&state.db, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("discount code {id}")))?;
    let redemptions = repo::list_redemptions_by_code(&state.db, &code.id).await?;
    Ok(Json(json!({
        "code": code,
        "redemptions": redemptions,
    })))
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
    repo::set_discount_code_active(&state.db, &id, req.active).await?;
    let action = if req.active {
        "discount_code.enable"
    } else {
        "discount_code.disable"
    };
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        action,
        Some("discount_code"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "active": req.active }),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

/// Hard-delete a discount code. Refuses if any redemptions reference
/// the code — those rows are part of the audit trail and shouldn't be
/// orphaned. For codes that have been used, the operator should
/// disable instead.
pub async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);

    // Look up the code so we can audit-log meaningful detail.
    let code = repo::get_discount_code_by_id(&state.db, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("discount code '{id}'")))?;

    // Refuse if any redemptions exist (referential integrity + audit
    // trail preservation). Operator should use Disable in that case.
    let redemption_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM discount_redemptions WHERE code_id = ?",
    )
    .bind(&id)
    .fetch_one(&state.db)
    .await?;
    if redemption_count > 0 {
        return Err(AppError::Conflict(format!(
            "cannot delete code '{}' — it has {} redemption(s) on the audit trail. \
             Disable it instead (it stops accepting new uses, but the history is kept).",
            code.code, redemption_count
        )));
    }

    sqlx::query("DELETE FROM discount_codes WHERE id = ?")
        .bind(&id)
        .execute(&state.db)
        .await?;

    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "discount_code.delete",
        Some("discount_code"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "code": code.code, "kind": code.kind }),
    )
    .await;
    Ok(Json(json!({ "ok": true, "deleted": code.code })))
}

#[derive(Debug, Deserialize)]
pub struct PreviewQuery {
    pub code: String,
    pub product: String,
}

/// PUBLIC endpoint — buyers hit this from the buy page when they click
/// Apply on a discount code. Validates the code (existence, active
/// state, expiry, product applicability) and returns the kind +
/// computed discounted price WITHOUT consuming a slot. The actual
/// purchase / redemption still goes through `/v1/purchase` or
/// `/v1/redeem` and is the real transaction; this is just for showing
/// the buyer what they'll be charged before they commit.
pub async fn preview(
    State(state): State<AppState>,
    Query(q): Query<PreviewQuery>,
) -> AppResult<Json<Value>> {
    let code_str = q.code.trim();
    if code_str.is_empty() {
        return Err(AppError::BadRequest("code is required".into()));
    }
    let product = repo::get_product_by_slug(&state.db, &q.product)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("product '{}'", q.product)))?;

    let code = match repo::get_discount_code_by_code(&state.db, code_str).await? {
        Some(c) => c,
        None => {
            return Ok(Json(json!({
                "valid": false,
                "reason": "unknown_code",
                "message": "Code not found.",
                "base_price_sats": product.price_sats,
            })));
        }
    };
    if !code.active {
        return Ok(Json(json!({
            "valid": false,
            "reason": "disabled",
            "message": "This code has been disabled.",
            "base_price_sats": product.price_sats,
        })));
    }
    if let Some(exp) = &code.expires_at {
        if let Ok(when) = chrono::DateTime::parse_from_rfc3339(exp) {
            if when.with_timezone(&chrono::Utc) < chrono::Utc::now() {
                return Ok(Json(json!({
                    "valid": false,
                    "reason": "expired",
                    "message": "This code has expired.",
                    "base_price_sats": product.price_sats,
                })));
            }
        }
    }
    if let Some(pid) = &code.applies_to_product_id {
        if pid != &product.id {
            return Ok(Json(json!({
                "valid": false,
                "reason": "wrong_product",
                "message": "This code does not apply to this product.",
                "base_price_sats": product.price_sats,
            })));
        }
    }
    if let Some(max) = code.max_uses {
        if code.used_count >= max {
            return Ok(Json(json!({
                "valid": false,
                "reason": "exhausted",
                "message": "This code has reached its use limit.",
                "base_price_sats": product.price_sats,
            })));
        }
    }

    // Compute the discounted price (mirroring purchase.rs's logic).
    let base = product.price_sats;
    let (final_price, discount_applied) = match code.kind.as_str() {
        "free_license" => (0i64, base),
        "percent" => {
            let bps = (code.amount).clamp(0, 10_000) as i128;
            let b = base as i128;
            let discount = ((b * bps) / 10_000).max(0).min(b) as i64;
            ((base - discount).max(1), discount)
        }
        "fixed_sats" => {
            let discount = code.amount.max(0).min(base);
            ((base - discount).max(1), discount)
        }
        _ => (base, 0),
    };

    let amount_pct = if code.kind == "percent" {
        Some(code.amount as f64 / 100.0)
    } else {
        None
    };

    Ok(Json(json!({
        "valid": true,
        "code": code.code,
        "kind": code.kind,
        "is_free": code.kind == "free_license",
        "base_price_sats": base,
        "discount_applied_sats": discount_applied,
        "final_price_sats": if code.kind == "free_license" { 0 } else { final_price },
        "amount_pct": amount_pct,
        "message": match code.kind.as_str() {
            "free_license" => "Free license — no payment required.".to_string(),
            "percent" => format!("{}% off applied.", code.amount as f64 / 100.0),
            "fixed_sats" => format!("{} sats off applied.", code.amount),
            _ => "Code applied.".to_string(),
        },
    })))
}
