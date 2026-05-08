//! Free-license code redemption — the no-BTCPay path.
//!
//! Flow for `kind = 'free_license'` codes:
//!   1. Buyer hits POST /v1/redeem with `{product, code, buyer_email?, buyer_note?}`.
//!   2. Server validates the code (active, not expired, applies-to, kind == free_license).
//!   3. Server atomically reserves a slot (try_reserve_code_slot).
//!   4. Server synthesizes a settled invoice with amount_sats = 0
//!      (so the rest of the data model — license → invoice — stays uniform).
//!   5. Server records the pending redemption row.
//!   6. Server calls the existing `issue_license_for_invoice` path which:
//!        - issues the license,
//!        - fires `license.issued`,
//!        - finalizes the redemption (pending → redeemed),
//!        - fires `code.redeemed`.
//!   7. Response includes the signed license_key so the buyer can paste it
//!      directly into your app — no polling, no BTCPay.

use crate::api::AppState;
use crate::crypto::{encode_key, sign_payload, LicensePayload, FLAG_TRIAL, KEY_VERSION_V2};
use crate::db::repo;
use crate::error::{AppError, AppResult};
use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct RedeemReq {
    /// Product slug.
    pub product: String,
    /// Redeemable code (case-insensitive).
    pub code: String,
    /// Optional email — recorded on the synthetic invoice and license for
    /// admin search and webhook payloads.
    pub buyer_email: Option<String>,
    /// Optional free-text note (recorded on invoice).
    pub buyer_note: Option<String>,
    /// Optional tier (policy slug). Same semantics as the purchase flow:
    /// when set, the chosen public+active policy is remembered on the
    /// invoice so its entitlements/expiry are baked into the license.
    pub policy_slug: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RedeemResp {
    pub license_id: String,
    pub license_key: String,
    pub invoice_id: String,
    pub redemption_id: String,
}

pub async fn redeem(
    State(state): State<AppState>,
    Json(req): Json<RedeemReq>,
) -> AppResult<Json<RedeemResp>> {
    let product = repo::get_product_by_slug(&state.db, &req.product)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("product '{}'", req.product)))?;
    if !product.active {
        return Err(AppError::BadRequest(format!(
            "product '{}' is not available for redemption",
            req.product
        )));
    }

    if req.code.trim().is_empty() {
        return Err(AppError::BadRequest("code is required".into()));
    }
    let code = repo::get_discount_code_by_code(&state.db, &req.code)
        .await?
        .ok_or_else(|| AppError::BadRequest("unknown code".into()))?;
    if !code.active {
        return Err(AppError::BadRequest("code is disabled".into()));
    }
    if code.kind != "free_license" {
        return Err(AppError::BadRequest(
            "this code requires payment — use the standard purchase flow with the code applied".into(),
        ));
    }
    if let Some(exp) = &code.expires_at {
        if let Ok(when) = chrono::DateTime::parse_from_rfc3339(exp) {
            if when.with_timezone(&chrono::Utc) < chrono::Utc::now() {
                return Err(AppError::BadRequest("code has expired".into()));
            }
        }
    }
    if let Some(pid) = &code.applies_to_product_id {
        if pid != &product.id {
            return Err(AppError::BadRequest(
                "code does not apply to this product".into(),
            ));
        }
    }

    // Resolve and validate the optional tier.
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
    if let Some(restricted_pid) = &code.applies_to_policy_id {
        if let Some(chosen) = &chosen_policy {
            if restricted_pid != &chosen.id {
                return Err(AppError::BadRequest(
                    "code does not apply to the selected tier".into(),
                ));
            }
        }
    }

    // Atomic reserve. If reserved succeeds and any subsequent step fails,
    // we release the slot so a freed slot becomes available again.
    repo::try_reserve_code_slot(&state.db, &code.id).await?;

    // Synthesize a settled, zero-amount invoice. Errors release the slot.
    let invoice = match repo::create_free_invoice(
        &state.db,
        &product.id,
        req.buyer_email.as_deref(),
        req.buyer_note.as_deref(),
        chosen_policy.as_ref().map(|p| p.id.as_str()),
    )
    .await
    {
        Ok(inv) => inv,
        Err(e) => {
            let _ = repo::release_code_slot(&state.db, &code.id).await;
            return Err(e);
        }
    };

    // Record the pending redemption row tying the slot to this invoice.
    if let Err(e) = repo::record_pending_redemption(
        &state.db,
        &code.id,
        &invoice.id,
        0, // discount_applied (whole price is "free")
        0, // base_price_sats (free)
        0, // final_price_sats
    )
    .await
    {
        let _ = repo::release_code_slot(&state.db, &code.id).await;
        return Err(e);
    }

    // Issue the license. This also finalizes the redemption (pending →
    // redeemed) and fires both `license.issued` and `code.redeemed`
    // outbound webhooks.
    let license_id = match crate::api::webhook::issue_license_for_invoice(&state, &invoice).await {
        Ok(id) => id,
        Err(e) => {
            // The invoice + redemption are persisted but the license
            // failed. Cancel the redemption so the slot is released and
            // log loudly.
            tracing::error!(
                code = %code.code,
                invoice_id = %invoice.id,
                error = %e,
                "free redemption: license issuance failed after invoice + redemption \
                 were persisted"
            );
            if let Ok(Some(red)) =
                repo::get_pending_redemption_by_invoice(&state.db, &invoice.id).await
            {
                let _ = repo::cancel_redemption(&state.db, &red.id).await;
            }
            return Err(e);
        }
    };

    // Re-derive the signed license key so we can return it to the buyer
    // directly. Mirrors the math in `purchase::status`.
    let license = repo::get_license_by_invoice(&state.db, &invoice.id)
        .await?
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("license vanished after issue")))?;
    let flags = if license.is_trial { FLAG_TRIAL } else { 0 };
    let expires_at_unix = license
        .expires_at
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&chrono::Utc).timestamp())
        .unwrap_or(0);
    let payload = LicensePayload {
        version: KEY_VERSION_V2,
        flags,
        product_id: uuid::Uuid::parse_str(&license.product_id)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("bad stored product_id: {e}")))?,
        license_id: uuid::Uuid::parse_str(&license.id)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("bad stored license_id: {e}")))?,
        issued_at: chrono::DateTime::parse_from_rfc3339(&license.issued_at)
            .map(|t| t.timestamp())
            .unwrap_or(0),
        expires_at: expires_at_unix,
        fingerprint_hash: [0u8; 32],
        entitlements: license.entitlements.clone(),
    };
    let sig = sign_payload(&state.keypair.signing, &payload);
    let license_key = encode_key(&payload, &sig);

    // The redemption row was finalized inside issue_license_for_invoice;
    // re-fetch to surface its id in the response.
    let redemption_id = repo::list_redemptions_by_code(&state.db, &code.id)
        .await
        .ok()
        .and_then(|rows| rows.into_iter().find(|r| r.invoice_id == invoice.id).map(|r| r.id))
        .unwrap_or_default();

    Ok(Json(RedeemResp {
        license_id,
        license_key,
        invoice_id: invoice.id,
        redemption_id,
    }))
}
