//! In-place tier upgrades / downgrades.
//!
//! Companion to migration 0013 (schema) and TIER_UPGRADES_DESIGN.md
//! at the repo root. This module owns:
//!
//!   1. **Quote logic** — given a license + target policy, what
//!      does the buyer owe right now, and when do new entitlements
//!      take effect? Branches on perpetual (flat price diff) vs
//!      recurring (prorated against time-remaining-in-cycle).
//!   2. **Apply step** — when a tier-change invoice settles (or
//!      an admin force-changes), mutate the license row's policy_id
//!      + entitlements + expiry + max_machines, mutate the
//!      subscription's listed_value (so future cycles bill the new
//!      tier), insert the `tier_changes` audit row.
//!
//! Phase 3 wires these into HTTP endpoints (`POST /v1/upgrade-quote`,
//! `POST /v1/upgrade`, `POST /v1/admin/licenses/:id/change-tier`).
//! Phase 2 (this file) is pure logic — no router changes, fully
//! exercised by integration tests under `tests/upgrades.rs`.
//!
//! Pricing primitive: every quote is computed in the LISTED
//! currency (whatever `product.price_currency` is — SAT, USD, EUR).
//! At purchase time the upgrade endpoint converts to sats via
//! `crate::rates::convert_to_sats`, exactly like the existing
//! purchase + renewal paths. Quotes are always in the same currency
//! the buyer originally paid, which is what they expect.

use crate::api::AppState;
use crate::db::repo;
use crate::error::{AppError, AppResult};
use crate::models::{License, Policy, Product};
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::SqlitePool;
use uuid::Uuid;

/// What an upgrade or downgrade will cost the buyer right now,
/// what currency it's quoted in, and when the new entitlements
/// take effect. Returned by `compute_upgrade_quote`; consumed by
/// the (Phase 3) HTTP endpoint, which serializes it to JSON for
/// the buyer-app frontend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpgradeQuote {
    pub from_policy_id: String,
    pub from_policy_slug: String,
    pub to_policy_id: String,
    pub to_policy_slug: String,
    /// 'upgrade' | 'downgrade'.
    pub direction: TierDirection,
    /// 'SAT' | 'USD' | 'EUR' — same currency the product is priced in.
    pub listed_currency: String,
    /// Smallest unit of `listed_currency` (sats for SAT, cents for fiat).
    /// 0 for downgrades on recurring subs (they take effect at next
    /// cycle, no charge today) and for free→paid first-cycle changes
    /// where the charge is the full new price (we still set this
    /// to that amount; only zero-charge case is comp/admin or downgrade).
    pub proration_charge_value: i64,
    /// Effective time of the new entitlements:
    /// - upgrade on perpetual / recurring: "immediate" on settle.
    /// - downgrade on recurring: end of current cycle (RFC3339 UTC).
    /// - downgrade on perpetual: rejected (admin must force).
    pub effective_at: EffectiveAt,
    /// What the next renewal cycle will charge, in the listed currency
    /// smallest unit. `None` for perpetual (no next cycle).
    pub next_renewal_charge: Option<i64>,
    /// Recurring period of the target. None for perpetual.
    pub next_renewal_period_days: Option<i64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TierDirection {
    Upgrade,
    Downgrade,
}

