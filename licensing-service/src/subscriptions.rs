//! Recurring subscriptions — renewal worker + state-transition
//! helpers.
//!
//! Companion to the schema in migration 0011 and the design at
//! `RECURRING_SUBSCRIPTIONS_DESIGN.md`. This module owns:
//!
//!   1. Background worker that scans for due renewals every 60s,
//!      creates fresh invoices via the active payment provider,
//!      and transitions the subscription's state machine.
//!   2. Repo helpers for the renewal lifecycle (find_due, mark_*,
//!      etc.) — kept here rather than in `db::repo` because they're
//!      conceptually subscription-specific and easier to reason
//!      about co-located with the worker that uses them.
//!   3. Helpers the webhook handler calls on settle to flip a
//!      sub from `past_due` back to `active`.
//!
//! State machine recap (full diagram in the design doc):
//!
//! ```text
//!     ┌─────────┐  cycle ends   ┌──────────┐
//!     │ active  │ ────────────▶ │ past_due │
//!     └─────────┘               └──────────┘
//!         ▲ pay (settle webhook)    │ grace expires
//!         └─────────────────────────┘
//!                                    │
//!                                    ▼
//!                                ┌────────┐
//!                                │ lapsed │
//!                                └────────┘
//! ```
//!
//! Cancellation can flip from `active` or `past_due` → `cancelled`
//! at any point (admin or buyer-initiated). Cancelled subs stop
//! the worker from picking them up, but the license stays valid
//! through the end of the current cycle.
//!
//! Auto-charge via saved payment profiles (Zaprite's
//! `paymentProfileId` flow) is now wired. When a buyer pays the
//! first cycle of a recurring subscription via Zaprite AND saves
//! a card at checkout, the renewal worker calls
//! `POST /v1/orders/charge` against the saved profile on each
//! cycle instead of waiting for manual pay. The wiring lives in
//! three places:
//!   - `api::purchase` sets `allow_save_payment_profile=Some(true)`
//!     on the first-cycle invoice when the policy is recurring,
//!     prompting Zaprite to show the save-card UI at checkout.
//!   - `on_invoice_settled` here calls
//!     `capture_zaprite_payment_profile`, which fetches the
//!     buyer's contact from Zaprite and persists the resulting
//!     profile id onto the subscriptions row.
//!   - `renew_one` here invokes `try_auto_charge_zaprite` after
//!     creating each renewal order. On success the buyer does
//!     nothing — the order settles via the usual webhook. On
//!     failure (decline, expired card, network) we fall through
//!     to the existing manual-pay `subscription.renewal_pending`
//!     event so the buyer can still recover the cycle.
//! BTCPay subscriptions and Zaprite subscriptions whose buyer
//! paid with Bitcoin / declined the save-card prompt have NULL
//! profile fields and continue to use the manual-pay branch
//! exclusively.

use crate::api::AppState;
use crate::db::repo;
use crate::error::AppError;
use crate::models::Invoice;
use crate::payment::CreateInvoiceParams;
use anyhow::{anyhow, Context, Result};
use chrono::{Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{Row, SqlitePool};
use std::time::Duration as StdDuration;
use uuid::Uuid;

/// How often the worker wakes up to scan for due renewals.
const TICK_INTERVAL: StdDuration = StdDuration::from_secs(60);

/// Hard cap on how many subscriptions one tick will process. Keeps
/// the worker bounded under load — a backlog of 1000 due renewals
/// drains in ~40 minutes rather than monopolizing a tick.
const MAX_PER_TICK: i64 = 25;

/// Cap on consecutive failures before the worker stops retrying
/// and waits for operator intervention. With the backoff schedule
/// below, 5 failures spans roughly 24 hours.
const MAX_CONSECUTIVE_FAILURES: i64 = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub id: String,
    pub license_id: String,
    pub policy_id: String,
    pub product_id: String,
    pub period_days: i64,
    pub listed_currency: String,
    pub listed_value: i64,
    pub status: String,
    pub started_at: String,
    pub next_renewal_at: Option<String>,
    pub cancelled_at: Option<String>,
    pub consecutive_failures: i64,
    /// Zaprite contact id for the buyer who paid the first cycle.
    /// Only ever populated for subs whose first-cycle invoice was
    /// settled via Zaprite AND whose buyer saved a payment profile
    /// at checkout. NULL otherwise (BTCPay subs, Bitcoin-paid
    /// Zaprite subs, declined-the-save-prompt Zaprite subs).
    pub zaprite_contact_id: Option<String>,
    /// Zaprite saved-profile id used by the renewal worker to
    /// auto-charge subsequent cycles via
    /// `POST /v1/orders/charge`. NULL means "no saved profile,
    /// fall through to manual-pay renewal" — the pre-feature
    /// behavior.
    pub zaprite_payment_profile_id: Option<String>,
    /// e.g. "CARD" / "BANK" — informational for the admin UI's
    /// subscription detail card. Not consulted by the worker
    /// today; Zaprite returns a decline error if the method
    /// doesn't support merchant-initiated charges.
    pub zaprite_payment_profile_method: Option<String>,
    /// ISO-8601. Informational for the admin UI ("card expires
    /// 03/27"). The renewal worker doesn't gate on this — if
    /// Zaprite reports the profile as expired we'll see it as
    /// an `/v1/orders/charge` failure and fall through to the
    /// manual-pay branch.
    pub zaprite_payment_profile_expires_at: Option<String>,
}

fn row_to_subscription(row: sqlx::sqlite::SqliteRow) -> Subscription {
    Subscription {
        id: row.get("id"),
        license_id: row.get("license_id"),
        policy_id: row.get("policy_id"),
        product_id: row.get("product_id"),
        period_days: row.get("period_days"),
        listed_currency: row.get("listed_currency"),
        listed_value: row.get("listed_value"),
        status: row.get("status"),
        started_at: row.get("started_at"),
        next_renewal_at: row.get("next_renewal_at"),
        cancelled_at: row.get("cancelled_at"),
        consecutive_failures: row.get("consecutive_failures"),
        zaprite_contact_id: row.try_get("zaprite_contact_id").ok(),
        zaprite_payment_profile_id: row.try_get("zaprite_payment_profile_id").ok(),
        zaprite_payment_profile_method: row.try_get("zaprite_payment_profile_method").ok(),
        zaprite_payment_profile_expires_at: row
            .try_get("zaprite_payment_profile_expires_at")
            .ok(),
    }
}

