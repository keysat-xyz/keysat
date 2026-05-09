//! Buyer-facing tier upgrade endpoints.
//!
//! Phase 3 of TIER_UPGRADES_DESIGN.md. Two endpoints:
//!
//! - `POST /v1/upgrade-quote`  — read-only quote: "what would I owe
//!                                if I upgraded to <tier>?"
//! - `POST /v1/upgrade`        — start an upgrade: creates a payment
//!                                invoice for the prorated charge,
//!                                returns the checkout URL. Webhook
//!                                handler applies the change on settle.
//!
//! Auth model matches the recovery + buyer-cancel endpoints — the
//! buyer's signed license key in the request body is the credential.
//! The daemon verifies the signature, looks up the local license row,
//! computes the quote, optionally creates an invoice. No admin token,
//! no cookie.
//!
//! Out of scope for Phase 3 (Phase 4 with admin endpoint):
//! - **Buyer-initiated recurring downgrades.** The quote function in
//!   `crate::upgrades` already returns a 0-charge quote with
//!   `effective_at = next_renewal_at`, but actually applying the
//!   change at the right moment (cycle boundary) requires renewal-
//!   worker integration. Phase 4 lands that. For now this endpoint
//!   rejects buyer downgrades with a 403 and a hint to contact support.
//! - **Admin force-change.** `POST /v1/admin/licenses/:id/change-tier`
//!   ships in Phase 4.

use crate::api::admin::request_context;
use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::payment::{CreateInvoiceParams, Money};
use axum::{
    extract::State,
    http::HeaderMap,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct QuoteReq {
    /// Buyer's signed license key. Verified before we compute anything.
    pub license_key: String,
    /// Slug of the policy the buyer wants to move to. Resolved within
    /// the license's product (cross-product changes are not supported
    /// by the quote function).
    pub target_policy_slug: String,
}

/// `POST /v1/upgrade-quote` — quote-only. No DB writes, no invoice.
/// Returns the same shape `crate::upgrades::UpgradeQuote` produces,
/// flattened to JSON for SDK consumption.
pub async fn quote(
    State(state): State<AppState>,
    Json(body): Json<QuoteReq>,
) -> AppResult<Json<Value>> {
    let (license, target_policy) = resolve_request(&state, &body.license_key, &body.target_policy_slug).await?;
    let q = crate::upgrades::compute_upgrade_quote(&state, &license, &target_policy).await?;
    Ok(Json(quote_to_json(&q)))
}

#[derive(Debug, Deserialize)]
pub struct StartReq {
    pub license_key: String,
    pub target_policy_slug: String,
    /// Optional buyer-supplied redirect target on payment-provider
    /// success. Mirrors the purchase flow's same-named field.
    #[serde(default)]
    pub redirect_url: Option<String>,
}

/// `POST /v1/upgrade` — buyer commits to the upgrade. We:
/// 1. Recompute the quote (DON'T trust client-side shaping; the
///    on-chain charge must match what the daemon's logic decides).
/// 2. Reject buyer-initiated downgrades for v0.2.x (Phase 4 ships).
/// 3. Reject zero-charge upgrades — those are admin-only (e.g., free
///    upgrades come through the comp path, not the buyer path).
/// 4. Create a provider invoice for the prorated charge.
/// 5. Persist the local invoice + tier_changes row tying them
///    together. The webhook handler picks it up on settle.
///
/// Returns `{ invoice_id, checkout_url, amount_sats }` so the SDK can
/// open the checkout URL in the buyer's browser, then poll the
/// existing `/v1/purchase/:invoice_id` to detect settle (the webhook
/// applies the change server-side).
pub async fn start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<StartReq>,
) -> AppResult<Json<Value>> {
    let (ip, ua) = request_context(&headers);
    let (license, target_policy) =
        resolve_request(&state, &body.license_key, &body.target_policy_slug).await?;

    let quote = crate::upgrades::compute_upgrade_quote(&state, &license, &target_policy).await?;

    // Phase 3 scope: buyer endpoint handles UPGRADE only. Downgrades
    // (even 0-charge ones) need the cycle-boundary apply path which
    // ships with Phase 4 admin endpoint + renewal-worker integration.
    if quote.direction == crate::upgrades::TierDirection::Downgrade {
        return Err(AppError::Forbidden);
    }

    if quote.proration_charge_value <= 0 {
        return Err(AppError::BadRequest(
            "this upgrade has no charge owed; admin must apply it as a comp via \
             POST /v1/admin/licenses/:id/change-tier (Phase 4)".into(),
        ));
    }

    // Convert proration to sats. SAT-currency licenses skip the rate
    // fetcher (identity). Fiat licenses re-quote against the live rate.
    let conversion = crate::rates::convert_to_sats(
        &state,
        &quote.listed_currency,
        quote.proration_charge_value,
    )
    .await
    .map_err(|e| AppError::Upstream(format!("rate conversion failed: {e:#}")))?;
    let amount_sats = conversion.sats.max(1);

    // Create provider invoice. Same trait method the purchase + renewal
    // paths use, so any provider-specific concerns (URL rewriting,
    // metadata enrichment) live inside the impl.
    let provider = state.payment_provider().await?;
    let internal_invoice_id = Uuid::new_v4().to_string();
    let default_redirect = format!(
        "{}/thank-you?invoice_id={}",
        state.config.public_base_url, internal_invoice_id
    );
    let redirect_url = body
        .redirect_url
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&default_redirect);

    let created = provider
        .create_invoice(CreateInvoiceParams {
            amount: Money::sats(amount_sats),
            redirect_url,
            metadata: json!({
                "productId": target_policy.product_id,
                "intent": "tier_change",
                "licenseId": license.id,
                "fromPolicyId": quote.from_policy_id,
                "toPolicyId": quote.to_policy_id,
            }),
            external_order_id: &internal_invoice_id,
            buyer_email: license.buyer_email.as_deref(),
        })
        .await
        .map_err(|e| AppError::Upstream(format!("provider create_invoice: {e:#}")))?;

    // Persist invoice. The exchange rate fields capture the conversion
    // so the receipt UI can show "you paid X sats which is $Y at the
    // moment of charge." Same shape as the regular purchase path.
    let invoice = crate::db::repo::create_invoice_with_currency(
        &state.db,
        &internal_invoice_id,
        &created.provider_invoice_id,
        &target_policy.product_id,
        amount_sats,
        &created.checkout_url,
        license.buyer_email.as_deref(),
        Some("tier upgrade"),
        Some(&quote.to_policy_id),
        Some(&quote.listed_currency),
        Some(quote.proration_charge_value),
        conversion.rate_centibps,
        Some(conversion.source.as_str()),
    )
    .await?;

    // Record the tier_change row, tied to this invoice. The webhook
    // handler looks it up by invoice_id on settle and applies.
    let effective_at = match &quote.effective_at {
        crate::upgrades::EffectiveAt::Immediate => chrono::Utc::now().to_rfc3339(),
        crate::upgrades::EffectiveAt::At(s) => s.clone(),
    };
    let tier_change_id = crate::upgrades::record_tier_change(
        &state.db,
        &license.id,
        &quote.from_policy_id,
        &quote.to_policy_id,
        quote.direction,
        &quote.listed_currency,
        quote.proration_charge_value,
        Some(&invoice.id),
        &effective_at,
        "buyer",
        None,
    )
    .await
    .map_err(AppError::Internal)?;

    // Audit row in the generic stream; tier_changes is its own
    // audit-shaped table, but audit_log is the single "what
    // happened" feed operators read from.
    let _ = crate::db::repo::insert_audit(
        &state.db,
        "buyer_license_key",
        Some(&license.id),
        "subscription.upgrade.started",
        Some("tier_change"),
        Some(&tier_change_id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({
            "license_id": license.id,
            "from_policy_id": quote.from_policy_id,
            "to_policy_id": quote.to_policy_id,
            "invoice_id": invoice.id,
            "listed_currency": quote.listed_currency,
            "proration_charge_value": quote.proration_charge_value,
            "amount_sats": amount_sats,
        }),
    )
    .await;

    Ok(Json(json!({
        "invoice_id": invoice.id,
        "provider_invoice_id": created.provider_invoice_id,
        "checkout_url": created.checkout_url,
        "amount_sats": amount_sats,
        "proration_charge_value": quote.proration_charge_value,
        "listed_currency": quote.listed_currency,
        "tier_change_id": tier_change_id,
        "from_policy_slug": quote.from_policy_slug,
        "to_policy_slug": quote.to_policy_slug,
    })))
}

