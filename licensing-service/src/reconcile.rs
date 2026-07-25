//! Invoice reconciliation background task.
//!
//! Webhooks are the primary signal from BTCPay to us — fast, push-based, and
//! authenticated with HMAC. But webhooks can be dropped (network blips, our
//! service restarting during a burst, BTCPay retry-budget exhaustion on a
//! long outage). If we only ever reacted to webhooks, a dropped settle
//! notification would mean a buyer paid and never got their license.
//!
//! Reconciliation closes that gap. Every N seconds we scan our own table
//! for invoices still in `pending` status that were created recently, ask
//! BTCPay directly what their real state is, and reconcile:
//!
//! - BTCPay says `Settled` → mark settled AND issue a license if one
//!   doesn't exist yet (idempotency enforced by the UNIQUE index on
//!   `licenses.invoice_id`).
//! - BTCPay says `Expired` / `Invalid` → mark accordingly, don't issue.
//! - BTCPay still says `New` / `Processing` → leave it alone.
//!
//! The task is cheap — one DB query and at most N HTTP calls per tick —
//! and bounded (we only look at invoices younger than MAX_AGE_HOURS).

use crate::api::AppState;
use crate::db::repo;
use crate::payment::health;
use std::time::{Duration, SystemTime};
use tokio::time::sleep;

const TICK: Duration = Duration::from_secs(60);
const MAX_AGE_HOURS: i64 = 72;

/// How long the whole liveness-probe phase may hold up the invoice sweep.
///
/// The probe runs **ahead of** reconciliation and probes providers one at a
/// time, each bounded only by the clients' own 15-second reqwest timeout — so
/// without a ceiling the phase costs up to N×15s, and every provider claims its
/// slot on the same tick, which resynchronizes them into one slow tick every
/// 15 minutes rather than spreading the cost out. At one or two providers that
/// is noise, but `unlimited_merchant_profiles` makes larger N reachable, and
/// past roughly 60 providers the phase would exceed
/// [`PROBE_INTERVAL`](health::PROBE_INTERVAL) and starve the invoice sweep
/// permanently.
///
/// Half a `TICK` bounds it at a constant regardless of N, and leaves the other
/// half for the reconciliation the money path actually depends on. Providers
/// the budget cut off are simply not reached this tick; they claim their slots
/// on a later one, which also de-synchronizes them. Money reconciliation is
/// never delayed by more than this, whatever the operator has connected.
const PROBE_PHASE_BUDGET: Duration = Duration::from_secs(TICK.as_secs() / 2);

pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        // Small initial delay so we don't race startup logs.
        sleep(Duration::from_secs(15)).await;
        loop {
            if let Err(e) = tick(&state).await {
                tracing::warn!(error = %e, "reconciliation tick failed");
            }
            sleep(TICK).await;
        }
    });
}

/// One reconciliation pass, on the wall clock.
///
/// `pub` so tests can drive it directly, matching `webhooks::tick` and
/// `subscriptions::tick`. It was private until the liveness probe landed, and
/// the probe is exactly the part that cannot be proven any other way: its whole
/// purpose is to run when there is nothing else to do, so a test has to see an
/// idle tick from the outside.
pub async fn tick(state: &AppState) -> anyhow::Result<()> {
    tick_at(state, SystemTime::now()).await
}