const SUB_COLS: &str = "id, license_id, policy_id, product_id, period_days, \
    listed_currency, listed_value, status, started_at, next_renewal_at, \
    cancelled_at, consecutive_failures, \
    zaprite_contact_id, zaprite_payment_profile_id, \
    zaprite_payment_profile_method, zaprite_payment_profile_expires_at";

/// Subs that are due for the worker to act on right now: status
/// is `active` or `past_due`, `next_renewal_at` is in the past,
/// and we haven't given up yet.
pub async fn find_due_renewals(
    pool: &SqlitePool,
    limit: i64,
) -> Result<Vec<Subscription>> {
    let now = Utc::now().to_rfc3339();
    let rows = sqlx::query(&format!(
        "SELECT {SUB_COLS} FROM subscriptions \
         WHERE status IN ('active', 'past_due') \
           AND next_renewal_at IS NOT NULL \
           AND next_renewal_at <= ? \
           AND consecutive_failures < ? \
         ORDER BY next_renewal_at ASC \
         LIMIT ?"
    ))
    .bind(&now)
    .bind(MAX_CONSECUTIVE_FAILURES)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("find_due_renewals query")?;
    Ok(rows.into_iter().map(row_to_subscription).collect())
}

/// Subs in `past_due` whose grace period has elapsed. Worker flips
/// these to `lapsed` in a separate sweep (license validation will
/// then start rejecting).
pub async fn find_lapsing_subscriptions(
    pool: &SqlitePool,
    limit: i64,
) -> Result<Vec<Subscription>> {
    // We need to JOIN to policies to read grace_period_days. Done
    // inline via a sub-query on the policy's grace value computed
    // against the sub's next_renewal_at.
    let now = Utc::now().to_rfc3339();
    let rows = sqlx::query(&format!(
        "SELECT s.id AS id, s.license_id, s.policy_id, s.product_id, s.period_days, \
                s.listed_currency, s.listed_value, s.status, s.started_at, \
                s.next_renewal_at, s.cancelled_at, s.consecutive_failures, \
                s.zaprite_contact_id, s.zaprite_payment_profile_id, \
                s.zaprite_payment_profile_method, \
                s.zaprite_payment_profile_expires_at \
         FROM subscriptions s \
         JOIN policies p ON p.id = s.policy_id \
         WHERE s.status = 'past_due' \
           AND s.next_renewal_at IS NOT NULL \
           AND datetime(s.next_renewal_at, '+' || p.grace_period_days || ' days') < ? \
         LIMIT ?"
    ))
    .bind(&now)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("find_lapsing_subscriptions query")?;
    Ok(rows.into_iter().map(row_to_subscription).collect())
}

/// Mark a sub as `lapsed`. Called from the worker's lapse sweep.
pub async fn mark_lapsed(pool: &SqlitePool, sub_id: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE subscriptions SET status = 'lapsed', updated_at = ? WHERE id = ?",
    )
    .bind(&now)
    .bind(sub_id)
    .execute(pool)
    .await
    .context("mark_lapsed")?;
    Ok(())
}

/// Mark a sub back to `active` after a successful settle webhook.
/// Resets the failure counter so future renewals get the full
/// retry budget. Called from `api::webhook::handle` when a
/// settled invoice is also linked via `subscription_invoices`.
pub async fn mark_active_after_settle(
    pool: &SqlitePool,
    sub_id: &str,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE subscriptions \
         SET status = 'active', consecutive_failures = 0, \
             last_renewal_attempt_at = ?, updated_at = ? \
         WHERE id = ?",
    )
    .bind(&now)
    .bind(&now)
    .bind(sub_id)
    .execute(pool)
    .await
    .context("mark_active_after_settle")?;
    Ok(())
}

/// Look up the subscription a given invoice belongs to (via
/// `subscription_invoices`). Returns `None` if the invoice is a
/// one-shot purchase (most invoices). Used by the webhook handler
/// to decide whether to flip a sub state on settle.
pub async fn find_subscription_for_invoice(
    pool: &SqlitePool,
    invoice_id: &str,
) -> Result<Option<String>> {
    let row = sqlx::query(
        "SELECT subscription_id FROM subscription_invoices WHERE invoice_id = ?",
    )
    .bind(invoice_id)
    .fetch_optional(pool)
    .await
    .context("find_subscription_for_invoice")?;
    Ok(row.map(|r| r.get::<String, _>("subscription_id")))
}

/// Look up a subscription by id.
pub async fn get_subscription_by_id(
    pool: &SqlitePool,
    sub_id: &str,
) -> Result<Option<Subscription>> {
    let row = sqlx::query(&format!(
        "SELECT {SUB_COLS} FROM subscriptions WHERE id = ?"
    ))
    .bind(sub_id)
    .fetch_optional(pool)
    .await
    .context("get_subscription_by_id")?;
    Ok(row.map(row_to_subscription))
}

/// Look up the subscription tied to a given license_id. There's at
/// most one (the schema enforces 1:1 via UNIQUE on license_id) — used
/// by the buyer self-service cancel endpoint, which authenticates via
/// license key, not subscription id.
pub async fn get_subscription_by_license_id(
    pool: &SqlitePool,
    license_id: &str,
) -> Result<Option<Subscription>> {
    let row = sqlx::query(&format!(
        "SELECT {SUB_COLS} FROM subscriptions WHERE license_id = ?"
    ))
    .bind(license_id)
    .fetch_optional(pool)
    .await
    .context("get_subscription_by_license_id")?;
    Ok(row.map(row_to_subscription))
}