impl TierDirection {
    pub fn as_str(&self) -> &'static str {
        match self {
            TierDirection::Upgrade => "upgrade",
            TierDirection::Downgrade => "downgrade",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveAt {
    /// Applies on payment settle (or immediately for comp/admin changes).
    Immediate,
    /// RFC3339 UTC timestamp — typically end of current cycle for
    /// recurring downgrades.
    At(String),
}

/// Compute a buyer-facing tier change quote. Enforces the
/// ladder rules: both policies must have non-NULL `tier_rank`,
/// target must be different from current, direction must match
/// the rank delta. Admin force-changes use the lower-level
/// `apply_tier_change` directly and are not subject to these
/// checks (Phase 4 admin endpoint covers that path).
pub async fn compute_upgrade_quote(
    state: &AppState,
    license: &License,
    target_policy: &Policy,
) -> AppResult<UpgradeQuote> {
    // 1. Resolve current policy from the license. License rows can
    //    legitimately have policy_id=NULL (legacy issuance / manual
    //    comp), in which case the buyer can't self-upgrade — they
    //    have no tier to upgrade FROM. Admin can force.
    let current_policy_id = license
        .policy_id
        .as_deref()
        .ok_or_else(|| AppError::BadRequest(
            "license has no policy attached — admin must assign a tier first".into()
        ))?;
    let current_policy = repo::get_policy_by_id(&state.db, current_policy_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("license's current policy '{current_policy_id}'")))?;

    // 2. Same-policy is a noop, not an error per se, but rejecting
    //    keeps the API contract clean (the endpoint should return
    //    400 rather than a $0 quote for an identity change).
    if current_policy.id == target_policy.id {
        return Err(AppError::BadRequest(
            "target policy is the same as current — no change to make".into(),
        ));
    }
    if current_policy.product_id != target_policy.product_id {
        return Err(AppError::BadRequest(
            "target policy belongs to a different product — cross-product changes not supported".into(),
        ));
    }
    if !target_policy.active {
        return Err(AppError::BadRequest(
            "target policy is inactive".into(),
        ));
    }

    // 3. Ladder rules: both policies must be in the ladder
    //    (non-NULL tier_rank), and target must differ in rank.
    let from_rank = current_policy.tier_rank.ok_or_else(|| {
        AppError::BadRequest(
            "current policy is not in any tier ladder — admin must set tier_rank first".into(),
        )
    })?;
    let to_rank = target_policy.tier_rank.ok_or_else(|| {
        AppError::BadRequest(
            "target policy is not in any tier ladder — admin must set tier_rank first".into(),
        )
    })?;
    let direction = match to_rank.cmp(&from_rank) {
        std::cmp::Ordering::Greater => TierDirection::Upgrade,
        std::cmp::Ordering::Less => TierDirection::Downgrade,
        std::cmp::Ordering::Equal => {
            return Err(AppError::BadRequest(
                "sideways tier changes (same rank) are admin-only".into(),
            ));
        }
    };

    // 4. Look up the product so we can read price_currency and
    //    fall back to product.price_value when a policy doesn't
    //    set its own override.
    let product = repo::get_product_by_id(&state.db, &current_policy.product_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("product '{}'", current_policy.product_id)))?;
    let listed_currency = product.price_currency.clone();

    // 5. Effective listed price for each policy.
    let from_listed = effective_listed_value(&current_policy, &product);
    let to_listed = effective_listed_value(target_policy, &product);

    // 6. Branch on perpetual vs recurring (driven by the TARGET
    //    policy — the buyer is choosing what kind of license they
    //    want to be on going forward).
    let sub = crate::subscriptions::get_subscription_by_license_id(&state.db, &license.id)
        .await
        .map_err(AppError::Internal)?;

    if target_policy.is_recurring {
        compute_recurring_quote(
            sub.as_ref(),
            &current_policy,
            target_policy,
            &product,
            from_listed,
            to_listed,
            direction,
        )
    } else {
        // Target is perpetual. Downgrade from recurring to perpetual
        // is technically a topology change; we treat it as admin-only.
        if direction == TierDirection::Downgrade && sub.is_some() {
            return Err(AppError::BadRequest(
                "downgrading from a recurring subscription to a perpetual policy is admin-only".into(),
            ));
        }
        // Perpetual downgrades by buyer also rejected — no proration
        // model that doesn't bake in a refund decision (operator's call).
        if direction == TierDirection::Downgrade {
            return Err(AppError::BadRequest(
                "perpetual downgrades are admin-only — they imply a refund decision".into(),
            ));
        }
        compute_perpetual_quote(
            &current_policy,
            target_policy,
            &listed_currency,
            from_listed,
            to_listed,
            direction,
        )
    }
}

/// Per-policy listed price, with fallback to the product's base
/// price when the policy's override is NULL. Always in the
/// product's listed currency's smallest unit.
fn effective_listed_value(policy: &Policy, product: &Product) -> i64 {
    policy.price_sats_override.unwrap_or(product.price_value)
}

