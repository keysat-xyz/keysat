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
    /// Optional Lightning recipient (e.g. "keysat@primal.net") to tip a percentage
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

    // Tier-cap gate: Creator caps at 5 policies per product.
    crate::api::tier::enforce_policy_cap(&state, &product.id).await?;

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
pub struct PolicyDeleteOpts {
    #[serde(default)]
    pub force: bool,
}

/// Hard-delete a policy. Two modes:
///
/// - **Safe (default)**: refuses if any invoice or license references
///   the policy. Operator should use Hide / Disable instead in that case.
///
/// - **Force (`?force=true`)**: cascades through machines → redemptions →
///   licenses → invoices for that policy_id before removing the policy.
///   Audit-logged with cascade counts.
pub async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(opts): Query<PolicyDeleteOpts>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);

    let policy = repo::get_policy_by_id(&state.db, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("policy '{id}'")))?;

    let invoice_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM invoices WHERE policy_id = ?")
            .bind(&id)
            .fetch_one(&state.db)
            .await?;
    let license_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM licenses WHERE policy_id = ?")
            .bind(&id)
            .fetch_one(&state.db)
            .await?;
    if !opts.force && invoice_count + license_count > 0 {
        return Err(AppError::Conflict(format!(
            "cannot delete policy '{}' — it has {} invoice(s) and {} license(s) \
             referencing it. Disable it via the active toggle, or hide it from the \
             buy page via the public toggle, instead. To override and wipe all \
             references, use ?force=true.",
            policy.slug, invoice_count, license_count
        )));
    }

    let machine_count: i64 = if opts.force {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM machines WHERE license_id IN
             (SELECT id FROM licenses WHERE policy_id = ?)",
        )
        .bind(&id)
        .fetch_one(&state.db)
        .await?
    } else {
        0
    };
    let redemption_count: i64 = if opts.force {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM discount_redemptions WHERE invoice_id IN
             (SELECT id FROM invoices WHERE policy_id = ?)",
        )
        .bind(&id)
        .fetch_one(&state.db)
        .await?
    } else {
        0
    };

    let mut tx = state.db.begin().await?;
    if opts.force {
        sqlx::query(
            "DELETE FROM machines WHERE license_id IN
             (SELECT id FROM licenses WHERE policy_id = ?)",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM discount_redemptions WHERE invoice_id IN
             (SELECT id FROM invoices WHERE policy_id = ?)",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM licenses WHERE policy_id = ?")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM invoices WHERE policy_id = ?")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("DELETE FROM policies WHERE id = ?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        if opts.force { "policy.force_delete" } else { "policy.delete" },
        Some("policy"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({
            "slug": policy.slug,
            "name": policy.name,
            "force": opts.force,
            "cascaded_licenses": if opts.force { license_count } else { 0 },
            "cascaded_invoices": if opts.force { invoice_count } else { 0 },
            "cascaded_machines": machine_count,
            "cascaded_redemptions": redemption_count,
        }),
    )
    .await;
    Ok(Json(json!({
        "ok": true,
        "deleted": policy.slug,
        "force": opts.force,
        "cascaded_licenses": if opts.force { license_count } else { 0 },
        "cascaded_invoices": if opts.force { invoice_count } else { 0 },
        "cascaded_machines": machine_count,
        "cascaded_redemptions": redemption_count,
    })))
}

/// Patch mutable fields on a policy. Slug + product are NOT editable —
/// they're identifiers operators may have hard-coded into integration
/// docs or buy URLs. Tip config has its own dedicated endpoint
/// (`PATCH /v1/admin/policies/:id/tip`).
#[derive(Debug, Deserialize)]
pub struct UpdatePolicyReq {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub duration_seconds: Option<i64>,
    #[serde(default)]
    pub grace_seconds: Option<i64>,
    #[serde(default)]
    pub max_machines: Option<i64>,
    #[serde(default)]
    pub is_trial: Option<bool>,
    /// Use `Some(Some(n))` to set a tier price, `Some(null)` to clear and
    /// fall back to the product's base price.
    #[serde(default, deserialize_with = "deser_double_option_i64", skip_serializing_if = "Option::is_none")]
    pub price_sats_override: Option<Option<i64>>,
    #[serde(default)]
    pub entitlements: Option<Vec<String>>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

fn deser_double_option_i64<'de, D>(de: D) -> Result<Option<Option<i64>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<i64>::deserialize(de).map(Some)
}