/// List all subscriptions, optionally filtered by status. Used by the
/// admin UI's subscriptions tab. Sorted newest-first by started_at.
pub async fn list_subscriptions(
    pool: &SqlitePool,
    status_filter: Option<&str>,
    limit: i64,
) -> Result<Vec<Subscription>> {
    let limit = limit.clamp(1, 1000);
    let rows = if let Some(s) = status_filter {
        sqlx::query(&format!(
            "SELECT {SUB_COLS} FROM subscriptions WHERE status = ? \
             ORDER BY started_at DESC LIMIT ?"
        ))
        .bind(s)
        .bind(limit)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query(&format!(
            "SELECT {SUB_COLS} FROM subscriptions \
             ORDER BY started_at DESC LIMIT ?"
        ))
        .bind(limit)
        .fetch_all(pool)
        .await
    }
    .context("list_subscriptions query")?;
    Ok(rows.into_iter().map(row_to_subscription).collect())
}

/// Mark a subscription as cancelled. The license stays valid through
/// the end of the current cycle (per design doc — no immediate
/// revoke); the renewal worker's `WHERE status IN ('active', 'past_due')`
/// filter ensures cancelled subs simply stop renewing. Idempotent —
/// re-cancelling an already-cancelled sub is a no-op (returns Ok).
pub async fn cancel_subscription(
    pool: &SqlitePool,
    sub_id: &str,
) -> Result<bool> {
    let now = Utc::now().to_rfc3339();
    let rows = sqlx::query(
        "UPDATE subscriptions \
         SET status = 'cancelled', cancelled_at = ?, updated_at = ? \
         WHERE id = ? AND status IN ('active', 'past_due')",
    )
    .bind(&now)
    .bind(&now)
    .bind(sub_id)
    .execute(pool)
    .await
    .context("cancel_subscription")?
    .rows_affected();
    // rows_affected = 0 means the sub was already cancelled, lapsed,
    // or doesn't exist. Return false so the caller can decide whether
    // that's a 404 (caller already verified existence) or a no-op.
    Ok(rows > 0)
}

/// Atomic creation of a subscription + the first cycle's invoice.
/// Used at purchase time when an operator's policy has
/// `is_recurring = 1`. Not invoked by the worker (the worker
/// renews EXISTING subs); kept here for symmetry.
#[allow(clippy::too_many_arguments)]
pub async fn create_subscription(
    pool: &SqlitePool,
    license_id: &str,
    policy_id: &str,
    product_id: &str,
    period_days: i64,
    listed_currency: &str,
    listed_value: i64,
    first_cycle_invoice_id: &str,
) -> Result<Subscription> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let started_at = now.to_rfc3339();
    let next_renewal_at = (now + ChronoDuration::days(period_days)).to_rfc3339();

    sqlx::query(
        "INSERT INTO subscriptions(id, license_id, policy_id, product_id, period_days, \
         listed_currency, listed_value, status, started_at, next_renewal_at, \
         consecutive_failures, created_at, updated_at) \
         VALUES(?, ?, ?, ?, ?, ?, ?, 'active', ?, ?, 0, ?, ?)",
    )
    .bind(&id)
    .bind(license_id)
    .bind(policy_id)
    .bind(product_id)
    .bind(period_days)
    .bind(listed_currency)
    .bind(listed_value)
    .bind(&started_at)
    .bind(&next_renewal_at)
    .bind(&started_at)
    .bind(&started_at)
    .execute(pool)
    .await
    .context("INSERT subscriptions")?;

    sqlx::query(
        "INSERT INTO subscription_invoices(id, subscription_id, invoice_id, \
         cycle_number, cycle_start_at, cycle_end_at, created_at) \
         VALUES(?, ?, ?, 1, ?, ?, ?)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&id)
    .bind(first_cycle_invoice_id)
    .bind(&started_at)
    .bind(&next_renewal_at)
    .bind(&started_at)
    .execute(pool)
    .await
    .context("INSERT subscription_invoices")?;

    Ok(Subscription {
        id,
        license_id: license_id.to_string(),
        policy_id: policy_id.to_string(),
        product_id: product_id.to_string(),
        period_days,
        listed_currency: listed_currency.to_string(),
        listed_value,
        status: "active".to_string(),
        started_at: started_at.clone(),
        next_renewal_at: Some(next_renewal_at),
        cancelled_at: None,
        consecutive_failures: 0,
        // Zaprite saved-profile metadata is populated by a separate
        // post-settle hook (see `capture_zaprite_payment_profile`),
        // not here — at create-subscription time we don't yet know
        // whether the buyer saved a card.
        zaprite_contact_id: None,
        zaprite_payment_profile_id: None,
        zaprite_payment_profile_method: None,
        zaprite_payment_profile_expires_at: None,
    })
}