fn compute_perpetual_quote(
    from: &Policy,
    to: &Policy,
    listed_currency: &str,
    from_listed: i64,
    to_listed: i64,
    direction: TierDirection,
) -> AppResult<UpgradeQuote> {
    // Perpetual upgrade: flat difference. No proration (no cycle
    // to prorate against). max(0) defends against an unusual case
    // where the operator priced an upgrade lower than the current
    // tier — should not happen with correct tier_rank, but the
    // buyer should never owe a negative amount.
    let charge = (to_listed - from_listed).max(0);
    Ok(UpgradeQuote {
        from_policy_id: from.id.clone(),
        from_policy_slug: from.slug.clone(),
        to_policy_id: to.id.clone(),
        to_policy_slug: to.slug.clone(),
        direction,
        listed_currency: listed_currency.to_string(),
        proration_charge_value: charge,
        effective_at: EffectiveAt::Immediate,
        // Perpetual has no next-cycle charge to surface.
        next_renewal_charge: None,
        next_renewal_period_days: None,
    })
}

fn compute_recurring_quote(
    sub: Option<&crate::subscriptions::Subscription>,
    from: &Policy,
    to: &Policy,
    product: &Product,
    from_listed: i64,
    to_listed: i64,
    direction: TierDirection,
) -> AppResult<UpgradeQuote> {
    let listed_currency = product.price_currency.clone();
    let target_period = to.renewal_period_days.max(1); // CHECK at API layer; clamp defensively

    match direction {
        TierDirection::Upgrade => {
            let charge = if let Some(sub) = sub {
                // Buyer is currently on a recurring sub. Prorate the
                // difference against time-remaining in the current cycle.
                let days_remaining = days_remaining_in_cycle(sub).unwrap_or(0);
                let period = sub.period_days.max(1);
                // Use i128 for the multiply to avoid overflow on
                // (price_diff * days_remaining) for high-precision fiat.
                let diff = (to_listed as i128) - (from_listed as i128);
                let prorated = diff * (days_remaining as i128) / (period as i128);
                prorated.max(0).min(i64::MAX as i128) as i64
            } else {
                // No active subscription (probably perpetual or
                // free-tier trial) upgrading TO a recurring tier.
                // Charge the full first-cycle price; the renewal
                // worker will handle subsequent cycles.
                to_listed.max(0)
            };
            Ok(UpgradeQuote {
                from_policy_id: from.id.clone(),
                from_policy_slug: from.slug.clone(),
                to_policy_id: to.id.clone(),
                to_policy_slug: to.slug.clone(),
                direction,
                listed_currency,
                proration_charge_value: charge,
                effective_at: EffectiveAt::Immediate,
                next_renewal_charge: Some(to_listed),
                next_renewal_period_days: Some(target_period),
            })
        }
        TierDirection::Downgrade => {
            // Recurring downgrade: no charge today. New tier kicks
            // in at next_renewal_at; buyer keeps full current-tier
            // entitlements through end of cycle. If there's no sub
            // (shouldn't happen — current policy was recurring), fall
            // back to immediate to avoid a stuck quote.
            let effective_at = match sub.and_then(|s| s.next_renewal_at.clone()) {
                Some(next) => EffectiveAt::At(next),
                None => EffectiveAt::Immediate,
            };
            Ok(UpgradeQuote {
                from_policy_id: from.id.clone(),
                from_policy_slug: from.slug.clone(),
                to_policy_id: to.id.clone(),
                to_policy_slug: to.slug.clone(),
                direction,
                listed_currency,
                proration_charge_value: 0,
                effective_at,
                next_renewal_charge: Some(to_listed),
                next_renewal_period_days: Some(target_period),
            })
        }
    }
}

/// Days from now until the sub's next_renewal_at. Returns None if
/// the sub has no scheduled renewal (cancelled). Floored at 0 so
/// past-due subs don't produce negative proration.
fn days_remaining_in_cycle(sub: &crate::subscriptions::Subscription) -> Option<i64> {
    let next = sub.next_renewal_at.as_deref()?;
    let next_dt = DateTime::parse_from_rfc3339(next).ok()?.with_timezone(&Utc);
    let now = Utc::now();
    let dur = next_dt.signed_duration_since(now);
    let days = dur.num_days().max(0);
    // Cap at period_days so a clock-skew or test fixture can't
    // produce a quote larger than a full cycle's price diff.
    Some(days.min(sub.period_days))
}