pub async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<UpdatePolicyReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);

    if let Some(d) = req.duration_seconds {
        if d < 0 {
            return Err(AppError::BadRequest("duration_seconds must be >= 0".into()));
        }
    }
    if let Some(g) = req.grace_seconds {
        if g < 0 {
            return Err(AppError::BadRequest("grace_seconds must be >= 0".into()));
        }
    }
    if let Some(m) = req.max_machines {
        if m < 0 {
            return Err(AppError::BadRequest("max_machines must be >= 0".into()));
        }
    }

    let updated = repo::update_policy(
        &state.db,
        &id,
        req.name.as_deref(),
        req.duration_seconds,
        req.grace_seconds,
        req.max_machines,
        req.is_trial,
        req.price_sats_override,
        req.entitlements.as_deref(),
        req.metadata.as_ref(),
    )
    .await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "policy.update",
        Some("policy"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({
            "name": req.name,
            "duration_seconds": req.duration_seconds,
            "max_machines": req.max_machines,
            "price_sats_override": req.price_sats_override,
            "entitlements": req.entitlements,
        }),
    )
    .await;
    Ok(Json(json!(updated)))
}

#[derive(Debug, Deserialize)]
pub struct SetPublicReq {
    pub public: bool,
}

/// Toggle whether a policy is rendered as a tier-card on /buy/<slug>.
/// Private policies remain usable from admin issuance, but are excluded
/// from the public tier picker.
pub async fn set_public(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<SetPublicReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    repo::set_policy_public(&state.db, &id, req.public).await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "policy.set_public",
        Some("policy"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "public": req.public }),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

// ---------- Public buyer endpoint ----------

/// Public (no-auth): `GET /v1/products/:slug/policies` — used by the buy
/// page tier picker. Returns the product (slug, name, description, base
/// price) and an array of active+public policies, each with the fields a
/// buyer needs to decide between tiers (name, slug, description from
/// metadata, price_sats, duration_seconds, max_machines, is_trial,
/// entitlements). Internal/admin fields (id, tip recipient, raw metadata,
/// created_at) are deliberately omitted.
pub async fn list_public_policies(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> AppResult<Json<Value>> {
    let product = repo::get_product_by_slug(&state.db, &slug)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("product '{slug}'")))?;
    if !product.active {
        return Err(AppError::NotFound(format!("product '{slug}'")));
    }
    let policies = repo::list_public_policies_by_product(&state.db, &product.id).await?;

    let policies_json: Vec<Value> = policies
        .into_iter()
        .map(|p| {
            // Description: pulled from metadata.description if present, so
            // operators can write a buyer-friendly per-tier blurb without a
            // schema change. Falls back to "" if absent.
            let description = p
                .metadata
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // Highlight: same pattern — metadata.highlight = true marks the
            // "most popular" tier so the buy page can render a gold ribbon.
            let highlighted = p
                .metadata
                .get("highlight")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let price_sats = p.price_sats_override.unwrap_or(product.price_sats);
            json!({
                "slug": p.slug,
                "name": p.name,
                "description": description,
                "price_sats": price_sats,
                "duration_seconds": p.duration_seconds,
                "max_machines": p.max_machines,
                "is_trial": p.is_trial,
                "entitlements": p.entitlements,
                "highlighted": highlighted,
            })
        })
        .collect();

    Ok(Json(json!({
        "product": {
            "slug": product.slug,
            "name": product.name,
            "description": product.description,
            "base_price_sats": product.price_sats,
        },
        "policies": policies_json,
    })))
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