/// Settle any pending tier changes whose `effective_at` has arrived
/// for this subscription's license. Returns the (possibly-updated)
/// subscription state plus a flag indicating whether at least one
/// change was applied. Used by the renewal worker before pricing
/// each cycle so the new cycle reflects any scheduled downgrade /
/// upgrade.
///
/// "Pending" means: tier_changes row with `effective_at <= now` AND
/// the license's policy_id != to_policy_id (i.e. not yet applied —
/// the buyer-paid path applies via webhook on settle and that path
/// updates license.policy_id, so this query naturally excludes
/// already-applied rows). In practice the rows we apply here are
/// the comp / scheduled-downgrade rows that have invoice_id IS NULL
/// (since paid tier-changes are applied at webhook-settle time).
async fn apply_pending_tier_changes(
    state: &AppState,
    sub: &Subscription,
) -> Result<(Subscription, bool)> {
    let now_str = Utc::now().to_rfc3339();
    // Find pending rows ordered oldest-first. We apply each in
    // order so the audit trail makes sense if there's a chain.
    let rows = sqlx::query(
        "SELECT tc.id AS id, tc.to_policy_id AS to_policy_id \
         FROM tier_changes tc \
         JOIN licenses l ON l.id = tc.license_id \
         WHERE tc.license_id = ? \
           AND tc.effective_at <= ? \
           AND tc.invoice_id IS NULL \
           AND (l.policy_id IS NULL OR l.policy_id != tc.to_policy_id) \
         ORDER BY tc.effective_at ASC, tc.created_at ASC",
    )
    .bind(&sub.license_id)
    .bind(&now_str)
    .fetch_all(&state.db)
    .await
    .context("find pending tier_changes")?;

    if rows.is_empty() {
        return Ok((sub.clone(), false));
    }

    let mut applied_any = false;
    for row in rows {
        let to_policy_id: String = row.get("to_policy_id");
        let target_policy = match crate::db::repo::get_policy_by_id(&state.db, &to_policy_id).await? {
            Some(p) => p,
            None => {
                tracing::warn!(
                    sub_id = %sub.id,
                    to_policy_id = %to_policy_id,
                    "pending tier_change references missing policy; skipping"
                );
                continue;
            }
        };
        let product = match crate::db::repo::get_product_by_id(&state.db, &target_policy.product_id).await? {
            Some(p) => p,
            None => {
                tracing::warn!(
                    sub_id = %sub.id,
                    product_id = %target_policy.product_id,
                    "pending tier_change references missing product; skipping"
                );
                continue;
            }
        };
        crate::upgrades::apply_tier_change(
            &state.db,
            &sub.license_id,
            &target_policy,
            &product,
        )
        .await
        .context("apply pending tier_change in renewal hook")?;
        applied_any = true;

        crate::webhooks::dispatch(
            state,
            "license.tier_changed",
            &json!({
                "license_id": sub.license_id,
                "product_id": product.id,
                "to_policy_id": to_policy_id,
                "to_policy_slug": target_policy.slug,
                "actor": "system",
                "applied_via": "renewal_worker",
            }),
        )
        .await;
    }

    // Re-fetch the sub if we applied anything (apply_tier_change
    // may have rewritten policy_id / listed_value / period_days /
    // status — most notably status='cancelled' if the new policy
    // is perpetual).
    if applied_any {
        match get_subscription_by_id(&state.db, &sub.id).await? {
            Some(updated) => Ok((updated, true)),
            None => {
                // Sub was deleted somehow — extremely unlikely.
                Ok((sub.clone(), true))
            }
        }
    } else {
        Ok((sub.clone(), false))
    }
}

/// Per-attempt backoff schedule for renewal failures. Index = the
/// upcoming consecutive-failures count (after this failure, what
/// will the new value be). MAX_CONSECUTIVE_FAILURES (5) is the cap
/// at which the worker stops retrying entirely.
fn renewal_backoff(attempts_after: i64) -> ChronoDuration {
    match attempts_after {
        1 => ChronoDuration::minutes(5),
        2 => ChronoDuration::minutes(30),
        3 => ChronoDuration::hours(2),
        4 => ChronoDuration::hours(6),
        _ => ChronoDuration::hours(12),
    }
}

/// One sweep of the renewal worker. Picks up to MAX_PER_TICK due
/// subs, attempts a renewal for each, and runs a lapse sweep at
/// the end. Returns Ok(()) even if individual renewals failed —
/// failure handling is per-sub via consecutive_failures + backoff.
/// Pub so integration tests can drive it synchronously without
/// waiting on the spawned background task.
pub async fn tick(state: &AppState) -> Result<()> {
    // Phase 1: due renewals.
    let due = find_due_renewals(&state.db, MAX_PER_TICK)
        .await
        .context("find due renewals")?;
    for sub in due {
        if let Err(e) = renew_one(state, &sub).await {
            tracing::warn!(
                sub_id = %sub.id,
                error = %e,
                "renewal failed; backing off"
            );
            mark_renewal_failed(&state.db, &sub).await.ok();
        }
    }

    // Phase 2: lapse sweep. Independent of phase 1; even if no
    // renewals fired this tick, an old past_due sub might have
    // crossed its grace boundary.
    let lapsing = find_lapsing_subscriptions(&state.db, MAX_PER_TICK)
        .await
        .context("find lapsing subs")?;
    for sub in lapsing {
        if let Err(e) = mark_lapsed(&state.db, &sub.id).await {
            tracing::warn!(sub_id = %sub.id, error = %e, "mark_lapsed failed");
            continue;
        }
        crate::webhooks::dispatch(
            state,
            "subscription.lapsed",
            &json!({
                "subscription_id": sub.id,
                "license_id": sub.license_id,
                "product_id": sub.product_id,
                "policy_id": sub.policy_id,
            }),
        )
        .await;
    }

    Ok(())
}

