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
    /// Currency for the `amount` field when `kind` is `fixed_sats`
    /// or `set_price`. 'SAT' (default), 'USD', or 'EUR'.
    /// Currency-agnostic for `kind = 'percent'` — basis points apply
    /// to whatever currency the product is priced in.
    #[serde(default)]
    pub discount_currency: Option<String>,
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
    /// Superseded by `policy_slugs` when both are present.
    #[serde(default)]
    pub policy_slug: Option<String>,
    /// Restrict to multiple policies (by slugs + product_slug). Omit
    /// or pass an empty list for "any policy on the product". Requires
    /// `product_slug` to be set if specified. Takes precedence over
    /// `policy_slug` when both are provided.
    #[serde(default)]
    pub policy_slugs: Option<Vec<String>>,
    /// Optional free-form tag for tracking, e.g. 'launch-twitter', 'alice@example.com'.
    #[serde(default)]
    pub referrer_label: Option<String>,
    #[serde(default)]
    pub description: String,
    /// Mark this as a "launch special" — publicly displayed on the buy
    /// page with a diagonal LAUNCH SPECIAL ribbon + original price
    /// struck through. Auto-applies for buyers who don't type any
    /// code. Operator-typed codes still win when the buyer pastes one.
    #[serde(default)]
    pub featured: bool,
}

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateDiscountCodeReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);

    // Tier-cap gate: Creator caps at 5 active discount codes.
    crate::api::tier::enforce_code_cap(&state).await?;

    // Resolve product/policy slugs to ids if supplied.
    let product_id = if let Some(slug) = req.product_slug.as_deref() {
        let p = repo::get_product_by_slug(&state.db, slug)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("product '{slug}'")))?;
        Some(p.id)
    } else {
        None
    };
    // Resolve policy scope. `policy_slugs` (multi) takes precedence over
    // `policy_slug` (singular legacy field). Both require `product_slug`.
    // Empty `policy_slugs` is treated as "no multi-scope" so the operator
    // can clear an existing multi-scope by passing [].
    let (policy_id, policy_ids_for_db): (Option<String>, Option<Vec<String>>) =
        if let Some(slugs) = req.policy_slugs.as_ref() {
            if slugs.is_empty() {
                (None, Some(Vec::new()))
            } else {
                let pid = product_id.as_deref().ok_or_else(|| {
                    AppError::BadRequest("policy_slugs requires product_slug".into())
                })?;
                let mut ids = Vec::with_capacity(slugs.len());
                for slug in slugs {
                    let policy = repo::get_policy_by_slug(&state.db, pid, slug)
                        .await?
                        .ok_or_else(|| {
                            AppError::NotFound(format!(
                                "policy '{slug}' for product '{}'",
                                req.product_slug.as_deref().unwrap_or("")
                            ))
                        })?;
                    ids.push(policy.id);
                }
                // For a single-policy choice, also populate the legacy
                // singular column so old readers stay coherent.
                let singular = if ids.len() == 1 { ids.first().cloned() } else { None };
                (singular, Some(ids))
            }
        } else if let Some(slug) = req.policy_slug.as_deref() {
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
            (Some(policy.id), None)
        } else {
            (None, None)
        };

    // Validate + normalize discount_currency. Accept SAT (default),
    // USD, EUR. For 'percent' codes the currency is irrelevant (basis
    // points are unitless) but we still record it so a future audit
    // can answer "what did the operator INTEND when they created this
    // code" — operators sometimes use a percent code with a fiat
    // mental model.
    let discount_currency = match req.discount_currency.as_deref() {
        None | Some("") => "SAT".to_string(),
        Some(c) => {
            let c = c.to_uppercase();
            if !matches!(c.as_str(), "SAT" | "USD" | "EUR") {
                return Err(AppError::BadRequest(format!(
                    "unsupported discount_currency '{c}'; accepted: SAT, USD, EUR"
                )));
            }
            c
        }
    };

    let code = repo::create_discount_code_with_currency(
        &state.db,
        &req.code,
        &req.kind,
        req.amount,
        &discount_currency,
        req.max_uses,
        req.expires_at.as_deref(),
        product_id.as_deref(),
        policy_id.as_deref(),
        policy_ids_for_db,
        req.referrer_label.as_deref(),
        &req.description,
        req.featured,
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

/// Patch fields on a discount code. Most fields are editable. The
/// `code` string and `kind` are not editable (identity fields), and
/// `applies_to_product` is not editable (moving a code between products
/// has weird semantics for historical redemptions). Policy scope IS
/// editable (v0.2.0:22+) so operators can refine which tiers a code
/// applies to without rotating the code string. All fields are optional;
/// `null` clears the field where the column is nullable.
#[derive(Debug, Deserialize)]
pub struct UpdateDiscountCodeReq {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<i64>,
    /// Use `Some(Some(n))` to set a cap, `Some(null)` to clear.
    #[serde(default, deserialize_with = "deser_double_option", skip_serializing_if = "Option::is_none")]
    pub max_uses: Option<Option<i64>>,
    #[serde(default, deserialize_with = "deser_double_option", skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, deserialize_with = "deser_double_option", skip_serializing_if = "Option::is_none")]
    pub referrer_label: Option<Option<String>>,
    /// Toggle the launch-special public-display flag. `Some(true)` to
    /// promote, `Some(false)` to demote, omit to leave alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub featured: Option<bool>,
    /// Policy slugs (multi). Overwrites the policy scope. Resolved
    /// against the code's existing `applies_to_product_id`. Send `[]`
    /// to clear the scope so the code applies to any policy on the
    /// existing product. Single-element arrays are also accepted and
    /// stored on the singular legacy column for clarity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_slugs: Option<Vec<String>>,
}

/// Helper for `Option<Option<T>>` with serde — distinguishes "not present in
/// JSON" from "present but null". Used by PATCH endpoints that need to
/// clear nullable columns explicitly.
fn deser_double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Option::<T>::deserialize(de).map(Some)
}