/// [`tick`] with the clock injected, for the probe throttle.
///
/// The throttle spans fifteen minutes, so a test that could not move the clock
/// would have to sleep through them (`tokio`'s `full` feature excludes
/// `test-util`, so `time::pause` is unavailable — see `payment::health`). `now`
/// is threaded only into [`health::claim_probe_slot`]; everything else here
/// keeps taking its own timestamps, including the health sink inside each
/// client, which stamps the call it actually observed.
pub async fn tick_at(state: &AppState, now: SystemTime) -> anyhow::Result<()> {
    // FIRST, and above the early return below. On an instance between sales
    // `list_pending_invoices` is empty, so everything past that point is
    // skipped and the daemon makes no provider calls at all — which is the
    // window a revoked key would otherwise survive until a buyer hit checkout.
    // Probing here is the entire point of the step; moving this call below the
    // early return restores that blind spot silently, so a test drives an idle
    // tick and asserts the probe still happened.
    //
    // It sits in front of the money path, so it gets a hard ceiling — see
    // PROBE_PHASE_BUDGET. Deliberately `timeout` rather than `tokio::spawn`:
    // spawning would remove the delay entirely, but it would also make "this
    // tick probed every due provider" stop being true when `tick` returns,
    // and that postcondition is what makes the probe observable from a test
    // at all. A bounded wait keeps the guarantee and caps the cost.
    if tokio::time::timeout(PROBE_PHASE_BUDGET, probe_providers(state, now))
        .await
        .is_err()
    {
        // The unprobed providers never claimed their slots, so they are picked
        // up by a later tick rather than skipped for a full interval.
        tracing::warn!(
            budget_secs = PROBE_PHASE_BUDGET.as_secs(),
            "liveness probe phase hit its budget; remaining providers deferred \
             to a later tick"
        );
    }

    // Provider-agnostic. Each provider's impl handles the
    // provider-specific status-string normalization (BTCPay's
    // "Settled"/"Complete"/"Expired"/"Invalid" → ProviderInvoiceStatus
    // enum); this loop just operates on the typed result.
    //
    // With multi-provider, each pending invoice is reconciled against
    // its OWN provider (recorded on the invoice row, migration 0021).
    // We can't iterate against a single global provider because the
    // operator may have multiple providers configured across multiple
    // merchant profiles. Pre-0021 invoices that slipped through with
    // a NULL provider id fall back to the legacy `payment_provider()`
    // accessor (which the migration's backfill should prevent from
    // ever being needed in practice).
    let pending = repo::list_pending_invoices(&state.db, MAX_AGE_HOURS)
        .await
        .map_err(|e| anyhow::anyhow!("listing pending invoices: {e:?}"))?;
    if pending.is_empty() {
        return Ok(());
    }

    tracing::debug!(count = pending.len(), "reconciling pending invoices");

    for inv in pending {
        let provider = match inv.payment_provider_id.as_deref() {
            Some(pid) => match state.payment_provider_by_id(pid).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        invoice_id = %inv.id,
                        provider_id = pid,
                        "reconciler skipping invoice — its provider is unavailable"
                    );
                    continue;
                }
            },
            None => match state.payment_provider().await {
                Ok(p) => p,
                Err(_) => continue, // not configured yet — skip silently
            },
        };
        match provider.get_invoice_status(&inv.btcpay_invoice_id).await {
            Ok(snapshot) => {
                use crate::payment::ProviderInvoiceStatus::*;
                let new_status = match snapshot.status {
                    Settled => "settled",
                    Expired => "expired",
                    Invalid => "invalid",
                    // Pending stays pending; Refunded is a v0.3 surface
                    // that the webhook handler also short-circuits on.
                    Pending | Refunded => continue,
                };

                if new_status == inv.status.as_str() {
                    continue; // no-op
                }

                if let Err(e) = repo::update_invoice_status(
                    &state.db,
                    &inv.btcpay_invoice_id,
                    new_status,
                )
                .await
                {
                    tracing::warn!(
                        error = %e,
                        btcpay_invoice_id = %inv.btcpay_invoice_id,
                        "reconciler failed to update invoice status"
                    );
                    continue;
                }

                // Free any reserved discount-code slot if the invoice
                // entered a terminal failure state.
                if matches!(new_status, "expired" | "invalid") {
                    if let Ok(Some(redemption)) =
                        repo::get_pending_redemption_by_invoice(&state.db, &inv.id).await
                    {
                        let _ = repo::cancel_redemption(&state.db, &redemption.id).await;
                    }
                }

                if new_status == "settled" {
                    // Same advisory amount tripwire the webhook path applies
                    // (see crate::api::webhook::audit_settle_amount). Never
                    // blocks issuance — logs + audits any amount/currency
                    // drift from what we charged.
                    crate::api::webhook::audit_settle_amount(
                        state,
                        &inv,
                        snapshot.amount.as_ref(),
                    )
                    .await;
                    if let Err(e) = ensure_license(state, &inv).await {
                        tracing::warn!(
                            error = %e,
                            btcpay_invoice_id = %inv.btcpay_invoice_id,
                            "reconciler failed to issue license after recovered settle"
                        );
                    } else {
                        tracing::info!(
                            btcpay_invoice_id = %inv.btcpay_invoice_id,
                            "reconciler issued license for recovered settled invoice"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    btcpay_invoice_id = %inv.btcpay_invoice_id,
                    "reconciler failed to fetch invoice from BTCPay"
                );
            }
        }
    }
    Ok(())
}