/// Persist a tier_changes audit row. Called from both the buyer
/// settle path and the admin force-change path. invoice_id is
/// nullable for comp / 0-charge changes.
#[allow(clippy::too_many_arguments)]
pub async fn record_tier_change(
    pool: &SqlitePool,
    license_id: &str,
    from_policy_id: &str,
    to_policy_id: &str,
    direction: TierDirection,
    listed_currency: &str,
    proration_charge_value: i64,
    invoice_id: Option<&str>,
    effective_at: &str,
    actor: &str, // 'buyer' | 'admin'
    reason: Option<&str>,
) -> Result<String> {
    if !["buyer", "admin"].contains(&actor) {
        return Err(anyhow!("actor must be 'buyer' or 'admin', got '{actor}'"));
    }
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO tier_changes(id, license_id, from_policy_id, to_policy_id, \
         direction, listed_currency, proration_charge_value, invoice_id, \
         effective_at, actor, reason, created_at) \
         VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(license_id)
    .bind(from_policy_id)
    .bind(to_policy_id)
    .bind(direction.as_str())
    .bind(listed_currency)
    .bind(proration_charge_value)
    .bind(invoice_id)
    .bind(effective_at)
    .bind(actor)
    .bind(reason)
    .bind(&now)
    .execute(pool)
    .await
    .context("INSERT tier_changes")?;
    Ok(id)
}

/// Apply a tier change: update the license row's policy_id +
/// entitlements + expires_at + max_machines + grace_seconds to
/// match the new policy, and (if a subscription exists) update
/// the sub's policy_id + listed_value + period_days. Caller is
/// responsible for `record_tier_change` separately so the audit
/// log captures the move.
///
/// This does NOT issue a new license key — the existing
/// `license_id` and signed-payload-key are kept; on next online
/// validation the buyer's app sees the new entitlements via
/// `/v1/validate`'s response. Per design doc: "matches buyers'
/// mental model — my license now does more."
pub async fn apply_tier_change(
    pool: &SqlitePool,
    license_id: &str,
    target_policy: &Policy,
    product: &Product,
) -> Result<()> {
    let now = Utc::now();
    let now_str = now.to_rfc3339();
    let entitlements_json =
        serde_json::to_string(&target_policy.entitlements).unwrap_or_else(|_| "[]".into());

    // Compute new expires_at based on target.duration_seconds.
    // 0 = perpetual; license.expires_at NULL.
    let expires_at = if target_policy.duration_seconds > 0 {
        Some((now + chrono::Duration::seconds(target_policy.duration_seconds)).to_rfc3339())
    } else {
        None
    };

    sqlx::query(
        "UPDATE licenses SET \
         policy_id = ?, entitlements_json = ?, expires_at = ?, \
         max_machines = ?, grace_seconds = ?, is_trial = ? \
         WHERE id = ?",
    )
    .bind(&target_policy.id)
    .bind(&entitlements_json)
    .bind(expires_at.as_deref())
    .bind(target_policy.max_machines)
    .bind(target_policy.grace_seconds)
    .bind(target_policy.is_trial as i64)
    .bind(license_id)
    .execute(pool)
    .await
    .context("UPDATE licenses for tier change")?;

    // If there's an active subscription for this license, update
    // its policy_id and listed_value so future renewals bill the
    // new tier. period_days also updates if the cadence changed
    // (e.g. monthly → annual).
    let sub = crate::subscriptions::get_subscription_by_license_id(pool, license_id)
        .await
        .context("fetch sub for tier-change apply")?;
    if let Some(sub) = sub {
        if target_policy.is_recurring {
            // Stay on a recurring sub at the new tier.
            let new_listed_value = effective_listed_value(target_policy, product);
            let new_period = target_policy.renewal_period_days.max(1);
            sqlx::query(
                "UPDATE subscriptions SET \
                 policy_id = ?, listed_value = ?, period_days = ?, \
                 updated_at = ? \
                 WHERE id = ?",
            )
            .bind(&target_policy.id)
            .bind(new_listed_value)
            .bind(new_period)
            .bind(&now_str)
            .bind(&sub.id)
            .execute(pool)
            .await
            .context("UPDATE subscriptions for tier change")?;
        } else {
            // Target is perpetual — the subscription has no role
            // anymore. Cancel it so the renewal worker doesn't
            // pick it up. The license itself stays valid (we
            // just updated expires_at above).
            sqlx::query(
                "UPDATE subscriptions SET \
                 status = 'cancelled', cancelled_at = ?, updated_at = ? \
                 WHERE id = ?",
            )
            .bind(&now_str)
            .bind(&now_str)
            .bind(&sub.id)
            .execute(pool)
            .await
            .context("cancel sub on perpetual tier-change apply")?;
        }
    }

    // Audit row in the existing audit_log. tier_changes is a
    // separate table that captures the upgrade-specific fields
    // (proration, invoice_id, etc.); audit_log is the generic
    // "what happened" stream. record_tier_change handles the
    // former; the latter is the caller's job (admin vs webhook
    // path each have their own actor + actor_hash semantics).
    Ok(())
}

