//! Subscriptions admin + buyer self-service API.
//!
//! This is the HTTP surface for the recurring-subscription state machine
//! whose underlying schema (migration 0011), helpers (`crate::subscriptions`),
//! and renewal worker live elsewhere. v0.2.x ships:
//!
//! - `GET  /v1/admin/subscriptions`       — list subscriptions (admin)
//! - `POST /v1/admin/subscriptions/:id/cancel` — operator-side cancel (admin)
//! - `POST /v1/subscriptions/cancel`      — buyer self-service cancel; auth
//!                                          is via the buyer's license key
//!                                          (no admin token, no cookie).
//!
//! The cancel paths are deliberately split: admin cancellation is a
//! fully-trusted call by the operator (e.g. customer service flow,
//! refund follow-through), while the buyer-side endpoint requires the
//! caller to prove ownership by sending the signed license key. Both
//! paths share the same downstream behavior — flip status to
//! `cancelled`, stamp `cancelled_at`, fire a `subscription.cancelled`
//! webhook, write an audit row.
//!
//! Cancellation does NOT immediately revoke the license. The buyer
//! keeps access through the end of the current billing cycle (the
//! license's `expires_at` is unchanged); the renewal worker simply
//! stops creating new invoices because its query filters for
//! `status IN ('active', 'past_due')`. This matches industry
//! convention (Stripe, Zaprite, etc.) and avoids a UX where the
//! buyer cancels mid-month and immediately loses what they paid for.

use crate::api::admin::{request_context, require_scope};
use crate::api::AppState;
use crate::error::{AppError, AppResult};
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// Filter on subscription status: 'active' | 'past_due' | 'cancelled' | 'lapsed'.
    /// Omit to get all.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: i64,
}
fn default_limit() -> i64 {
    200
}

/// `GET /v1/admin/subscriptions` — list subscriptions for the admin UI.
/// Filterable by status. Returned newest-first; renders as a table in
/// the SPA's "Subscriptions" tab with action buttons (Cancel / View).
pub async fn admin_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<Value>> {
    require_scope(&state, &headers, "subscriptions:read").await?;
    if let Some(s) = q.status.as_deref() {
        if !["active", "past_due", "cancelled", "lapsed"].contains(&s) {
            return Err(AppError::BadRequest(format!(
                "unknown status filter '{s}' (allowed: active, past_due, cancelled, lapsed)"
            )));
        }
    }
    let subs = crate::subscriptions::list_subscriptions(
        &state.db,
        q.status.as_deref(),
        q.limit,
    )
    .await
    .map_err(|e| AppError::Internal(e))?;
    // Hand-shape JSON so we can include cancelled_at consistently and
    // hide internal fields if any get added later.
    let payload: Vec<Value> = subs
        .into_iter()
        .map(|s| {
            json!({
                "id": s.id,
                "license_id": s.license_id,
                "policy_id": s.policy_id,
                "product_id": s.product_id,
                "period_days": s.period_days,
                "listed_currency": s.listed_currency,
                "listed_value": s.listed_value,
                "status": s.status,
                "started_at": s.started_at,
                "next_renewal_at": s.next_renewal_at,
                "cancelled_at": s.cancelled_at,
                "consecutive_failures": s.consecutive_failures,
            })
        })
        .collect();
    Ok(Json(json!({ "subscriptions": payload })))
}

#[derive(Debug, Deserialize, Default)]
pub struct CancelReq {
    /// Optional free-form reason (audit log only — not user-visible).
    #[serde(default)]
    pub reason: Option<String>,
}