/// Ask every connected provider, at most once per
/// [`PROBE_INTERVAL`](health::PROBE_INTERVAL), whether its API key still
/// authenticates.
///
/// Never fails the tick. A probe is a question, and every way of failing to ask
/// it — the provider list not reading, a row that will not build, the call
/// erroring — is either already recorded (the client records off the HTTP
/// status, before this function ever sees a `Result`) or is not evidence about
/// the key at all. Aborting reconciliation over one would trade a working
/// invoice sweep for a failed health check.
async fn probe_providers(state: &AppState, now: SystemTime) {
    let rows = match repo::list_all_payment_providers(&state.db).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::debug!(error = %e, "liveness probe could not list payment providers");
            return;
        }
    };

    for row in rows {
        // Claim before building anything: the slot is spent on the *attempt*,
        // so a provider that cannot be built, or cannot be reached, is retried
        // on the throttled cadence rather than on every 60-second tick.
        if !health::claim_probe_slot(&state.provider_health, &row.id, now) {
            continue;
        }
        // Uses the row already in hand rather than re-reading it, and honors
        // the `provider_override` test seam. It never names the health map, so
        // it cannot bind the wrong one — see the Step 2b note about main.rs.
        let provider = match state.provider_from_row(&row) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    provider_id = %row.id,
                    "liveness probe skipping provider — could not build it"
                );
                continue;
            }
        };
        if let Err(e) = provider.probe_auth().await {
            // Debug, not warn: a genuinely revoked key is reported by the
            // health summary from the streak this call just fed, and a probe
            // against a provider having a bad minute is not news.
            tracing::debug!(
                error = %e,
                provider_id = %row.id,
                kind = %row.kind,
                "provider liveness probe failed"
            );
        }
    }
}

async fn ensure_license(
    state: &AppState,
    invoice: &crate::models::Invoice,
) -> anyhow::Result<()> {
    if repo::get_license_by_invoice(&state.db, &invoice.id)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?
        .is_some()
    {
        // Even if the license already exists, the reconciler may be
        // running because the webhook never delivered. In that case
        // `on_invoice_settled` (which runs the Zaprite-saved-profile
        // capture for recurring first-cycle subs) never fired either.
        // Try the post-settle hook now — it's idempotent (early-returns
        // if the sub already has a captured profile, or if the active
        // provider isn't Zaprite, or if no matching profile exists on
        // the contact). Without this, a subscription created via the
        // reconciler path never gets its `zaprite_payment_profile_id`
        // populated, and renewals fall back to manual-pay forever
        // even though the saved profile is sitting on Zaprite's side.
        if let Err(e) =
            crate::subscriptions::on_invoice_settled(state, invoice).await
        {
            tracing::warn!(
                error = %e,
                invoice_id = %invoice.id,
                "reconciler post-settle hook failed (non-fatal — license already exists)"
            );
        }
        return Ok(());
    }
    crate::api::webhook::issue_license_for_invoice(state, invoice)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;

    // Same rationale as the early-return branch above — if the
    // reconciler is running, the webhook may have missed; run the
    // post-settle hook so a brand-new recurring sub also captures its
    // Zaprite saved profile. issue_license_for_invoice already created
    // the subscription row by this point, so on_invoice_settled can
    // find it.
    if let Err(e) =
        crate::subscriptions::on_invoice_settled(state, invoice).await
    {
        tracing::warn!(
            error = %e,
            invoice_id = %invoice.id,
            "reconciler post-settle hook failed (non-fatal — license issued ok)"
        );
    }
    Ok(())
}