/// Attempt a single subscription renewal: convert listed amount
/// to sats, call the active payment provider's create_invoice,
/// insert the invoice + subscription_invoices rows, advance
/// next_renewal_at to the start of the next cycle, mark sub as
/// past_due (returns to active when settle webhook fires).
async fn renew_one(state: &AppState, sub: &Subscription) -> Result<()> {
    // 0. Settle any pending tier changes whose effective_at has
    //    arrived. This fires recurring downgrades scheduled by the
    //    admin endpoint (or the future buyer-downgrade flow): the
    //    operator records "downgrade Pro → Standard at next cycle"
    //    and we apply it here, BEFORE pricing the next invoice, so
    //    the new cycle bills at the new tier.
    //
    //    We re-load `sub` after applying so the renewal proceeds
    //    against the fresh policy_id / listed_value / period_days.
    let (sub_for_renewal, updated_at_least_once) =
        apply_pending_tier_changes(state, sub).await?;
    let sub = &sub_for_renewal;
    if updated_at_least_once {
        tracing::info!(
            sub_id = %sub.id,
            new_policy_id = %sub.policy_id,
            new_listed_value = sub.listed_value,
            new_period_days = sub.period_days,
            "applied pending tier change before renewal"
        );
    }

    // 0a. Refuse to renew an archived policy. The operator has
    //     explicitly taken this tier out of circulation. We dispatch a
    //     clear webhook + audit event so the operator can decide
    //     whether to unarchive or accept the lapse. The sub is left in
    //     its current state — the lapsing worker will eventually move
    //     it to `lapsed` when its grace period expires.
    let policy_for_check =
        crate::db::repo::get_policy_by_id(&state.db, &sub.policy_id).await?;
    if let Some(policy) = policy_for_check.as_ref() {
        if policy.archived_at.is_some() {
            tracing::warn!(
                sub_id = %sub.id,
                policy_id = %sub.policy_id,
                policy_slug = %policy.slug,
                "skipping renewal: policy is archived",
            );
            let _ = crate::db::repo::insert_audit(
                &state.db,
                "renewal_worker",
                None,
                "subscription.renewal_skipped_archived",
                Some("subscription"),
                Some(&sub.id),
                None,
                None,
                &json!({
                    "policy_id": sub.policy_id,
                    "policy_slug": policy.slug,
                    "reason": "policy_archived",
                }),
            )
            .await;
            crate::webhooks::dispatch(
                state,
                "subscription.renewal_skipped",
                &json!({
                    "subscription_id": sub.id,
                    "license_id": sub.license_id,
                    "product_id": sub.product_id,
                    "policy_id": sub.policy_id,
                    "policy_slug": policy.slug,
                    "reason": "policy_archived",
                }),
            )
            .await;
            return Ok(());
        }
    }

    // 1. Convert listed price to sats. SAT-currency subs are an
    //    identity (no rate fetcher hit); fiat subs re-quote each
    //    cycle (per MULTI_CURRENCY_DESIGN.md decision).
    let conversion =
        crate::rates::convert_to_sats(state, &sub.listed_currency, sub.listed_value)
            .await
            .context("rate conversion")?;
    let amount_sats = conversion.sats.max(1);

    // 2. Get the active provider. If no provider is configured
    //    we can't bill — surfaces as a renewal failure that
    //    backs off (operator probably mid-Disconnect).
    let provider = state.payment_provider().await.map_err(|e| {
        anyhow!("payment provider unavailable for renewal: {e:#}")
    })?;

    // 3. Compute the next cycle window.
    let now = Utc::now();
    let cycle_start = now;
    let cycle_end = cycle_start + ChronoDuration::days(sub.period_days);

    // 4. Fresh internal invoice id. Becomes externalUniqId on
    //    Zaprite + the local invoice row id on our side.
    let internal_invoice_id = Uuid::new_v4().to_string();
    let redirect_url = format!(
        "{}/thank-you?invoice_id={}",
        state.config.public_base_url, internal_invoice_id
    );
    let metadata = json!({
        "productId": sub.product_id,
        "subscriptionId": sub.id,
        "cycleStartAt": cycle_start.to_rfc3339(),
    });

    // 5. Create the provider-side order/invoice.
    let handle = provider
        .create_invoice(CreateInvoiceParams {
            amount: crate::payment::Money {
                currency: if sub.listed_currency == "SAT" {
                    "SAT".to_string()
                } else {
                    sub.listed_currency.clone()
                },
                amount: if sub.listed_currency == "SAT" {
                    amount_sats
                } else {
                    sub.listed_value
                },
            },
            redirect_url: &redirect_url,
            metadata,
            external_order_id: &internal_invoice_id,
            buyer_email: None, // renewal email comes from the license, not solicited fresh
            // The save-card prompt only matters on the FIRST cycle.
            // By the time we're here the sub either already has a
            // `zaprite_payment_profile_id` (we'll auto-charge below)
            // or doesn't (it never will — buyer paid with Bitcoin /
            // declined the prompt). Either way, re-prompting on
            // every renewal would be confusing UX; renewals always
            // pass `None` here.
            allow_save_payment_profile: None,
        })
        .await
        .context("provider.create_invoice for renewal")?;

    // 6. Persist the local invoice row carrying the rate audit.
    repo::create_invoice_with_currency(
        &state.db,
        &internal_invoice_id,
        &handle.provider_invoice_id,
        &sub.product_id,
        amount_sats,
        &handle.checkout_url,
        None,
        Some(&format!("Renewal cycle for subscription {}", sub.id)),
        Some(&sub.policy_id),
        if sub.listed_currency == "SAT" {
            None
        } else {
            Some(sub.listed_currency.as_str())
        },
        if sub.listed_currency == "SAT" {
            None
        } else {
            Some(sub.listed_value)
        },
        // Rate metadata only meaningful for fiat-priced subs.
        // SAT-priced subs have an identity conversion that's not
        // worth recording.
        if sub.listed_currency == "SAT" {
            None
        } else {
            conversion.rate_centibps
        },
        if sub.listed_currency == "SAT" {
            None
        } else {
            Some(conversion.source.as_str())
        },
    )
    .await
    .map_err(|e: AppError| anyhow!("repo create_invoice: {e:?}"))?;

    // 7. Link to the subscription. Cycle number = max(existing) + 1.
    let next_cycle_num: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(cycle_number), 0) + 1 \
         FROM subscription_invoices WHERE subscription_id = ?",
    )
    .bind(&sub.id)
    .fetch_one(&state.db)
    .await
    .context("compute next cycle_number")?;
    sqlx::query(
        "INSERT INTO subscription_invoices(id, subscription_id, invoice_id, \
         cycle_number, cycle_start_at, cycle_end_at, created_at) \
         VALUES(?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&sub.id)
    .bind(&internal_invoice_id)
    .bind(next_cycle_num)
    .bind(cycle_start.to_rfc3339())
    .bind(cycle_end.to_rfc3339())
    .bind(cycle_start.to_rfc3339())
    .execute(&state.db)
    .await
    .context("INSERT subscription_invoices for renewal")?;

    // 8. Advance the sub: status = past_due, next_renewal_at =
    //    end of THIS new cycle, last_renewal_attempt_at = now,
    //    consecutive_failures unchanged (will be reset on settle).
    let now_str = now.to_rfc3339();
    let next_renewal = cycle_end.to_rfc3339();
    sqlx::query(
        "UPDATE subscriptions \
         SET status = 'past_due', next_renewal_at = ?, \
             last_renewal_attempt_at = ?, updated_at = ? \
         WHERE id = ?",
    )
    .bind(&next_renewal)
    .bind(&now_str)
    .bind(&now_str)
    .bind(&sub.id)
    .execute(&state.db)
    .await
    .context("UPDATE subscriptions on renewal create")?;

    // 9. If this subscription has a saved Zaprite payment profile
    //    (captured on first-cycle settle via
    //    `capture_zaprite_payment_profile`), try to merchant-
    //    initiate the charge against it now. On success, the buyer
    //    is NOT expected to do anything — Zaprite will run the
    //    charge and fire the usual `order.paid` webhook, which
    //    `on_invoice_settled` will pick up to flip the sub back to
    //    `active` and dispatch `subscription.renewed`. On failure
    //    (declined card, expired profile, Zaprite hiccup) we log
    //    + audit + fall through to the manual-pay
    //    `subscription.renewal_pending` event below so the buyer
    //    still has a path to recover this cycle.
    let auto_charged = match try_auto_charge_zaprite(
        state,
        sub,
        &handle.provider_invoice_id,
    )
    .await
    {
        Ok(charged) => charged,
        Err(e) => {
            tracing::warn!(
                sub_id = %sub.id,
                invoice_id = %internal_invoice_id,
                error = %e,
                "Zaprite auto-charge failed; falling back to manual-pay renewal"
            );
            let _ = repo::insert_audit(
                &state.db,
                "renewal_worker",
                None,
                "subscription.auto_charge_failed",
                Some("subscription"),
                Some(&sub.id),
                None,
                None,
                &json!({
                    "invoice_id": internal_invoice_id,
                    "provider_invoice_id": handle.provider_invoice_id,
                    "error": format!("{e:#}"),
                }),
            )
            .await;
            crate::webhooks::dispatch(
                state,
                "subscription.auto_charge_failed",
                &json!({
                    "subscription_id": sub.id,
                    "license_id": sub.license_id,
                    "invoice_id": internal_invoice_id,
                    "reason": format!("{e:#}"),
                }),
            )
            .await;
            false
        }
    };

    if auto_charged {
        // Auto-charge succeeded — Zaprite will fire `order.paid`
        // shortly and the webhook handler runs the rest of the
        // renewal lifecycle. Fire an operator-visible event so
        // the operator's app can render "renewed automatically"
        // copy in their notification UI, distinct from "buyer
        // needs to pay" copy.
        crate::webhooks::dispatch(
            state,
            "subscription.auto_charge_initiated",
            &json!({
                "subscription_id": sub.id,
                "license_id": sub.license_id,
                "product_id": sub.product_id,
                "policy_id": sub.policy_id,
                "invoice_id": internal_invoice_id,
                "amount_sats": amount_sats,
                "listed_currency": sub.listed_currency,
                "listed_value": sub.listed_value,
                "cycle_number": next_cycle_num,
                "cycle_start_at": cycle_start.to_rfc3339(),
                "cycle_end_at": cycle_end.to_rfc3339(),
            }),
        )
        .await;
        return Ok(());
    }

    // 10. Manual-pay path. Operator's app gets notified that a
    //    renewal invoice exists and the buyer needs to pay. The
    //    operator's webhook receiver renders an email / push /
    //    in-app notification with `checkout_url` and sends it to
    //    `buyer_email` (Keysat does not email buyers itself —
    //    operator-driven communication, same as license issuance).
    //
    //    `is_first_paid_cycle` lets operators distinguish "your
    //    free trial is ending, here's the first real charge" from
    //    "your monthly renewal is due" — different copy is usually
    //    appropriate.
    let buyer_email: Option<String> = sqlx::query_scalar(
        "SELECT buyer_email FROM licenses WHERE id = ?",
    )
    .bind(&sub.license_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let is_first_paid_cycle = next_cycle_num == 2;

    crate::webhooks::dispatch(
        state,
        "subscription.renewal_pending",
        &json!({
            "subscription_id": sub.id,
            "license_id": sub.license_id,
            "product_id": sub.product_id,
            "policy_id": sub.policy_id,
            "invoice_id": internal_invoice_id,
            "checkout_url": handle.checkout_url,
            "amount_sats": amount_sats,
            "listed_currency": sub.listed_currency,
            "listed_value": sub.listed_value,
            "cycle_number": next_cycle_num,
            "cycle_start_at": cycle_start.to_rfc3339(),
            "cycle_end_at": cycle_end.to_rfc3339(),
            "due_at": cycle_end.to_rfc3339(),
            "buyer_email": buyer_email,
            "is_first_paid_cycle": is_first_paid_cycle,
        }),
    )
    .await;

    Ok(())
}

/// On renewal failure: bump consecutive_failures, push
/// next_renewal_at out by the backoff schedule, leave status as
/// past_due (or transition active → past_due if this was the
/// first attempt that failed).
async fn mark_renewal_failed(
    pool: &SqlitePool,
    sub: &Subscription,
) -> Result<()> {
    let now = Utc::now();
    let new_failures = sub.consecutive_failures + 1;
    let backoff = renewal_backoff(new_failures);
    let new_next_renewal = (now + backoff).to_rfc3339();
    let now_str = now.to_rfc3339();

    sqlx::query(
        "UPDATE subscriptions \
         SET status = 'past_due', \
             consecutive_failures = ?, \
             next_renewal_at = ?, \
             last_renewal_attempt_at = ?, \
             updated_at = ? \
         WHERE id = ?",
    )
    .bind(new_failures)
    .bind(&new_next_renewal)
    .bind(&now_str)
    .bind(&now_str)
    .bind(&sub.id)
    .execute(pool)
    .await
    .context("UPDATE subscriptions on renewal failure")?;
    Ok(())
}

/// Spawn the renewal worker as a long-lived background task.
/// Mirrors `webhooks::spawn_delivery_worker` — single owner,
/// process-wide, panics are logged + the loop continues.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        // Stagger startup so we don't race other boot-time tasks.
        tokio::time::sleep(StdDuration::from_secs(30)).await;
        loop {
            if let Err(e) = tick(&state).await {
                tracing::warn!(error = %e, "subscription renewal tick failed");
            }
            tokio::time::sleep(TICK_INTERVAL).await;
        }
    });
}

