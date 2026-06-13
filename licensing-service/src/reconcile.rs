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
use std::time::Duration;
use tokio::time::sleep;

const TICK: Duration = Duration::from_secs(60);
const MAX_AGE_HOURS: i64 = 72;

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

async fn tick(state: &AppState) -> anyhow::Result<()> {
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