/// Look up a tier_changes row by id. Used by Phase 4 admin
/// endpoints to surface change history.
pub async fn get_tier_change(
    pool: &SqlitePool,
    id: &str,
) -> Result<Option<TierChangeRow>> {
    let row = sqlx::query_as::<_, TierChangeRow>(
        "SELECT id, license_id, from_policy_id, to_policy_id, direction, \
                listed_currency, proration_charge_value, invoice_id, \
                effective_at, actor, reason, created_at \
         FROM tier_changes WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("SELECT tier_changes")?;
    Ok(row)
}

/// Ordered history of tier changes for a license. Newest first.
pub async fn list_tier_changes_for_license(
    pool: &SqlitePool,
    license_id: &str,
) -> Result<Vec<TierChangeRow>> {
    let rows = sqlx::query_as::<_, TierChangeRow>(
        "SELECT id, license_id, from_policy_id, to_policy_id, direction, \
                listed_currency, proration_charge_value, invoice_id, \
                effective_at, actor, reason, created_at \
         FROM tier_changes WHERE license_id = ? \
         ORDER BY created_at DESC",
    )
    .bind(license_id)
    .fetch_all(pool)
    .await
    .context("list tier_changes for license")?;
    Ok(rows)
}

/// Look up an in-flight tier-change by its invoice_id. Used by the
/// (Phase 3) webhook handler to decide on settle whether the
/// settling invoice is a tier-change vs a normal purchase or
/// subscription renewal.
pub async fn get_tier_change_by_invoice(
    pool: &SqlitePool,
    invoice_id: &str,
) -> Result<Option<TierChangeRow>> {
    let row = sqlx::query_as::<_, TierChangeRow>(
        "SELECT id, license_id, from_policy_id, to_policy_id, direction, \
                listed_currency, proration_charge_value, invoice_id, \
                effective_at, actor, reason, created_at \
         FROM tier_changes WHERE invoice_id = ?",
    )
    .bind(invoice_id)
    .fetch_optional(pool)
    .await
    .context("SELECT tier_changes by invoice")?;
    Ok(row)
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct TierChangeRow {
    pub id: String,
    pub license_id: String,
    pub from_policy_id: String,
    pub to_policy_id: String,
    pub direction: String,
    pub listed_currency: String,
    pub proration_charge_value: i64,
    pub invoice_id: Option<String>,
    pub effective_at: String,
    pub actor: String,
    pub reason: Option<String>,
    pub created_at: String,
}

// Suppress dead-code warnings on the audit-payload helper until
// Phase 3 wires it into the webhook + admin endpoints.
#[allow(dead_code)]
fn _audit_payload(quote: &UpgradeQuote) -> serde_json::Value {
    json!({
        "from_policy_id": quote.from_policy_id,
        "from_policy_slug": quote.from_policy_slug,
        "to_policy_id": quote.to_policy_id,
        "to_policy_slug": quote.to_policy_slug,
        "direction": quote.direction.as_str(),
        "listed_currency": quote.listed_currency,
        "proration_charge_value": quote.proration_charge_value,
    })
}