/// Helper for `api::webhook::handle` — when a settle webhook
/// fires for an invoice that's part of a subscription, flip the
/// sub back to `active` and dispatch the `subscription.renewed`
/// event. Returns Ok(()) whether or not the invoice was a
/// subscription invoice; only acts when there's a match.
pub async fn on_invoice_settled(state: &AppState, invoice: &Invoice) -> Result<()> {
    let sub_id = match find_subscription_for_invoice(&state.db, &invoice.id).await? {
        Some(id) => id,
        None => return Ok(()), // not a subscription invoice
    };
    mark_active_after_settle(&state.db, &sub_id).await?;

    // Best-effort: if this was the FIRST cycle of a Zaprite-paid
    // recurring subscription AND the buyer saved a payment profile
    // at checkout, capture the profile id so the renewal worker can
    // auto-charge subsequent cycles. Failures here are logged but
    // never block — the sub stays valid; renewals just fall back to
    // the manual-pay branch.
    if let Err(e) = capture_zaprite_payment_profile(state, &sub_id, invoice).await {
        tracing::warn!(
            sub_id = %sub_id,
            invoice_id = %invoice.id,
            error = %e,
            "capture_zaprite_payment_profile failed; renewals will fall back to manual pay"
        );
    }

    crate::webhooks::dispatch(
        state,
        "subscription.renewed",
        &json!({
            "subscription_id": sub_id,
            "invoice_id": invoice.id,
            "amount_sats": invoice.amount_sats,
        }),
    )
    .await;
    Ok(())
}