pub async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<UpdateDiscountCodeReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);

    // Resolve policy_slugs → policy ids using the code's EXISTING product
    // (product scope is not editable here; see UpdateDiscountCodeReq).
    // Three pass-throughs to update_discount_code:
    //   - applies_to_policy_id (singular column): set when count == 1,
    //     cleared when count != 1.
    //   - applies_to_policy_ids (JSON column): set when count >= 2,
    //     cleared when count <= 1.
    //   - both None when req.policy_slugs is absent (no change).
    let (policy_id_update, policy_ids_update): (Option<Option<String>>, Option<Vec<String>>) =
        match req.policy_slugs.as_ref() {
            None => (None, None),
            Some(slugs) => {
                let existing = repo::get_discount_code_by_id(&state.db, &id)
                    .await?
                    .ok_or_else(|| AppError::NotFound(format!("discount code {id}")))?;
                let product_id = existing.applies_to_product_id.as_deref().ok_or_else(|| {
                    AppError::BadRequest(
                        "this code is not scoped to a product, so policy scope cannot be set".into(),
                    )
                })?;
                let mut ids = Vec::with_capacity(slugs.len());
                for slug in slugs {
                    let policy = repo::get_policy_by_slug(&state.db, product_id, slug)
                        .await?
                        .ok_or_else(|| {
                            AppError::NotFound(format!(
                                "policy '{slug}' for product '{product_id}'"
                            ))
                        })?;
                    ids.push(policy.id);
                }
                match ids.len() {
                    0 => (Some(None), Some(Vec::new())),
                    1 => (Some(Some(ids[0].clone())), Some(Vec::new())),
                    _ => (Some(None), Some(ids)),
                }
            }
        };

    let updated = repo::update_discount_code(
        &state.db,
        &id,
        req.amount,
        req.max_uses,
        req.expires_at.as_ref().map(|opt| opt.as_deref()),
        req.description.as_deref(),
        req.referrer_label.as_ref().map(|opt| opt.as_deref()),
        req.featured,
        policy_id_update,
        policy_ids_update,
    )
    .await?;

    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "discount_code.update",
        Some("discount_code"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({
            "amount": req.amount,
            "max_uses": req.max_uses,
            "expires_at": req.expires_at,
            "description": req.description,
            "referrer_label": req.referrer_label,
        }),
    )
    .await;

    Ok(Json(json!(updated)))
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
    /// Optional tier slug. When set, the preview computes the discount
    /// against the policy's effective price (price_sats_override, falling
    /// back to product.price_sats), and validates that the code's
    /// applies_to_policy_id (if any) matches the chosen tier.
    #[serde(default)]
    pub policy_slug: Option<String>,
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

    // Resolve the chosen tier (if any). Lets the preview reflect the actual
    // sat amount the buyer will see for that tier, AND lets us reject a
    // code that's restricted to a different tier early.
    let chosen_policy = if let Some(ps) = q.policy_slug.as_deref().filter(|s| !s.is_empty()) {
        repo::get_policy_by_slug(&state.db, &product.id, ps).await?
    } else {
        None
    };

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
    let allowed = code.allowed_policy_ids();
    if !allowed.is_empty() {
        if let Some(chosen) = &chosen_policy {
            if !allowed.iter().any(|p| *p == chosen.id) {
                return Ok(Json(json!({
                    "valid": false,
                    "reason": "wrong_tier",
                    "message": "This code does not apply to the selected tier.",
                    "base_price_sats": chosen.price_sats_override.unwrap_or(product.price_sats),
                })));
            }
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

    // Compute the discounted price (mirroring purchase.rs's logic). Uses
    // the chosen tier's effective price if a policy_slug was supplied.
    let base = chosen_policy
        .as_ref()
        .and_then(|p| p.price_sats_override)
        .unwrap_or(product.price_sats);
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
        // 'set_price' = the buyer pays exactly this many sats (regardless of
        // the product's base price). If amount is >= base, the code provides
        // no benefit and the buyer pays base price.
        "set_price" => {
            let target = code.amount.max(0);
            if target >= base {
                (base, 0)
            } else {
                ((target).max(1), base - target)
            }
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
            "set_price" => {
                if code.amount >= base {
                    "Code applied — but it doesn't lower the price for this product.".to_string()
                } else {
                    format!("Flat price applied: {} sats.", code.amount)
                }
            }
            _ => "Code applied.".to_string(),
        },
    })))
}
