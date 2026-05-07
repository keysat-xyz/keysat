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
    let btcpay = match state.btcpay_client().await {
        Ok(c) => c,
        Err(_) => return Ok(()), // not configured yet — skip silently
    };

    let pending = repo::list_pending_invoices(&state.db, MAX_AGE_HOURS)
        .await
        .map_err(|e| anyhow::anyhow!("listing pending invoices: {e:?}"))?;
    if pending.is_empty() {
        return Ok(());
    }

    tracing::debug!(count = pending.len(), "reconciling pending invoices");

    for inv in pending {
        match btcpay.get_invoice(&inv.btcpay_invoice_id).await {
            Ok(raw) => {
                let status_str = raw
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let normalized = match status_str.as_str() {
                    "Settled" | "Complete" => Some("settled"),
                    "Expired" => Some("expired"),
                    "Invalid" => Some("invalid"),
                    // still in flight
                    _ => None,
                };
                let Some(new_status) = normalized else { continue };

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
        return Ok(());
    }
    crate::api::webhook::issue_license_for_invoice(state, invoice)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(())
}
