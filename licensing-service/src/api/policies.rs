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

use crate::api::admin::{request_context, require_scope};
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
    /// Recurring-subscription cadence (migration 0011). When `is_recurring`
    /// is true, the renewal worker re-invoices every `renewal_period_days`.
    /// Pro-tier feature.
    #[serde(default)]
    pub is_recurring: bool,
    #[serde(default)]
    pub renewal_period_days: i64,
    /// Days the subscription stays in `past_due` before lapsing. Defaults
    /// to 7 (matches migration default) when omitted on a recurring policy.
    #[serde(default)]
    pub grace_period_days: Option<i64>,
    /// Optional free-trial length at the first cycle. 0 = no trial.
    #[serde(default)]
    pub trial_days: i64,
    /// Operator-defined ladder rank for in-place tier upgrades.
    /// `None` (or omitted) leaves the policy out of any ladder —
    /// buyer-facing upgrade flows reject changes touching it.
    /// Higher rank = better tier. See TIER_UPGRADES_DESIGN.md.
    #[serde(default)]
    pub tier_rank: Option<i64>,
}

fn default_max_machines() -> i64 {
    1
}

/// Centralised validation for the recurring-subscription knobs. Called
/// from both create + update paths so the rules stay in one place. We
/// reject internally inconsistent combos (recurring=true with period=0,
/// trial>renewal period, etc.) so the renewal worker never has to
/// defensively normalize bad rows.
fn validate_recurring(
    is_recurring: bool,
    renewal_period_days: i64,
    grace_period_days: i64,
    trial_days: i64,
) -> AppResult<()> {
    if !is_recurring {
        // Non-recurring policy: ignore the other knobs (they may be
        // carried in legacy callers).
        return Ok(());
    }
    if renewal_period_days <= 0 {
        return Err(AppError::BadRequest(
            "renewal_period_days must be > 0 when is_recurring=true".into(),
        ));
    }
    if renewal_period_days > 366 * 5 {
        return Err(AppError::BadRequest(
            "renewal_period_days unreasonably large (>5 years)".into(),
        ));
    }
    if grace_period_days < 0 {
        return Err(AppError::BadRequest("grace_period_days must be >= 0".into()));
    }
    if grace_period_days > 90 {
        return Err(AppError::BadRequest(
            "grace_period_days capped at 90 — operators wanting longer should disable lapsing manually".into(),
        ));
    }
    if trial_days < 0 {
        return Err(AppError::BadRequest("trial_days must be >= 0".into()));
    }
    if trial_days > renewal_period_days {
        return Err(AppError::BadRequest(format!(
            "trial_days ({trial_days}) cannot exceed renewal_period_days ({renewal_period_days})"
        )));
    }
    Ok(())
}

/// Closed-list validation for policy entitlements (migration 0014).
/// When the product has a non-empty entitlements catalog, every slug
/// referenced by the policy must appear in that catalog. Products
/// with no catalog (NULL or empty) accept any free-text entitlement
/// — that's the legacy mode preserved for back-compat.
fn validate_entitlements_against_catalog(
    product: &crate::models::Product,
    entitlements: &[String],
) -> AppResult<()> {
    let Some(catalog) = product.entitlements_catalog.as_ref() else {
        return Ok(());
    };
    if catalog.is_empty() {
        return Ok(());
    }
    let known: std::collections::HashSet<&str> =
        catalog.iter().map(|e| e.slug.as_str()).collect();
    for slug in entitlements {
        if !known.contains(slug.as_str()) {
            return Err(AppError::BadRequest(format!(
                "entitlement '{slug}' is not in product '{}' catalog. \
                 Add it to the product's entitlements catalog first, or \
                 clear the catalog to drop back to free-text mode.",
                product.slug
            )));
        }
    }
    Ok(())
}

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreatePolicyReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_scope(&state, &headers, "policies:write").await?;
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

    // Recurring config: fall back to migration default (7 days grace) when
    // operator omits the field. Validation rejects inconsistent combos.
    let grace_period_days = req.grace_period_days.unwrap_or(7);
    validate_recurring(
        req.is_recurring,
        req.renewal_period_days,
        grace_period_days,
        req.trial_days,
    )?;
    // Pro-tier gate: only Pro/Patron can create recurring policies. Free
    // and Creator tiers see a 402 with an upgrade URL.
    if req.is_recurring {
        crate::api::tier::enforce_recurring_feature(&state).await?;
    }
    let recurring = repo::RecurringConfig {
        is_recurring: req.is_recurring,
        renewal_period_days: req.renewal_period_days,
        grace_period_days,
        trial_days: req.trial_days,
    };

    // Tier-rank validation: if set, must be 0..=1000 — high enough
    // for any real ladder, low enough to keep arithmetic in i32 if
    // we ever expose a tier-rank UI dropdown.
    if let Some(r) = req.tier_rank {
        if !(0..=1000).contains(&r) {
            return Err(AppError::BadRequest(
                "tier_rank must be between 0 and 1000".into(),
            ));
        }
    }

    // Closed-list validation: if the product has a non-empty
    // entitlements catalog, every requested entitlement slug must
    // appear in that catalog. Products without a catalog stay in
    // legacy "free-text" mode where any string is accepted.
    validate_entitlements_against_catalog(&product, &req.entitlements)?;

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
        recurring,
        req.tier_rank,
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
    /// When true, archived policies (those with a non-null `archived_at`)
    /// are included. Default false — admin grid hides archived unless the
    /// "Show archived" toggle is on.
    #[serde(default)]
    pub include_archived: bool,
}

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListPoliciesQuery>,
) -> AppResult<Json<Value>> {
    require_scope(&state, &headers, "policies:read").await?;
    let product = repo::get_product_by_slug(&state.db, &q.product_slug)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("product '{}'", q.product_slug)))?;
    let rows = repo::list_policies_by_product_with_archived(
        &state.db,
        &product.id,
        !q.include_inactive,
        q.include_archived,
    )
    .await?;
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
    let actor_hash = require_scope(&state, &headers, "policies:write").await?;
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
pub struct SetArchivedReq {
    pub archived: bool,
}

