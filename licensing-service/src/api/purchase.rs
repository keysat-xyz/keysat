//! Purchase flow:
//!   1. Client POSTs `/v1/purchase` with a product slug.
//!   2. We create a BTCPay invoice, stash a row, return the checkout URL.
//!   3. Client opens the URL, pays. BTCPay hits our webhook (see
//!      [`crate::api::webhook`]) which marks the invoice 'settled' and
//!      issues a license.
//!   4. Client polls `/v1/purchase/:invoice_id` until `license_key` is
//!      present, then stores it locally.

use crate::api::AppState;
use crate::crypto::{encode_key, sign_payload, LicensePayload, FLAG_TRIAL, KEY_VERSION_V2};
use crate::db::repo;
use crate::error::{AppError, AppResult};
use crate::payment::{CreateInvoiceParams, Money};
use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
pub struct StartPurchaseReq {
    /// Product slug to buy.
    pub product: String,
    /// Optional email for receipt / future contact.
    pub buyer_email: Option<String>,
    /// Optional free-text note from the buyer.
    pub buyer_note: Option<String>,
    /// Optional URL the buyer should be returned to after payment.
    pub redirect_url: Option<String>,
    /// Optional discount / referral code (case-insensitive).
    pub code: Option<String>,
    /// Optional tier (policy slug). When set, the policy's
    /// `price_sats_override` becomes the base price (if defined), and the
    /// chosen policy is remembered on the invoice so it's used at license
    /// issuance time. When omitted, the daemon falls back to the product's
    /// default policy at issuance — same as pre-:27 behaviour.
    pub policy_slug: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StartPurchaseResp {
    pub invoice_id: String,           // our internal id
    /// Empty for the free-tier shortcut path (price = 0 after override/discount):
    /// we synthesize a settled invoice locally and skip BTCPay entirely.
    pub btcpay_invoice_id: String,
    /// Non-empty on the paid path. On the free path, empty — the buyer should
    /// be shown the license card directly using `license_key` below.
    pub checkout_url: String,
    pub amount_sats: i64,             // what BTCPay was charged (post-discount)
    pub base_price_sats: i64,         // product list price (pre-discount)
    pub discount_applied_sats: i64,   // base - amount_sats; 0 if no code
    pub poll_url: String,             // where to check status
    /// Set when the daemon issued the license inline (free tier or 100%-off).
    /// When present, the client should display the license card directly
    /// instead of redirecting to a BTCPay checkout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license_id: Option<String>,
}

/// Floor for invoiced amount after a discount is applied. Set to 1 sat so
/// 100%-off codes still produce a real BTCPay invoice (and the buyer
/// experiences the purchase flow). 0-sat invoices aren't always supported
/// by BTCPay anyway.
const MIN_INVOICE_SATS: i64 = 1;

pub async fn start(
    State(state): State<AppState>,
    Json(req): Json<StartPurchaseReq>,
) -> AppResult<Json<StartPurchaseResp>> {
    let product = repo::get_product_by_slug(&state.db, &req.product)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("product '{}'", req.product)))?;
    if !product.active {
        return Err(AppError::BadRequest(format!(
            "product '{}' is not available for purchase",
            req.product
        )));
    }

    // Resolve the optional tier (policy_slug). The chosen policy must be
    // active and public for it to be selectable from the public buy page.
    // (The admin can still issue under non-public policies via /v1/admin/licenses.)
    let chosen_policy = if let Some(ps) = req.policy_slug.as_deref().filter(|s| !s.is_empty()) {
        let pol = repo::get_policy_by_slug(&state.db, &product.id, ps)
            .await?
            .ok_or_else(|| {
                AppError::NotFound(format!(
                    "policy '{ps}' for product '{}'",
                    req.product
                ))
            })?;
        if !pol.active {
            return Err(AppError::BadRequest(format!(
                "policy '{ps}' is not active"
            )));
        }
        if !pol.public {
            return Err(AppError::BadRequest(format!(
                "policy '{ps}' is not available on the public buy page"
            )));
        }
        Some(pol)
    } else {
        None
    };

    // Effective base price in sats. For SAT-priced products this is
    // straightforward (policy override or product.price_sats). For
    // fiat-priced products (USD, EUR), we convert the listed value
    // to sats here using the daemon's rate fetcher — the rate gets
    // recorded on the invoice row below for audit. The CONTRACT to
    // the buyer is sat-denominated either way; the listed currency
    // is just the operator's display preference.
    //
    // We capture the listed (currency, value) and the rate-source
    // tuple so the invoice row carries the full audit trail.
    let mut listed_currency: Option<String> = None;
    let mut listed_value: Option<i64> = None;
    let mut exchange_rate_centibps: Option<i64> = None;
    let mut exchange_rate_source: Option<String> = None;

    let base_price: i64 = if product.price_currency == "SAT" {
        chosen_policy
            .as_ref()
            .and_then(|p| p.price_sats_override)
            .unwrap_or(product.price_sats)
    } else {
        // Fiat-priced. Use the policy override (in the same currency
        // as the product) if set, otherwise the product's listed
        // value. v0.3 will introduce per-policy currency overrides;
        // for now policies inherit the product's currency.
        let listed = chosen_policy
            .as_ref()
            .and_then(|p| p.price_sats_override) // legacy column; may carry override in fiat units after admin UI lands
            .unwrap_or(product.price_value);
        let conversion =
            crate::rates::convert_to_sats(&state, &product.price_currency, listed)
                .await
                .map_err(|e| AppError::Upstream(format!("rate fetch failed: {e:#}")))?;
        listed_currency = Some(product.price_currency.clone());
        listed_value = Some(listed);
        exchange_rate_centibps = conversion.rate_centibps;
        exchange_rate_source = Some(conversion.source);
        conversion.sats
    };

    // ----- Free-trial shortcut (recurring + trial_days > 0) -----
    // Before any pricing / discount logic: if the chosen policy is a
    // recurring subscription with trial_days > 0, the buyer pays
    // nothing today. We synthesize a settled free invoice, issue the
    // license inline with expires_at = now + trial_days, and create
    // the subscription row with next_renewal_at = trial_end so the
    // renewal worker fires the FIRST paid invoice when the trial
    // ends. Discount codes are deliberately ignored for trials —
    // they're already free; layering a discount on a free first
    // cycle is a no-op that just complicates the audit trail.
    if let Some(p) = chosen_policy.as_ref() {
        if p.is_recurring && p.trial_days > 0 {
            let free_invoice = repo::create_free_invoice(
                &state.db,
                &product.id,
                req.buyer_email.as_deref(),
                req.buyer_note.as_deref(),
                Some(p.id.as_str()),
            )
            .await?;

            // issue_license_for_invoice handles the recurring branch
            // (creates the subscription with next_renewal_at = trial_end)
            // because we now special-case is_recurring + trial_days
            // inside that function.
            let license_id = crate::api::webhook::issue_license_for_invoice(
                &state, &free_invoice,
            )
            .await?;

            // Re-derive the signed key.
            let lic = repo::get_license_by_invoice(&state.db, &free_invoice.id)
                .await?
                .ok_or_else(|| {
                    AppError::Internal(anyhow::anyhow!("license vanished after issue"))
                })?;
            let flags = if lic.is_trial { FLAG_TRIAL } else { 0 };
            let expires_at_unix = lic
                .expires_at
                .as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.timestamp())
                .unwrap_or(0);
            let payload = LicensePayload {
                version: KEY_VERSION_V2,
                flags,
                product_id: uuid::Uuid::parse_str(&lic.product_id)
                    .map_err(|e| AppError::Internal(anyhow::anyhow!("bad product_id: {e}")))?,
                license_id: uuid::Uuid::parse_str(&lic.id)
                    .map_err(|e| AppError::Internal(anyhow::anyhow!("bad license_id: {e}")))?,
                issued_at: chrono::DateTime::parse_from_rfc3339(&lic.issued_at)
                    .map(|t| t.timestamp())
                    .unwrap_or(0),
                expires_at: expires_at_unix,
                fingerprint_hash: [0u8; 32],
                entitlements: lic.entitlements.clone(),
            };
            let sig = sign_payload(&state.keypair.signing, &payload);
            let license_key = encode_key(&payload, &sig);

            let poll_url = format!(
                "{}/v1/purchase/{}",
                state.config.public_base_url, free_invoice.id
            );

            tracing::info!(
                product_slug = %req.product,
                policy_slug = %p.slug,
                trial_days = p.trial_days,
                license_id = %license_id,
                "trial license issued — no charge for first cycle"
            );

            return Ok(Json(StartPurchaseResp {
                invoice_id: free_invoice.id.clone(),
                btcpay_invoice_id: free_invoice.btcpay_invoice_id.clone(),
                checkout_url: String::new(),
                amount_sats: 0,
                base_price_sats: base_price,
                discount_applied_sats: 0,
                poll_url,
                license_key: Some(license_key),
                license_id: Some(license_id),
            }));
        }
    }

    // Resolve and validate the discount code if one was supplied. The
    // ordering here matters: we must atomically reserve a counter slot
    // BEFORE we create the BTCPay invoice, so that a code-cap race can't
    // result in a buyer holding a discounted live invoice for an
    // already-exhausted code.
    //
    //   step A: lookup + eligibility checks (active, expired, applies-to)
    //   step B: atomically increment used_count (try_reserve_code_slot)
    //   step C: compute discount, create BTCPay invoice
    //   step D: persist local invoice
    //   step E: insert the pending redemption row (record_pending_redemption)
    //
    // If C, D, or E fail after B succeeded, we call release_code_slot to
    // give the slot back.
    let (final_price, reservation, discount_applied) = if let Some(raw_code) =
        req.code.as_deref().filter(|s| !s.trim().is_empty())
    {
        let code = repo::get_discount_code_by_code(&state.db, raw_code)
            .await?
            .ok_or_else(|| AppError::BadRequest("unknown discount code".into()))?;
        if !code.active {
            return Err(AppError::BadRequest("discount code is disabled".into()));
        }
        if let Some(exp) = &code.expires_at {
            if let Ok(when) = chrono::DateTime::parse_from_rfc3339(exp) {
                if when.with_timezone(&chrono::Utc) < chrono::Utc::now() {
                    return Err(AppError::BadRequest("discount code has expired".into()));
                }
            }
        }
        if let Some(pid) = &code.applies_to_product_id {
            if pid != &product.id {
                return Err(AppError::BadRequest(
                    "discount code does not apply to this product".into(),
                ));
            }
        }
        // If the code is restricted to a specific policy and a tier was
        // selected, they must match. If no tier was selected, the code is
        // implicitly applied to the product's default policy at issuance
        // time, which we accept here (v0.1.0:27+).
        if let Some(restricted_pid) = &code.applies_to_policy_id {
            if let Some(chosen) = &chosen_policy {
                if restricted_pid != &chosen.id {
                    return Err(AppError::BadRequest(
                        "discount code does not apply to the selected tier".into(),
                    ));
                }
            }
        }

        // Step B: atomic reserve.
        repo::try_reserve_code_slot(&state.db, &code.id).await?;

        let discount = compute_discount(&code.kind, code.amount, base_price);
        let final_price = (base_price - discount).max(MIN_INVOICE_SATS);
        (final_price, Some(code), discount)
    } else {
        (base_price, None, 0)
    };

    // ----- Free-tier shortcut -----
    // If the post-discount, post-policy-override price came out at 0 sats
    // (price_sats_override = 0 on a "free" tier, OR a 100%-off discount on
    // a paid tier), skip BTCPay entirely. BTCPay refuses 0-sat invoices and
    // would also waste a UI step that prompts the buyer to "pay" zero. We
    // synthesize a settled invoice locally, issue the license inline, and
    // return the signed key in the response. The buy page renders the
    // license card directly.
    if final_price <= 0 {
        let free_invoice = repo::create_free_invoice(
            &state.db,
            &product.id,
            req.buyer_email.as_deref(),
            req.buyer_note.as_deref(),
            chosen_policy.as_ref().map(|p| p.id.as_str()),
        )
        .await
        .map_err(|e| {
            // If we got a code reservation earlier, release it.
            let pool = state.db.clone();
            let code = reservation.clone();
            tokio::spawn(async move {
                if let Some(c) = code {
                    let _ = repo::release_code_slot(&pool, &c.id).await;
                }
            });
            e
        })?;

        // If a discount code was applied, record the redemption.
        if let Some(code) = &reservation {
            let _ = repo::record_pending_redemption(
                &state.db,
                &code.id,
                &free_invoice.id,
                discount_applied,
                base_price,
                0,
            )
            .await;
        }

        // Issue the license. This finalizes the redemption row and fires
        // license.issued + (if applicable) code.redeemed webhooks.
        let license_id =
            crate::api::webhook::issue_license_for_invoice(&state, &free_invoice).await?;

        // Re-derive the signed key (same pattern as redeem.rs / status()).
        let lic = repo::get_license_by_invoice(&state.db, &free_invoice.id)
            .await?
            .ok_or_else(|| {
                AppError::Internal(anyhow::anyhow!("license vanished after issue"))
            })?;
        let flags = if lic.is_trial { FLAG_TRIAL } else { 0 };
        let expires_at_unix = lic
            .expires_at
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.timestamp())
            .unwrap_or(0);
        let payload = LicensePayload {
            version: KEY_VERSION_V2,
            flags,
            product_id: uuid::Uuid::parse_str(&lic.product_id)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("bad product_id: {e}")))?,
            license_id: uuid::Uuid::parse_str(&lic.id)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("bad license_id: {e}")))?,
            issued_at: chrono::DateTime::parse_from_rfc3339(&lic.issued_at)
                .map(|t| t.timestamp())
                .unwrap_or(0),
            expires_at: expires_at_unix,
            fingerprint_hash: [0u8; 32],
            entitlements: lic.entitlements.clone(),
        };
        let sig = sign_payload(&state.keypair.signing, &payload);
        let license_key = encode_key(&payload, &sig);

        let poll_url = format!(
            "{}/v1/purchase/{}",
            state.config.public_base_url, free_invoice.id
        );

        return Ok(Json(StartPurchaseResp {
            invoice_id: free_invoice.id.clone(),
            btcpay_invoice_id: free_invoice.btcpay_invoice_id.clone(), // "free-<uuid>"
            checkout_url: String::new(),                                // signal: no BTCPay
            amount_sats: 0,
            base_price_sats: base_price,
            discount_applied_sats: discount_applied,
            poll_url,
            license_key: Some(license_key),
            license_id: Some(license_id),
        }));
    }

    // Pre-allocate an internal invoice id so we can pass it to BTCPay as
    // metadata, letting us correlate webhook events back to our row even
    // before we've persisted the BTCPay invoice id.
    let internal_id = uuid::Uuid::new_v4().to_string();

    // If the caller didn't supply a redirect_url, default to our own
    // /thank-you page with the invoice id baked in. After payment
    // BTCPay sends the buyer's browser there; the page polls
    // /v1/purchase/<invoice_id> until the license is issued, then
    // renders it. Internal ID (UUID) goes in the URL so the buyer can
    // bookmark it / refresh later if they close the tab.
    let default_redirect = format!(
        "{}/thank-you?invoice_id={}",
        state.config.public_base_url, internal_id
    );
    let redirect_url = req
        .redirect_url
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&default_redirect);

    // Step C: provider-agnostic invoice creation. The trait method
    // handles provider-specific concerns (HMAC-headered request, URL
    // rewriting from internal hostname to public, metadata enrichment
    // with `orderId`/`source`) inside its impl, so this code path is
    // identical for any future provider (Zaprite, etc.). On failure,
    // release the slot and bail.
    let provider = match state.payment_provider().await {
        Ok(p) => p,
        Err(e) => {
            if let Some(code) = &reservation {
                let _ = repo::release_code_slot(&state.db, &code.id).await;
            }
            return Err(e);
        }
    };
    let created = match provider
        .create_invoice(CreateInvoiceParams {
            amount: Money::sats(final_price),
            redirect_url,
            // We pass `productId` through for any provider that exposes
            // it on its dashboard / receipt. The trait's enrichment
            // adds `orderId` (= our internal_id) and `source` so
            // webhooks can be correlated to the local invoice row.
            metadata: json!({ "productId": product.id }),
            external_order_id: &internal_id,
            buyer_email: req.buyer_email.as_deref(),
        })
        .await
    {
        Ok(handle) => handle,
        Err(e) => {
            if let Some(code) = &reservation {
                let _ = repo::release_code_slot(&state.db, &code.id).await;
            }
            return Err(AppError::Upstream(format!(
                "payment provider create-invoice failed: {e}"
            )));
        }
    };
    let checkout_url = created.checkout_url.clone();

    // Step D: persist local invoice. On failure, release the slot.
    // Use internal_id we pre-generated (and baked into the BTCPay
    // redirect_url) as the local row id so /v1/purchase/<id> and
    // /thank-you?invoice_id=<id> all resolve to the same row.
    let invoice = match repo::create_invoice_with_currency(
        &state.db,
        &internal_id,
        &created.provider_invoice_id,
        &product.id,
        final_price,
        &checkout_url,
        req.buyer_email.as_deref(),
        req.buyer_note.as_deref(),
        chosen_policy.as_ref().map(|p| p.id.as_str()),
        listed_currency.as_deref(),
        listed_value,
        exchange_rate_centibps,
        exchange_rate_source.as_deref(),
    )
    .await
    {
        Ok(inv) => inv,
        Err(e) => {
            if let Some(code) = &reservation {
                let _ = repo::release_code_slot(&state.db, &code.id).await;
            }
            return Err(e);
        }
    };

    // Step E: persist the redemption row tying the slot to the invoice.
    if let Some(code) = &reservation {
        if let Err(e) = repo::record_pending_redemption(
            &state.db,
            &code.id,
            &invoice.id,
            discount_applied,
            base_price,
            final_price,
        )
        .await
        {
            // Slot was reserved but we couldn't record the redemption.
            // Release the slot and mark the BTCPay invoice as invalid
            // locally so we don't accidentally honour it on settle.
            tracing::error!(
                code = %code.code,
                invoice_id = %invoice.id,
                error = %e,
                "failed to persist pending redemption; releasing slot \
                 and invalidating local invoice"
            );
            let _ = repo::release_code_slot(&state.db, &code.id).await;
            let _ = repo::update_invoice_status(&state.db, &created.provider_invoice_id, "invalid").await;
            return Err(e);
        }
    }

    let poll_url = format!("{}/v1/purchase/{}", state.config.public_base_url, invoice.id);

    Ok(Json(StartPurchaseResp {
        invoice_id: invoice.id,
        btcpay_invoice_id: created.provider_invoice_id,
        checkout_url,
        amount_sats: final_price,
        base_price_sats: base_price,
        discount_applied_sats: discount_applied,
        poll_url,
        license_key: None,
        license_id: None,
    }))
}