/// Best-effort capture of the Zaprite saved-payment-profile after a
/// first-cycle settle. No-ops in any of these cases:
///   - sub already has `zaprite_payment_profile_id` set (idempotent
///     re-delivery of the same settle webhook)
///   - active provider isn't Zaprite (BTCPay subs have no equivalent)
///   - the invoice predates the saved-profile feature (pre-:44
///     Zaprite subs)
///   - buyer paid with Bitcoin/Lightning, or declined the save-card
///     prompt — no profile gets created on Zaprite's side
///
/// When it does fire, we:
///   1. Fetch the Zaprite order to find the buyer's `contact.id`
///   2. Fetch the contact to enumerate `paymentProfiles[]`
///   3. Find the profile whose `sourceOrder.externalUniqId` matches
///      our local invoice id (= the externalUniqId we set at order
///      creation) — that's the profile saved on THIS purchase
///   4. UPDATE the subscriptions row with id / method / expiresAt
pub async fn capture_zaprite_payment_profile(
    state: &AppState,
    sub_id: &str,
    invoice: &Invoice,
) -> Result<()> {
    use crate::payment::ProviderKind;

    tracing::info!(
        sub_id = %sub_id,
        invoice_id = %invoice.id,
        provider_invoice_id = %invoice.btcpay_invoice_id,
        "capture_zaprite_payment_profile: starting"
    );

    // Idempotency: already captured?
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT zaprite_payment_profile_id FROM subscriptions WHERE id = ?",
    )
    .bind(sub_id)
    .fetch_optional(&state.db)
    .await
    .context("read existing zaprite_payment_profile_id")?
    .flatten();
    if existing.is_some() {
        tracing::info!(sub_id = %sub_id, "capture: already captured, skipping");
        return Ok(());
    }

    // Active provider must be Zaprite for any of the rest to be
    // meaningful — `as_any` downcast keeps the trait clean.
    let provider = match state.payment_provider().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                sub_id = %sub_id, error = %e,
                "capture: no active payment provider — skipping"
            );
            return Ok(());
        }
    };
    if provider.kind() != ProviderKind::Zaprite {
        tracing::info!(
            sub_id = %sub_id, kind = ?provider.kind(),
            "capture: active provider is not Zaprite — skipping"
        );
        return Ok(());
    }
    let zaprite = match provider
        .as_any()
        .downcast_ref::<crate::payment::zaprite::ZapriteProvider>()
    {
        Some(z) => z,
        None => {
            tracing::warn!(
                sub_id = %sub_id,
                "capture: provider kind is Zaprite but downcast failed — skipping"
            );
            return Ok(());
        }
    };
    let client = zaprite.client();

    // 1. Fetch the order so we can read its contact.
    let order = client
        .get_order(&invoice.btcpay_invoice_id)
        .await
        .context("fetch Zaprite order for profile capture")?;
    let contact_id = order
        .pointer("/contact/id")
        .or_else(|| order.get("contactId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let contact_id = match contact_id {
        Some(c) => c,
        None => {
            // Order has no contact — buyer paid without an email /
            // Zaprite didn't materialize a contact. No profile to
            // capture; renewals fall back to manual pay.
            tracing::warn!(
                sub_id = %sub_id,
                order_status = order.get("status").and_then(|v| v.as_str()).unwrap_or("?"),
                order_has_contact = order.get("contact").is_some(),
                order_has_contactId = order.get("contactId").is_some(),
                "capture: order has no contact.id / contactId — cannot capture profile. \
                 Check that buyer_email was present at purchase + that :47+ contact \
                 creation ran."
            );
            return Ok(());
        }
    };
    tracing::info!(
        sub_id = %sub_id, contact_id = %contact_id,
        "capture: resolved contact_id from order"
    );

    // 2. Fetch the contact and enumerate its payment profiles.
    let contact = client
        .get_contact(&contact_id)
        .await
        .context("fetch Zaprite contact for profile capture")?;
    let profiles = match contact.get("paymentProfiles").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => {
            tracing::warn!(
                sub_id = %sub_id, contact_id = %contact_id,
                "capture: contact has no paymentProfiles array — likely the buyer \
                 didn't check 'save card' at Zaprite checkout, OR profile creation \
                 is async on Zaprite's side and not yet visible at webhook time"
            );
            return Ok(());
        }
    };
    tracing::info!(
        sub_id = %sub_id, contact_id = %contact_id,
        profile_count = profiles.len(),
        "capture: enumerated contact's payment profiles"
    );

    // 3. Find the profile whose sourceOrder.externalUniqId is
    //    THIS invoice. Zaprite scopes saved profiles to a contact,
    //    but a contact may have multiple profiles from prior
    //    purchases (e.g. the buyer subscribed to another product
    //    too). The sourceOrder pin is how we identify the one
    //    Zaprite just minted on this purchase.
    let matching = profiles.iter().find(|p| {
        p.pointer("/sourceOrder/externalUniqId")
            .and_then(|v| v.as_str())
            .map(|s| s == invoice.id)
            .unwrap_or(false)
    });
    let profile = match matching {
        Some(p) => p,
        None => {
            // Most common reason: buyer paid with Bitcoin / Lightning
            // (no autopay-supporting rail) OR declined the save-
            // payment-profile prompt on the card form. Both are
            // legitimate; renewals fall back to manual pay.
            //
            // Also possible: race condition — Zaprite's profile-save
            // step hasn't finished by the time the order.paid webhook
            // fires. If you see this with profile_count > 0 but no
            // match for invoice.id, that's the race.
            let sample = profiles.iter().take(3).map(|p| {
                p.pointer("/sourceOrder/externalUniqId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("<none>")
                    .to_string()
            }).collect::<Vec<_>>();
            tracing::warn!(
                sub_id = %sub_id,
                contact_id = %contact_id,
                invoice_id = %invoice.id,
                profile_count = profiles.len(),
                sample_source_external_uniq_ids = ?sample,
                "capture: no profile matches sourceOrder.externalUniqId == invoice.id — \
                 either the buyer declined the save-card prompt, paid via a non-saving \
                 rail (BTC/Lightning), OR Zaprite's profile-attach is racing the \
                 webhook delivery"
            );
            return Ok(());
        }
    };

    let profile_id = match profile.get("id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            tracing::warn!(
                sub_id = %sub_id, contact_id = %contact_id,
                "capture: matched profile has no 'id' field — skipping"
            );
            return Ok(());
        }
    };
    let method = profile.get("method").and_then(|v| v.as_str()).map(|s| s.to_string());
    let expires_at = profile
        .get("expiresAt")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // 4. Persist.
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE subscriptions \
         SET zaprite_contact_id = ?, zaprite_payment_profile_id = ?, \
             zaprite_payment_profile_method = ?, \
             zaprite_payment_profile_expires_at = ?, \
             updated_at = ? \
         WHERE id = ?",
    )
    .bind(&contact_id)
    .bind(&profile_id)
    .bind(&method)
    .bind(&expires_at)
    .bind(&now)
    .bind(sub_id)
    .execute(&state.db)
    .await
    .context("UPDATE subscriptions with Zaprite profile metadata")?;

    tracing::info!(
        sub_id = %sub_id,
        contact_id = %contact_id,
        profile_id = %profile_id,
        method = method.as_deref().unwrap_or("?"),
        "captured Zaprite saved payment profile for auto-charge on renewal"
    );
    Ok(())
}