/// Verify the buyer's license key, look up the local license row, and
/// resolve the target policy by slug (under the license's product).
/// Centralises the auth + lookup so quote and start handlers can
/// stay narrow. 401 on auth failure (don't leak whether the policy
/// exists), 404 on missing target.
async fn resolve_request(
    state: &AppState,
    license_key: &str,
    target_policy_slug: &str,
) -> AppResult<(crate::models::License, crate::models::Policy)> {
    let (payload, signature, signed_bytes) =
        crate::crypto::parse_key(license_key).map_err(|_| AppError::Unauthorized)?;
    crate::crypto::verify_payload(&state.keypair.verifying, &signed_bytes, &signature)
        .map_err(|_| AppError::Unauthorized)?;

    let license_id = payload.license_id.to_string();
    let license = crate::db::repo::get_license_by_id(&state.db, &license_id)
        .await?
        .ok_or(AppError::Unauthorized)?;
    if license.revoked_at.is_some() || license.suspended_at.is_some() {
        return Err(AppError::Unauthorized);
    }

    let target_policy = crate::db::repo::get_policy_by_slug(
        &state.db,
        &license.product_id,
        target_policy_slug,
    )
    .await?
    .ok_or_else(|| AppError::NotFound(format!("target policy '{target_policy_slug}'")))?;

    Ok((license, target_policy))
}

fn quote_to_json(q: &crate::upgrades::UpgradeQuote) -> Value {
    let effective_at = match &q.effective_at {
        crate::upgrades::EffectiveAt::Immediate => json!("immediate"),
        crate::upgrades::EffectiveAt::At(s) => json!(s),
    };
    json!({
        "from_policy_id": q.from_policy_id,
        "from_policy_slug": q.from_policy_slug,
        "to_policy_id": q.to_policy_id,
        "to_policy_slug": q.to_policy_slug,
        "direction": q.direction.as_str(),
        "listed_currency": q.listed_currency,
        "proration_charge_value": q.proration_charge_value,
        "effective_at": effective_at,
        "next_renewal_charge": q.next_renewal_charge,
        "next_renewal_period_days": q.next_renewal_period_days,
    })
}