/// `POST /v1/admin/subscriptions/:id/cancel` — admin cancellation.
///
/// Idempotent: cancelling a sub that's already cancelled (or lapsed)
/// returns 200 with `{ok: true, already: <prior_state>}`. Cancelling
/// a non-existent sub returns 404.
pub async fn admin_cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Option<Json<CancelReq>>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_scope(&state, &headers, "subscriptions:write").await?;
    let (ip, ua) = request_context(&headers);
    let reason = body.and_then(|Json(b)| b.reason).filter(|s| !s.trim().is_empty());

    let sub = crate::subscriptions::get_subscription_by_id(&state.db, &id)
        .await
        .map_err(AppError::Internal)?
        .ok_or_else(|| AppError::NotFound(format!("subscription '{id}'")))?;

    let did_cancel = crate::subscriptions::cancel_subscription(&state.db, &id)
        .await
        .map_err(AppError::Internal)?;

    if !did_cancel {
        // Already in a terminal state.
        return Ok(Json(json!({
            "ok": true,
            "already": sub.status,
            "subscription_id": id,
        })));
    }

    let _ = crate::db::repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "subscription.cancel",
        Some("subscription"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({
            "license_id": sub.license_id,
            "product_id": sub.product_id,
            "policy_id": sub.policy_id,
            "reason": reason,
            "actor": "admin",
        }),
    )
    .await;

    crate::webhooks::dispatch(
        &state,
        "subscription.cancelled",
        &json!({
            "subscription_id": id,
            "license_id": sub.license_id,
            "product_id": sub.product_id,
            "policy_id": sub.policy_id,
            "actor": "admin",
            "reason": reason,
        }),
    )
    .await;

    Ok(Json(json!({
        "ok": true,
        "subscription_id": id,
        "status": "cancelled",
    })))
}

#[derive(Debug, Deserialize)]
pub struct BuyerCancelReq {
    /// The buyer's full license key (LIC1...). Used as proof-of-ownership
    /// — we re-validate it (signature + DB row) before honoring the
    /// cancellation. There is no admin token / no cookie / no email.
    pub license_key: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// `POST /v1/subscriptions/cancel` — buyer self-service cancellation.
///
/// Authentication: the request body carries the full signed license key.
/// We decode + verify the signature, look up the license, then resolve
/// the subscription tied to that license_id. This means a cancellation
/// CAN be initiated by anyone holding the key — which is the same
/// trust model as the rest of `/v1/validate`. If the buyer has shared
/// their key, that's already a security problem they need to rotate.
///
/// Returns the same shape as the admin endpoint so SDK code paths can
/// share a parser. Fires the `subscription.cancelled` webhook with
/// `actor=buyer` so operators can distinguish self-service cancels.
pub async fn buyer_cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<BuyerCancelReq>,
) -> AppResult<Json<Value>> {
    let (ip, ua) = request_context(&headers);

    // Verify the license key against our pubkey + DB row.
    // parse_key returns (payload, signature, signed_bytes). verify_payload
    // takes the raw signed_bytes (NOT a re-serialized payload, because v1
    // keys round-trip through a different serializer and we'd break the
    // signature if we re-encoded).
    let (payload, signature, signed_bytes) =
        crate::crypto::parse_key(&body.license_key).map_err(|_| AppError::Unauthorized)?;
    crate::crypto::verify_payload(&state.keypair.verifying, &signed_bytes, &signature)
        .map_err(|_| AppError::Unauthorized)?;

    let license_id = payload.license_id.to_string();
    let license = crate::db::repo::get_license_by_id(&state.db, &license_id)
        .await?
        .ok_or(AppError::Unauthorized)?;
    if license.revoked_at.is_some() || license.suspended_at.is_some() {
        // Don't leak revocation state via a 404; treat as not-authorized.
        return Err(AppError::Unauthorized);
    }

    let sub = crate::subscriptions::get_subscription_by_license_id(&state.db, &license_id)
        .await
        .map_err(AppError::Internal)?
        .ok_or_else(|| AppError::NotFound("no subscription tied to this license".into()))?;

    let reason = body.reason.filter(|s| !s.trim().is_empty());

    let did_cancel = crate::subscriptions::cancel_subscription(&state.db, &sub.id)
        .await
        .map_err(AppError::Internal)?;

    if !did_cancel {
        return Ok(Json(json!({
            "ok": true,
            "already": sub.status,
            "subscription_id": sub.id,
        })));
    }

    let _ = crate::db::repo::insert_audit(
        &state.db,
        "buyer_license_key",
        Some(&license_id),
        "subscription.cancel",
        Some("subscription"),
        Some(&sub.id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({
            "license_id": sub.license_id,
            "product_id": sub.product_id,
            "policy_id": sub.policy_id,
            "reason": reason,
            "actor": "buyer",
        }),
    )
    .await;

    crate::webhooks::dispatch(
        &state,
        "subscription.cancelled",
        &json!({
            "subscription_id": sub.id,
            "license_id": sub.license_id,
            "product_id": sub.product_id,
            "policy_id": sub.policy_id,
            "actor": "buyer",
            "reason": reason,
        }),
    )
    .await;

    Ok(Json(json!({
        "ok": true,
        "subscription_id": sub.id,
        "status": "cancelled",
    })))
}
