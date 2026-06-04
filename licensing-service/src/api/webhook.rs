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

    // Tier-change branch: this settled invoice may be a tier upgrade
    // (recorded by POST /v1/upgrade or the future admin-change-tier
    // endpoint) rather than a fresh purchase or a subscription
    // renewal. If so, apply the change against the existing license
    // — DON'T issue a new license — and short-circuit the rest.
    if let Some(tier_change) =
        crate::upgrades::get_tier_change_by_invoice(&state.db, &invoice.id)
            .await
            .map_err(AppError::Internal)?
    {
        return apply_tier_change_on_settle(&state, &invoice, &tier_change).await;
    }

    // If this settled invoice is associated with a subscription
    // (renewal cycle), flip the sub back to `active` and fire
    // `subscription.renewed`. Idempotent — re-running on a sub
    // already in `active` state is a no-op UPDATE. Runs BEFORE
    // the license-issuance branch so the sub state is correct
    // even on first-cycle subs (where the license is also being
    // issued for the first time).
    if let Err(e) = crate::subscriptions::on_invoice_settled(&state, &invoice).await {
        tracing::warn!(
            invoice_id = %invoice.id,
            error = %e,
            "subscriptions::on_invoice_settled failed; non-fatal, license issuance proceeds"
        );
    }

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
    // Tiered pricing (v0.1.0:27+): if the invoice carries a `policy_id`, the
    // buyer chose a specific tier on /buy/<slug>. Use that policy verbatim
    // (its entitlements, expiry, max_machines, trial flag get baked into the
    // license). Otherwise fall back to the legacy default-pick: first
    // active policy whose slug is "default", else the first active, else
    // no policy (perpetual, no entitlements, max_machines=1).
    let policy = if let Some(pid) = invoice.policy_id.as_deref() {
        repo::get_policy_by_id(&state.db, pid).await?
    } else {
        let policies = repo::list_policies_by_product(&state.db, &invoice.product_id, true).await?;
        policies
            .iter()
            .find(|p| p.slug == "default")
            .or_else(|| policies.first())
            .cloned()
    };

    let now = Utc::now();
    let issued_at = now.to_rfc3339();
    // For recurring policies with a free-trial period, the FIRST license's
    // expires_at is the trial end, not the policy's duration_seconds. The
    // renewal worker will extend on settle when the buyer pays the first
    // real cycle. trial_days = 0 (no trial) falls through to the regular
    // duration_seconds path.
    let is_recurring = policy.as_ref().map(|p| p.is_recurring).unwrap_or(false);
    let trial_days = policy.as_ref().map(|p| p.trial_days).unwrap_or(0);
    let duration_seconds = if is_recurring && trial_days > 0 {
        trial_days * 86_400
    } else {
        policy.as_ref().map(|p| p.duration_seconds).unwrap_or(0)
    };
    let expires_at = if duration_seconds == 0 {
        None
    } else {
        Some((now + chrono::Duration::seconds(duration_seconds)).to_rfc3339())
    };
    let grace_seconds = policy.as_ref().map(|p| p.grace_seconds).unwrap_or(0);
    let max_machines = policy.as_ref().map(|p| p.max_machines).unwrap_or(1);
    // For trial recurring licenses, set the TRIAL flag on the signed
    // payload too, so SDKs can render "trial — N days remaining".
    let is_trial =
        policy.as_ref().map(|p| p.is_trial).unwrap_or(false) || (is_recurring && trial_days > 0);
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

    // Recurring policy: create the subscription row that the renewal
    // worker uses as its source of truth. Uses the invoice's listed
    // currency + value if available (multi-currency support); falls
    // back to SAT + invoice.amount_sats for legacy / SAT-only setups.
    //
    // First-cycle scheduling:
    //   - trial_days > 0: next_renewal_at = now + trial_days. When the
    //     trial ends, the renewal worker creates the FIRST paid invoice.
    //   - trial_days = 0: next_renewal_at = now + period_days. Buyer
    //     already paid for the current cycle; renewal worker creates
    //     the second cycle's invoice when this one ends.
    if let Some(p) = policy.as_ref() {
        if p.is_recurring {
            let period_days = p.renewal_period_days.max(1);
            let first_cycle_days = if p.trial_days > 0 { p.trial_days } else { period_days };
            let listed_currency = invoice
                .listed_currency
                .clone()
                .unwrap_or_else(|| "SAT".to_string());
            let listed_value = invoice
                .listed_value
                .unwrap_or(invoice.amount_sats);
            let existing = crate::subscriptions::get_subscription_by_license_id(
                &state.db,
                &license_id,
            )
            .await
            .ok()
            .flatten();
            if existing.is_none() {
                // Snapshot the merchant profile + payment provider that
                // settled this purchase, so the renewal worker uses the
                // SAME business + payment account on subsequent cycles
                // even if the operator later moves the product to a
                // different profile. Falls back to the product's
                // current profile (and the invoice's recorded provider)
                // when the snapshot fields aren't already on the invoice.
                let snapshot_profile_id = crate::db::repo::get_merchant_profile_for_product(
                    &state.db, &invoice.product_id,
                )
                .await
                .ok()
                .flatten()
                .map(|p| p.id);
                let snapshot_provider_id = invoice.payment_provider_id.clone();
                match crate::subscriptions::create_subscription(
                    &state.db,
                    &license_id,
                    &p.id,
                    &invoice.product_id,
                    period_days,
                    &listed_currency,
                    listed_value,
                    &invoice.id,
                    snapshot_profile_id.as_deref(),
                    snapshot_provider_id.as_deref(),
                )
                .await
                {
                    Ok(sub) => {
                        // Override next_renewal_at to the first-cycle window.
                        // create_subscription defaults to now + period_days;
                        // for trials we want now + trial_days. Cheap UPDATE.
                        if first_cycle_days != period_days {
                            let trial_end = (Utc::now()
                                + chrono::Duration::days(first_cycle_days))
                                .to_rfc3339();
                            let _ = sqlx::query(
                                "UPDATE subscriptions SET next_renewal_at = ?, \
                                 updated_at = ? WHERE id = ?",
                            )
                            .bind(&trial_end)
                            .bind(&trial_end)
                            .bind(&sub.id)
                            .execute(&state.db)
                            .await;
                        }
                        tracing::info!(
                            license_id = %license_id,
                            policy_id = %p.id,
                            period_days,
                            first_cycle_days,
                            listed_currency,
                            listed_value,
                            trial = (first_cycle_days != period_days),
                            "subscription created for recurring purchase"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            license_id = %license_id,
                            policy_id = %p.id,
                            error = %e,
                            "failed to create subscription row on recurring purchase; \
                             license issued but renewal worker will not pick this up"
                        );
                    }
                }
            }
        }
    }

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