/// Attempt a merchant-initiated charge against the saved Zaprite
/// payment profile on this subscription. Called by the renewal
/// worker *after* it has created the order; this turns the order
/// from "buyer must pay" into "auto-charged, will settle via the
/// usual webhook." Returns:
///   - `Ok(true)`  — the charge call succeeded; the buyer is not
///                   expected to pay manually. The settle webhook
///                   will fire on its own and flip the sub to
///                   `active` via `on_invoice_settled`.
///   - `Ok(false)` — sub has no saved profile, or active provider
///                   isn't Zaprite. Caller proceeds with manual-pay
///                   fallback (`subscription.renewal_pending`).
///   - `Err(_)`    — Zaprite returned an error (declined card,
///                   expired profile, network blip). Caller treats
///                   this as a soft failure: log, audit, and ALSO
///                   fall through to manual-pay so the buyer has
///                   a path to recover.
async fn try_auto_charge_zaprite(
    state: &AppState,
    sub: &Subscription,
    provider_invoice_id: &str,
) -> Result<bool> {
    use crate::payment::ProviderKind;

    let profile_id = match sub.zaprite_payment_profile_id.as_deref() {
        Some(p) if !p.is_empty() => p,
        _ => return Ok(false),
    };

    let provider = state
        .payment_provider()
        .await
        .map_err(|e| anyhow!("payment provider unavailable: {e:#}"))?;
    if provider.kind() != ProviderKind::Zaprite {
        return Ok(false);
    }
    let zaprite = provider
        .as_any()
        .downcast_ref::<crate::payment::zaprite::ZapriteProvider>()
        .ok_or_else(|| anyhow!("provider.kind is Zaprite but downcast failed"))?;

    let resp = zaprite
        .client()
        .charge_order_with_profile(provider_invoice_id, profile_id)
        .await
        .context("Zaprite charge_order_with_profile")?;

    tracing::info!(
        sub_id = %sub.id,
        order_id = %provider_invoice_id,
        profile_id = %profile_id,
        order_status = resp.get("status").and_then(|v| v.as_str()).unwrap_or("?"),
        "Zaprite auto-charge succeeded; awaiting settle webhook"
    );
    Ok(true)
}
