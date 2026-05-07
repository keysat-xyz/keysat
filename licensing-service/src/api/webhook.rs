//! Payment-provider webhook landing endpoint.
//!
//! Generic over the active `PaymentProvider` (BTCPay today; Zaprite in
//! v0.3). The flow:
//!
//! 1. The provider POSTs an invoice status event here. We hand the raw
//!    bytes + headers to the active provider's `validate_webhook` so it
//!    can apply its own signature scheme before we trust the body.
//! 2. On `InvoiceSettled`, we mark the invoice settled AND issue a
//!    license row (if one doesn't already exist for this invoice —
//!    webhooks can be retried). Idempotency is critical.
//! 3. On other events (expired / invalid / refunded), we update status
//!    and (for refunds in v0.3) revoke the license.
//!
//! We do **not** sign and return the license key here — the key is
//! lazily re-derived from the stored license row when the buyer polls
//! `/v1/purchase/:invoice_id`. This keeps webhook handling fast and
//! means a dropped webhook response doesn't lose a key.

use crate::api::AppState;
use crate::db::repo;
use crate::error::{AppError, AppResult};
use crate::payment::ProviderWebhookEvent;
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use chrono::Utc;

pub async fn handle(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<StatusCode> {
    // Active provider validates its own webhooks (each provider has a
    // different signature scheme — BTCPay's HMAC-SHA256 in BTCPay-Sig,
    // Zaprite's TBD). On any verification failure we 401.
    let provider = state.payment_provider().await?;
    let event = provider
        .validate_webhook(&headers, &body)
        .map_err(|e| AppError::Unauthorized.tap_log(format!("webhook validation: {e:#}")))?;

    let provider_invoice_id = match event.provider_invoice_id() {
        Some(id) => id.to_string(),
        None => {
            tracing::info!("webhook event without an invoice id; acking");
            return Ok(StatusCode::OK);
        }
    };

    let new_status = match &event {
        ProviderWebhookEvent::InvoiceSettled { .. } => Some("settled"),
        ProviderWebhookEvent::InvoiceExpired { .. } => Some("expired"),
        ProviderWebhookEvent::InvoiceInvalid { .. } => Some("invalid"),
        // Refunds are a v0.3 surface; for now we treat them as a noop
        // and just ack so the provider stops retrying. Once the
        // license-revoke-on-refund flow ships, this branch flips to
        // doing the revoke + audit-entry work.
        ProviderWebhookEvent::InvoiceRefunded { .. } => {
            tracing::info!(
                provider_invoice_id = %provider_invoice_id,
                "refund webhook received; revoke-on-refund flow lands in v0.3"
            );
            return Ok(StatusCode::OK);
        }
        ProviderWebhookEvent::Other { kind, .. } => {
            tracing::info!(
                event_type = %kind,
                provider_invoice_id = %provider_invoice_id,
                "ignoring non-actionable webhook event"
            );
            return Ok(StatusCode::OK);
        }
    };

    let new_status = match new_status {
        Some(s) => s,
        None => return Ok(StatusCode::OK),
    };

    tracing::info!(
        provider = provider.kind().as_str(),
        new_status,
        provider_invoice_id = %provider_invoice_id,
        "webhook event applied"
    );

    // Persist status.
    repo::update_invoice_status(&state.db, &provider_invoice_id, new_status).await?;

    // If the invoice is going to a non-success terminal state, free any
    // discount-code slot that was reserved for it. We need the internal
    // invoice id (not the provider one) to look up the redemption.
    if matches!(new_status, "expired" | "invalid") {
        if let Ok(Some(inv)) =
            repo::get_invoice_by_btcpay_id(&state.db, &provider_invoice_id).await
        {
            if let Ok(Some(redemption)) =
                repo::get_pending_redemption_by_invoice(&state.db, &inv.id).await
            {
                if let Err(e) = repo::cancel_redemption(&state.db, &redemption.id).await {
                    tracing::warn!(
                        redemption_id = %redemption.id,
                        error = %e,
                        "failed to cancel redemption on terminal invoice; counter slot may leak"
                    );
                }
            }
        }
    }

    if new_status != "settled" {
        return Ok(StatusCode::OK);
    }

    // Find the invoice and issue a license if not already issued.
    let invoice = repo::get_invoice_by_btcpay_id(&state.db, &provider_invoice_id).await?;
    let Some(invoice) = invoice else {
        tracing::warn!(
            provider_invoice_id = %provider_invoice_id,
            "settled invoice not found in local DB; ignoring"
        );
        return Ok(StatusCode::OK);
    };

    // Idempotency: if a license already exists for this invoice, do nothing.
    if repo::get_license_by_invoice(&state.db, &invoice.id)
        .await?
        .is_some()
    {
        return Ok(StatusCode::OK);
    }

    let _license_id = issue_license_for_invoice(&state, &invoice).await?;

    Ok(StatusCode::OK)
}

/// Shared issuance path — used by both the webhook handler and the reconcile
/// loop. Pulls the invoice's associated policy (if the product has a default
/// one) and materializes a license row with the right expiry / entitlements.
pub async fn issue_license_for_invoice(
    state: &AppState,
    invoice: &crate::models::Invoice,
) -> AppResult<String> {
    // Pick the "default" policy for the product: the first active policy
    // whose slug is "default" if present, else the first active policy, else
    // none (perpetual, no entitlements, max_machines=1).
    let policies = repo::list_policies_by_product(&state.db, &invoice.product_id, true).await?;
    let policy = policies
        .iter()
        .find(|p| p.slug == "default")
        .or_else(|| policies.first())
        .cloned();

    let now = Utc::now();
    let issued_at = now.to_rfc3339();
    let duration_seconds = policy.as_ref().map(|p| p.duration_seconds).unwrap_or(0);
    let expires_at = if duration_seconds == 0 {
        None
    } else {
        Some((now + chrono::Duration::seconds(duration_seconds)).to_rfc3339())
    };
    let grace_seconds = policy.as_ref().map(|p| p.grace_seconds).unwrap_or(0);
    let max_machines = policy.as_ref().map(|p| p.max_machines).unwrap_or(1);
    let is_trial = policy.as_ref().map(|p| p.is_trial).unwrap_or(false);
    let entitlements = policy
        .as_ref()
        .map(|p| p.entitlements.clone())
        .unwrap_or_default();

    let license_id = uuid::Uuid::new_v4().to_string();
    repo::create_license(
        &state.db,
        &license_id,
        &invoice.product_id,
        Some(&invoice.id),
        &issued_at,
        &serde_json::json!({
            "source": "purchase",
            "btcpay_invoice_id": invoice.btcpay_invoice_id,
        }),
        policy.as_ref().map(|p| p.id.as_str()),
        expires_at.as_deref(),
        grace_seconds,
        max_machines,
        &entitlements,
        is_trial,
        invoice.buyer_email.as_deref(),
        None,
    )
    .await?;

    tracing::info!(
        license_id = %license_id,
        invoice_id = %invoice.id,
        policy_id = ?policy.as_ref().map(|p| &p.id),
        "license issued for settled invoice"
    );

    // Fire-and-forget Lightning tip to the policy's configured recipient,
    // if any. This never blocks issuance: errors are logged + audited inside
    // the spawned task. Skipped silently when the policy has no tip config.
    if let Some(p) = policy.as_ref() {
        if p.tip_recipient.is_some() && p.tip_pct_bps > 0 {
            crate::tipping::spawn_tip(
                state.clone(),
                license_id.clone(),
                p.clone(),
                invoice.amount_sats,
            );
        }
    }

    crate::webhooks::dispatch(
        state,
        "license.issued",
        &serde_json::json!({
            "license_id": license_id,
            "product_id": invoice.product_id,
            "invoice_id": invoice.id,
            "policy_id": policy.as_ref().map(|p| &p.id),
            "is_trial": is_trial,
            "expires_at": expires_at,
            "entitlements": entitlements,
            "source": "purchase",
        }),
    )
    .await;

    // If this invoice used a discount code, finalize the redemption row
    // (transition pending → redeemed, attach license_id) and fire a
    // `code.redeemed` webhook. Done here (rather than in the webhook
    // handler) so both the webhook path and the reconciler-recovered
    // path produce identical effects.
    if let Some(redemption) =
        repo::get_pending_redemption_by_invoice(&state.db, &invoice.id).await?
    {
        if let Err(e) =
            repo::mark_redemption_redeemed(&state.db, &redemption.id, &license_id).await
        {
            tracing::warn!(
                redemption_id = %redemption.id,
                license_id = %license_id,
                error = %e,
                "failed to mark redemption as redeemed; continuing"
            );
        }

        let code_payload = match repo::get_discount_code_by_id(&state.db, &redemption.code_id).await
        {
            Ok(Some(code)) => Some(code),
            _ => None,
        };
        let _ = repo::insert_audit(
            &state.db,
            "system",
            None,
            "code.redeemed",
            Some("discount_code"),
            Some(&redemption.code_id),
            None,
            None,
            &serde_json::json!({
                "redemption_id": redemption.id,
                "invoice_id": invoice.id,
                "license_id": license_id,
                "discount_applied_sats": redemption.discount_applied_sats,
                "base_price_sats": redemption.base_price_sats,
                "final_price_sats": redemption.final_price_sats,
            }),
        )
        .await;
        crate::webhooks::dispatch(
            state,
            "code.redeemed",
            &serde_json::json!({
                "redemption_id": redemption.id,
                "code_id": redemption.code_id,
                "code": code_payload.as_ref().map(|c| c.code.clone()),
                "license_id": license_id,
                "product_id": invoice.product_id,
                "invoice_id": invoice.id,
                "discount_applied_sats": redemption.discount_applied_sats,
                "base_price_sats": redemption.base_price_sats,
                "final_price_sats": redemption.final_price_sats,
            }),
        )
        .await;
    }

    Ok(license_id)
}

// Small helper to attach a log line to an error conversion.
trait TapLog {
    fn tap_log(self, msg: String) -> Self;
}
impl TapLog for AppError {
    fn tap_log(self, msg: String) -> Self {
        tracing::warn!("{msg}");
        self
    }
}