/// Webhook-side handler for a settled tier-change invoice. Idempotent:
/// if the license is already on the target tier (re-delivered webhook),
/// the UPDATE is a no-op and we still ack 200.
async fn apply_tier_change_on_settle(
    state: &AppState,
    invoice: &crate::models::Invoice,
    tier_change: &crate::upgrades::TierChangeRow,
) -> AppResult<StatusCode> {
    // Resolve the bits we need: the license, the target policy, and
    // the product (so apply_tier_change can compute the new
    // listed_value for the subscription if any).
    let license = repo::get_license_by_id(&state.db, &tier_change.license_id)
        .await?
        .ok_or_else(|| {
            AppError::Internal(anyhow::anyhow!(
                "tier_change references missing license '{}'",
                tier_change.license_id
            ))
        })?;
    let target_policy = repo::get_policy_by_id(&state.db, &tier_change.to_policy_id)
        .await?
        .ok_or_else(|| {
            AppError::Internal(anyhow::anyhow!(
                "tier_change references missing target policy '{}'",
                tier_change.to_policy_id
            ))
        })?;
    let product = repo::get_product_by_id(&state.db, &target_policy.product_id)
        .await?
        .ok_or_else(|| {
            AppError::Internal(anyhow::anyhow!(
                "target policy references missing product '{}'",
                target_policy.product_id
            ))
        })?;

    // Idempotency: if the license's policy_id already matches the
    // target, the change has already been applied by an earlier
    // webhook delivery. Ack and move on.
    if license.policy_id.as_deref() == Some(target_policy.id.as_str()) {
        tracing::info!(
            license_id = %license.id,
            tier_change_id = %tier_change.id,
            "tier-change already applied (idempotent re-delivery); acking"
        );
        return Ok(StatusCode::OK);
    }

    // Apply the change.
    crate::upgrades::apply_tier_change(&state.db, &license.id, &target_policy, &product)
        .await
        .map_err(AppError::Internal)?;

    let _ = repo::insert_audit(
        &state.db,
        "system",
        None,
        "subscription.upgrade.applied",
        Some("tier_change"),
        Some(&tier_change.id),
        None,
        None,
        &serde_json::json!({
            "license_id": license.id,
            "from_policy_id": tier_change.from_policy_id,
            "to_policy_id": tier_change.to_policy_id,
            "invoice_id": invoice.id,
            "actor": tier_change.actor,
            "direction": tier_change.direction,
        }),
    )
    .await;

    crate::webhooks::dispatch(
        state,
        "license.tier_changed",
        &serde_json::json!({
            "license_id": license.id,
            "product_id": product.id,
            "from_policy_id": tier_change.from_policy_id,
            "to_policy_id": tier_change.to_policy_id,
            "to_policy_slug": target_policy.slug,
            "direction": tier_change.direction,
            "actor": tier_change.actor,
            "invoice_id": invoice.id,
            "tier_change_id": tier_change.id,
        }),
    )
    .await;

    tracing::info!(
        license_id = %license.id,
        from_policy_id = %tier_change.from_policy_id,
        to_policy_id = %tier_change.to_policy_id,
        invoice_id = %invoice.id,
        "tier change applied on settle"
    );

    Ok(StatusCode::OK)
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
