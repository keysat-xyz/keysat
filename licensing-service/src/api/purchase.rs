//! Purchase flow:
//!   1. Client POSTs `/v1/purchase` with a product slug.
//!   2. We create a BTCPay invoice, stash a row, return the checkout URL.
//!   3. Client opens the URL, pays. BTCPay hits our webhook (see
//!      [`crate::api::webhook`]) which marks the invoice 'settled' and
//!      issues a license.
//!   4. Client polls `/v1/purchase/:invoice_id` until `license_key` is
//!      present, then stores it locally.

use crate::api::AppState;
use crate::btcpay::client::BtcpayClient;
use crate::crypto::{encode_key, sign_payload, LicensePayload, FLAG_TRIAL, KEY_VERSION_V2};
use crate::db::repo;
use crate::error::{AppError, AppResult};
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
}

#[derive(Debug, Serialize)]
pub struct StartPurchaseResp {
    pub invoice_id: String,           // our internal id
    pub btcpay_invoice_id: String,    // BTCPay's id (for debugging)
    pub checkout_url: String,         // URL the user opens to pay
    pub amount_sats: i64,             // what BTCPay was charged (post-discount)
    pub base_price_sats: i64,         // product list price (pre-discount)
    pub discount_applied_sats: i64,   // base - amount_sats; 0 if no code
    pub poll_url: String,             // where to check status
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

    let base_price = product.price_sats;

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
        // Note: applies_to_policy_id is informational in v0.1 — the
        // policy used at license-issuance time is the product's default.

        // Step B: atomic reserve.
        repo::try_reserve_code_slot(&state.db, &code.id).await?;

        let discount = compute_discount(&code.kind, code.amount, base_price);
        let final_price = (base_price - discount).max(MIN_INVOICE_SATS);
        (final_price, Some(code), discount)
    } else {
        (base_price, None, 0)
    };

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

    let metadata = BtcpayClient::invoice_metadata(&product.id, &internal_id);
    let btcpay = match state.btcpay_client().await {
        Ok(c) => c,
        Err(e) => {
            // Release the reserved slot if we have one — BTCPay isn't ready.
            if let Some(code) = &reservation {
                let _ = repo::release_code_slot(&state.db, &code.id).await;
            }
            return Err(e);
        }
    };

    // Step C: BTCPay invoice. On failure, release the slot and bail.
    let created = match btcpay
        .create_invoice(final_price, metadata, Some(redirect_url))
        .await
    {
        Ok(c) => c,
        Err(e) => {
            if let Some(code) = &reservation {
                let _ = repo::release_code_slot(&state.db, &code.id).await;
            }
            return Err(AppError::Upstream(format!(
                "BTCPay invoice create failed: {e}"
            )));
        }
    };

    // BTCPay returns a checkout URL using whatever URL we called its
    // API at — for us, the internal Docker hostname (fast). Rewrite
    // the host to the configured public URL so the buyer actually
    // gets a link they can open. Falls through unchanged if no public
    // URL is configured (test/dev only).
    let checkout_url = match &state.config.btcpay_public_url {
        Some(public_base) => {
            let rewritten =
                crate::payment::btcpay::rewrite_to_public(&created.checkout_link, public_base);
            tracing::info!(
                original = %created.checkout_link,
                rewritten = %rewritten,
                public_base = %public_base,
                "purchase: checkout URL rewritten for buyer"
            );
            rewritten
        }
        None => {
            tracing::warn!(
                original = %created.checkout_link,
                "purchase: checkout URL NOT rewritten — btcpay_public_url is None"
            );
            created.checkout_link.clone()
        }
    };

    // Step D: persist local invoice. On failure, release the slot.
    // Use internal_id we pre-generated (and baked into the BTCPay
    // redirect_url) as the local row id so /v1/purchase/<id> and
    // /thank-you?invoice_id=<id> all resolve to the same row.
    let invoice = match repo::create_invoice(
        &state.db,
        &internal_id,
        &created.id,
        &product.id,
        final_price,
        &checkout_url,
        req.buyer_email.as_deref(),
        req.buyer_note.as_deref(),
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
            let _ = repo::update_invoice_status(&state.db, &created.id, "invalid").await;
            return Err(e);
        }
    }

    let poll_url = format!("{}/v1/purchase/{}", state.config.public_base_url, invoice.id);

    Ok(Json(StartPurchaseResp {
        invoice_id: invoice.id,
        btcpay_invoice_id: created.id,
        checkout_url,
        amount_sats: final_price,
        base_price_sats: base_price,
        discount_applied_sats: discount_applied,
        poll_url,
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