/// PATCH `/v1/admin/policies/:id/archived` — toggle the soft-archive flag.
///
/// Archived policies are hidden from the admin grid (unless "Show archived"
/// is on) and from the public `/buy/<slug>` page. Existing licenses keep
/// validating because their entitlements are signed into the key. Active
/// recurring subscriptions tied to an archived policy will stop renewing
/// (renewal worker treats archived as a hard stop and surfaces a clear
/// event in the audit log).
pub async fn set_archived(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<SetArchivedReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_scope(&state, &headers, "policies:write").await?;
    let (ip, ua) = request_context(&headers);
    repo::set_policy_archived(&state.db, &id, req.archived).await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        if req.archived { "policy.archive" } else { "policy.unarchive" },
        Some("policy"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "archived": req.archived }),
    )
    .await;
    Ok(Json(json!({ "ok": true, "archived": req.archived })))
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
    let actor_hash = require_scope(&state, &headers, "policies:write").await?;
    let (ip, ua) = request_context(&headers);

    let policy = repo::get_policy_by_id(&state.db, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("policy '{id}'")))?;

    // Total counts (for cascade reporting).
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

    // "Live" references that would actually block a safe-delete: a
    // non-revoked license, a settled invoice (real audit history), or
    // an active/past_due subscription. Revoked-license tombstones and
    // non-settled invoices (pending/expired/invalid) are dead weight
    // that the safe-delete can sweep up — they hold no operator value.
    let live_license_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM licenses
         WHERE policy_id = ? AND COALESCE(status,'active') != 'revoked'",
    )
    .bind(&id)
    .fetch_one(&state.db)
    .await?;
    let settled_invoice_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM invoices WHERE policy_id = ? AND status = 'settled'",
    )
    .bind(&id)
    .fetch_one(&state.db)
    .await?;
    let active_sub_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM subscriptions
         WHERE policy_id = ? AND status IN ('active', 'past_due')",
    )
    .bind(&id)
    .fetch_one(&state.db)
    .await?;

    if !opts.force && (live_license_count + settled_invoice_count + active_sub_count) > 0 {
        return Err(AppError::Conflict(format!(
            "cannot delete policy '{}' — it has {} live license(s), {} settled invoice(s), \
             and {} active subscription(s) referencing it. Archive it to hide it from \
             the admin grid and the buy page, revoke any outstanding licenses to free \
             the safe-delete path, or use ?force=true to cascade through everything.",
            policy.slug, live_license_count, settled_invoice_count, active_sub_count
        )));
    }

    // Even in safe-delete mode we cascade through tombstones (revoked
    // licenses, dead invoices, machines/redemptions tied to them) since
    // the operator has signalled intent to fully delete. Compute the
    // counts for the audit log either way.
    let machine_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM machines WHERE license_id IN
         (SELECT id FROM licenses WHERE policy_id = ?)",
    )
    .bind(&id)
    .fetch_one(&state.db)
    .await?;
    let redemption_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM discount_redemptions WHERE invoice_id IN
         (SELECT id FROM invoices WHERE policy_id = ?)",
    )
    .bind(&id)
    .fetch_one(&state.db)
    .await?;

    // Cascade order matters — children before parents to satisfy FKs.
    // Safe-delete + force-delete share the cascade body now; the only
    // difference is the eligibility check above.
    let mut tx = state.db.begin().await?;
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
    // tier_changes references both from_policy_id + to_policy_id — wipe
    // any row touching this policy on either side.
    sqlx::query(
        "DELETE FROM tier_changes WHERE from_policy_id = ? OR to_policy_id = ?",
    )
    .bind(&id)
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    // discount_codes.applies_to_policy_id references this policy. Null
    // it out rather than delete the code — codes can target multiple
    // policies in future and surviving codes are useful audit material.
    sqlx::query(
        "UPDATE discount_codes SET applies_to_policy_id = NULL
         WHERE applies_to_policy_id = ?",
    )
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM subscriptions WHERE policy_id = ?")
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
            "cascaded_licenses": license_count,
            "cascaded_invoices": invoice_count,
            "cascaded_machines": machine_count,
            "cascaded_redemptions": redemption_count,
        }),
    )
    .await;
    Ok(Json(json!({
        "ok": true,
        "deleted": policy.slug,
        "force": opts.force,
        "cascaded_licenses": license_count,
        "cascaded_invoices": invoice_count,
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
    /// Recurring-subscription knobs. Each is optional (`None` = "leave
    /// untouched"). Validation rejects internally inconsistent combos
    /// (e.g. flipping is_recurring=true while leaving renewal_period_days=0).
    #[serde(default)]
    pub is_recurring: Option<bool>,
    #[serde(default)]
    pub renewal_period_days: Option<i64>,
    #[serde(default)]
    pub grace_period_days: Option<i64>,
    #[serde(default)]
    pub trial_days: Option<i64>,
    /// Tier-upgrade ladder rank. Outer Option = "did the patch
    /// touch this field?", inner Option = the value. Use
    /// `Some(Some(n))` to set, `Some(null)` to remove from the
    /// ladder, omit to leave alone. Mirrors `price_sats_override`'s
    /// nullable-patch pattern.
    #[serde(default, deserialize_with = "deser_double_option_i64", skip_serializing_if = "Option::is_none")]
    pub tier_rank: Option<Option<i64>>,
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
    let actor_hash = require_scope(&state, &headers, "policies:write").await?;
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

    // Validate recurring-subscription knobs *if* any are being touched. We
    // need to load the current row to fill in untouched fields so the
    // validator sees the post-update shape.
    if req.is_recurring.is_some()
        || req.renewal_period_days.is_some()
        || req.grace_period_days.is_some()
        || req.trial_days.is_some()
    {
        let current = repo::get_policy_by_id(&state.db, &id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("policy '{id}'")))?;
        let next_is_recurring = req.is_recurring.unwrap_or(current.is_recurring);
        let next_renewal = req
            .renewal_period_days
            .unwrap_or(current.renewal_period_days);
        let next_grace = req.grace_period_days.unwrap_or(current.grace_period_days);
        let next_trial = req.trial_days.unwrap_or(current.trial_days);
        validate_recurring(next_is_recurring, next_renewal, next_grace, next_trial)?;
        // Pro-tier gate: refuse to flip a policy to recurring on a
        // tier without `recurring_billing`. We only check on a positive
        // transition (false → true) — patches that leave is_recurring
        // alone or turn it OFF are fine for everyone.
        let was_recurring = current.is_recurring;
        if !was_recurring && next_is_recurring {
            crate::api::tier::enforce_recurring_feature(&state).await?;
        }
    }

    // Tier-rank: if the patch sets it, validate range. None-from-the-
    // outer-Option means "leave alone"; Some(None) means "remove from
    // ladder" and is always allowed.
    if let Some(Some(r)) = req.tier_rank {
        if !(0..=1000).contains(&r) {
            return Err(AppError::BadRequest(
                "tier_rank must be between 0 and 1000".into(),
            ));
        }
    }

    // Closed-list validation: if the patch supplies a new entitlements
    // list AND the parent product has a non-empty catalog, every
    // entitlement slug must appear in the catalog. Look up the
    // policy → product chain to do the check.
    if let Some(new_ents) = req.entitlements.as_deref() {
        let policy = repo::get_policy_by_id(&state.db, &id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("policy '{id}'")))?;
        let product = repo::get_product_by_id(&state.db, &policy.product_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("product '{}'", policy.product_id)))?;
        validate_entitlements_against_catalog(&product, new_ents)?;
    }

    let recurring_update = repo::RecurringUpdate {
        is_recurring: req.is_recurring,
        renewal_period_days: req.renewal_period_days,
        grace_period_days: req.grace_period_days,
        trial_days: req.trial_days,
        tier_rank: req.tier_rank,
    };

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
        recurring_update,
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
            "is_recurring": req.is_recurring,
            "renewal_period_days": req.renewal_period_days,
            "grace_period_days": req.grace_period_days,
            "trial_days": req.trial_days,
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
    let actor_hash = require_scope(&state, &headers, "policies:write").await?;
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

    // For each policy, look up an applicable active featured discount
    // (if any). The buy page + dynamic pricing page render the ribbon +
    // slashed price using this. Done as a sequential loop because the
    // policy count per product is small (≤ tier-cap = 5 on Creator,
    // unlimited on Pro but realistically <20). Switch to a single
    // batched SQL if profiling ever flags this.
    let mut featured_by_policy: std::collections::HashMap<String, crate::models::DiscountCode> =
        std::collections::HashMap::new();
    for p in &policies {
        if let Some(code) =
            repo::find_applicable_featured_discount(&state.db, &product.id, &p.id).await?
        {
            featured_by_policy.insert(p.id.clone(), code);
        }
    }

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
            // Marketing bullets: operator-controlled copy that renders
            // as ✓ checkmarks above (default) or below the entitlement
            // bullets on the buy page based on
            // `marketing_bullets_position`. Stored as an array of
            // strings in metadata; passes through to JSON unchanged.
            let marketing_bullets = p
                .metadata
                .get("marketing_bullets")
                .cloned()
                .unwrap_or_else(|| json!([]));
            // "above" (default — matches prior behavior) or "below".
            // Normalize anything else to "above" so SDK consumers don't
            // have to defensively coerce.
            let marketing_bullets_position = match p
                .metadata
                .get("marketing_bullets_position")
                .and_then(|v| v.as_str())
            {
                Some("below") => "below",
                _ => "above",
            };
            // Entitlement slugs the operator chose to hide from the
            // buy-page tier-card display. The license still grants
            // these — this only filters what buyers see. SDKs that
            // render dynamic pricing pages should also filter on this.
            let hidden_entitlements = p
                .metadata
                .get("hidden_entitlements")
                .cloned()
                .unwrap_or_else(|| json!([]));
            let price_sats = p.price_sats_override.unwrap_or(product.price_sats);
            // Featured discount (if any) — compute the post-discount
            // price the buyer would actually pay if they bought right
            // now without typing any code. We mirror the same math
            // the purchase endpoint applies (compute_discount), and
            // floor the result at 0 so a 100%-off code doesn't go
            // negative.
            let featured = featured_by_policy.get(&p.id).map(|code| {
                let discount = crate::api::purchase::compute_discount(
                    &code.kind, code.amount, price_sats,
                );
                let final_price = (price_sats - discount).max(0);
                let remaining = code.max_uses.map(|m| (m - code.used_count).max(0));
                json!({
                    "code": code.code,
                    "kind": code.kind,
                    "amount": code.amount,
                    "description": code.description,
                    "expires_at": code.expires_at,
                    "max_uses": code.max_uses,
                    "used_count": code.used_count,
                    "remaining_uses": remaining,
                    "discount_applied_sats": discount,
                    "discounted_price_sats": final_price,
                })
            });
            json!({
                "slug": p.slug,
                "name": p.name,
                "description": description,
                "price_sats": price_sats,
                "duration_seconds": p.duration_seconds,
                "max_machines": p.max_machines,
                "is_trial": p.is_trial,
                "entitlements": p.entitlements,
                "marketing_bullets": marketing_bullets,
                "marketing_bullets_position": marketing_bullets_position,
                "hidden_entitlements": hidden_entitlements,
                "highlighted": highlighted,
                // Recurring-subscription cadence — buy page renders
                // "Renews every N days" / "$X/month" when is_recurring=true.
                "is_recurring": p.is_recurring,
                "renewal_period_days": p.renewal_period_days,
                "trial_days": p.trial_days,
                // Featured (launch-special) discount metadata —
                // null when no applicable featured code exists.
                "featured_discount": featured,
            })
        })
        .collect();

    // Surface the entitlements catalog so the buy page (and SDK
    // consumers' in-app tier pickers) can render display names and
    // descriptions instead of raw slugs. Empty/None falls through
    // to the buyer's app rendering slugs verbatim — same as today.
    let entitlements_catalog = product
        .entitlements_catalog
        .as_ref()
        .map(|cat| serde_json::to_value(cat).unwrap_or_else(|_| json!([])))
        .unwrap_or_else(|| json!([]));

    Ok(Json(json!({
        "product": {
            "slug": product.slug,
            "name": product.name,
            "description": product.description,
            "base_price_sats": product.price_sats,
            "entitlements_catalog": entitlements_catalog,
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
    let actor_hash = require_scope(&state, &headers, "policies:write").await?;
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
    require_scope(&state, &headers, "policies:read").await?;
    let entries = repo::list_tip_attempts(
        &state.db,
        q.license_id.as_deref(),
        q.recipient.as_deref(),
        q.limit,
    )
    .await?;
    Ok(Json(json!({ "tips": entries })))
}
