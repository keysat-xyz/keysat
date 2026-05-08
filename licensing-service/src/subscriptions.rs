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
//!
//! Cancellation can flip from `active` or `past_due` → `cancelled`
//! at any point (admin or buyer-initiated). Cancelled subs stop
//! the worker from picking them up, but the license stays valid
//! through the end of the current cycle.
//!
//! Auto-charge via saved payment profiles (Zaprite's
//! `paymentProfileId` flow) is NOT in this version. The first
//! renewal-worker iteration creates fresh invoices that the buyer
//! pays manually. v0.2.0:5+ adds the auto-charge path so cycles
//! after the first don't require buyer interaction.

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
    }
}

const SUB_COLS: &str = "id, license_id, policy_id, product_id, period_days, \
    listed_currency, listed_value, status, started_at, next_renewal_at, \
    cancelled_at, consecutive_failures";

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
                s.next_renewal_at, s.cancelled_at, s.consecutive_failures \
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
    })
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

    // 9. Webhook event: operator's app gets notified that a
    //    renewal invoice exists and the buyer needs to pay.
    crate::webhooks::dispatch(
        state,
        "subscription.renewal_pending",
        &json!({
            "subscription_id": sub.id,
            "license_id": sub.license_id,
            "invoice_id": internal_invoice_id,
            "checkout_url": handle.checkout_url,
            "amount_sats": amount_sats,
            "listed_currency": sub.listed_currency,
            "listed_value": sub.listed_value,
            "cycle_number": next_cycle_num,
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