/// Apply the discount math. Returns the sats to subtract from `base`.
/// Caller is responsible for clamping the result (and for floor enforcement).
fn compute_discount(kind: &str, amount: i64, base_price_sats: i64) -> i64 {
    match kind {
        "percent" => {
            // amount is basis points (0..=10000). 5000 == 50%.
            // Multiply in i128 to avoid overflow on large sat amounts.
            let bps = amount.clamp(0, 10_000) as i128;
            let base = base_price_sats as i128;
            ((base * bps) / 10_000).max(0).min(base) as i64
        }
        "fixed_sats" => amount.max(0).min(base_price_sats),
        // 'set_price' = the buyer pays exactly `amount` sats regardless of
        // base price. Compute it as a discount: subtract enough to land at
        // `amount`. If `amount >= base_price_sats`, the code provides no
        // benefit (discount = 0).
        "set_price" => {
            let target = amount.max(0);
            if target >= base_price_sats {
                0
            } else {
                base_price_sats - target
            }
        }
        _ => 0,
    }
}

/// Polling endpoint — returns status; if settled and a license has been
/// issued, returns the signed key string.
pub async fn status(
    State(state): State<AppState>,
    Path(invoice_id): Path<String>,
) -> AppResult<Json<Value>> {
    let invoice = repo::get_invoice_by_id(&state.db, &invoice_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("invoice '{invoice_id}'")))?;

    let license = repo::get_license_by_invoice(&state.db, &invoice.id).await?;

    let license_key = match &license {
        Some(lic) if lic.status == "active" => {
            // Re-issue the encoded key deterministically from the stored
            // license row. `issued_at` is parseable as RFC3339; we reduce to
            // unix seconds. Fingerprint binding isn't done here because the
            // key is still unbound at first delivery — it'll be bound the
            // first time the app calls /v1/validate or /v1/machines/activate.
            let flags = if lic.is_trial { FLAG_TRIAL } else { 0 };
            let expires_at = lic
                .expires_at
                .as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.timestamp())
                .unwrap_or(0);
            let payload = LicensePayload {
                version: KEY_VERSION_V2,
                flags,
                product_id: uuid::Uuid::parse_str(&lic.product_id).map_err(|e| {
                    AppError::Internal(anyhow::anyhow!("bad stored product_id: {e}"))
                })?,
                license_id: uuid::Uuid::parse_str(&lic.id).map_err(|e| {
                    AppError::Internal(anyhow::anyhow!("bad stored license_id: {e}"))
                })?,
                issued_at: chrono::DateTime::parse_from_rfc3339(&lic.issued_at)
                    .map(|t| t.timestamp())
                    .unwrap_or(0),
                expires_at,
                fingerprint_hash: [0u8; 32],
                entitlements: lic.entitlements.clone(),
            };
            let sig = sign_payload(&state.keypair.signing, &payload);
            Some(encode_key(&payload, &sig))
        }
        _ => None,
    };

    Ok(Json(json!({
        "invoice_id": invoice.id,
        "status": invoice.status,
        "product_id": invoice.product_id,
        "amount_sats": invoice.amount_sats,
        "license_key": license_key,
        "license_id": license.as_ref().map(|l| l.id.clone()),
    })))
}
