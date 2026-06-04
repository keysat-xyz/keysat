//! Zaprite connect / disconnect / status admin endpoints.
//!
//! Zaprite doesn't expose an OAuth-style consent flow the way BTCPay
//! does — there's no `/authorize` redirect chain. Operators just create
//! an API key in their Zaprite dashboard and paste it in. So this
//! module is much smaller than `btcpay_authorize.rs`: a single connect
//! endpoint validates + stores the key, a disconnect endpoint wipes it,
//! a status endpoint reports state.
//!
//! Multi-merchant-profile model (migration 0020+): the connect endpoint
//! now takes a `merchant_profile_id` (defaulting to the default profile)
//! and INSERTs a row in `payment_providers` attached to that profile.
//! The disconnect endpoint takes a provider id and deletes that row.
//! Old "active provider" semantics are gone — profiles attach to
//! products explicitly.

use crate::api::admin::{request_context, require_admin};
use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::payment::zaprite::{ZapriteClient, ZapriteProvider};
use axum::{extract::State, http::HeaderMap, Json};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

const DEFAULT_BASE_URL: &str = "https://api.zaprite.com";

#[derive(Debug, Deserialize)]
pub struct ConnectReq {
    pub api_key: String,
    /// Optional override — defaults to https://api.zaprite.com. Useful
    /// for sandbox orgs (which point at a different host) or for future
    /// regional endpoints.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Optional operator-set label distinguishing this Zaprite account
    /// from other providers in the admin UI (e.g. "Recaps Zaprite" vs
    /// "Keysat Zaprite"). Defaults to "Zaprite — {merchant profile name}".
    #[serde(default)]
    pub label: Option<String>,
    /// Which merchant profile to attach this Zaprite account to. NULL =
    /// the default profile. Operators with Pro/Patron tier can name a
    /// non-default profile to set up per-business Zaprite orgs.
    #[serde(default)]
    pub merchant_profile_id: Option<String>,
}

/// `POST /v1/admin/zaprite/connect` — validate + store an API key as a
/// `payment_providers` row attached to the requested merchant profile.
/// Validates the key by calling `GET /v1/orders?limit=1` against
/// Zaprite — auth-guarded, so a 200 confirms the key works for the
/// right org. A 401 / 403 / network error short-circuits before we
/// persist anything.
pub async fn connect(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ConnectReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    crate::api::tier::enforce_zaprite_feature(&state).await?;
    let (ip, ua) = request_context(&headers);

    let api_key = req.api_key.trim().to_string();
    if api_key.is_empty() {
        return Err(AppError::BadRequest("api_key is required".into()));
    }

    // Resolve the target merchant profile. Defaults to the auto-created
    // default profile when not specified — single-profile operators
    // never see this concept.
    let profile = match req.merchant_profile_id.as_deref() {
        Some(id) => crate::merchant_profiles::get(&state.db, id)
            .await?
            .ok_or_else(|| {
                AppError::BadRequest(format!("merchant profile {id} not found"))
            })?,
        None => crate::merchant_profiles::require_default(&state.db).await?,
    };

    // Refuse if this profile already has a Zaprite provider attached —
    // the unique index on (merchant_profile_id, kind) would also catch
    // this but a clean 409 message is friendlier than a constraint error.
    let existing = crate::db::repo::list_payment_providers_for_profile(&state.db, &profile.id)
        .await?;
    if existing.iter().any(|p| p.kind == "zaprite") {
        return Err(AppError::Conflict(format!(
            "merchant profile '{}' already has a Zaprite provider attached. \
             Disconnect it first if you want to rotate the API key or switch \
             organizations, or pick a different merchant profile.",
            profile.name
        )));
    }

    let base_url = req
        .base_url
        .as_deref()
        .map(|s| s.trim().trim_end_matches('/'))
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_BASE_URL)
        .to_string();
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err(AppError::BadRequest(
            "base_url must start with http:// or https://".into(),
        ));
    }

    // Smoke-test the key before saving anything. Zaprite will 401 a
    // bad key — surface that as a clean operator-facing error rather
    // than letting it crash later in the purchase flow.
    let client = ZapriteClient::new(&base_url, &api_key);
    client.ping().await.map_err(|e| {
        AppError::Upstream(format!(
            "Zaprite key validation failed (key may be invalid or revoked): {e:#}"
        ))
    })?;

    // Persist the new payment_providers row.
    let label = req
        .label
        .as_deref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("Zaprite — {}", profile.name));
    let provider_id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    crate::db::repo::create_payment_provider(
        &state.db,
        &provider_id,
        &profile.id,
        "zaprite",
        &label,
        &api_key,
        &base_url,
        None, // webhook_id — operator configures the webhook on Zaprite's dashboard
        None, // webhook_secret — Zaprite doesn't sign webhooks
        None, // store_id — BTCPay only
        &now,
    )
    .await?;

    // If this is the very first provider on the default profile, also
    // populate the legacy state.payment singleton so back-compat call
    // sites (the few that still use state.payment_provider()) work
    // without waiting for a daemon restart. Per-product resolution
    // doesn't use this singleton.
    if profile.is_default && existing.is_empty() {
        let provider = ZapriteProvider::new(client);
        state.set_payment_provider(Arc::new(provider)).await;
    }

    let _ = crate::db::repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "payment_provider.connect",
        Some("payment_provider"),
        Some(&provider_id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({
            "kind": "zaprite",
            "merchant_profile_id": profile.id,
            "base_url": base_url,
        }),
    )
    .await;

    // The webhook URL is now path-keyed by provider id so multiple
    // Zaprite orgs (one per profile) get isolated webhook deliveries.
    // Operator pastes this exact URL into the corresponding Zaprite
    // dashboard's webhooks page.
    let webhook_url = format!(
        "{}/v1/zaprite/webhook/{}",
        state.config.public_base_url.trim_end_matches('/'),
        provider_id
    );

    Ok(Json(json!({
        "ok": true,
        "provider": "zaprite",
        "provider_id": provider_id,
        "merchant_profile_id": profile.id,
        "merchant_profile_name": profile.name,
        "label": label,
        "base_url": base_url,
        "webhook_url": webhook_url,
    })))
}

#[derive(Debug, Deserialize)]
pub struct DisconnectReq {
    /// Which provider row to disconnect. NULL = disconnect the Zaprite
    /// provider on the default profile (back-compat for the single-
    /// profile case).
    #[serde(default)]
    pub provider_id: Option<String>,
}

/// `POST /v1/admin/zaprite/disconnect` — delete the named provider
/// row (or the default-profile Zaprite row when no id is supplied).
/// Operator should also delete the corresponding webhook on Zaprite's
/// dashboard — we don't reach out to Zaprite to delete it because
/// Zaprite's webhook-management endpoints aren't on the public
/// OpenAPI we have access to.
pub async fn disconnect(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<DisconnectReq>>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    let req = body.map(|Json(b)| b).unwrap_or_default();

    let provider_id = match req.provider_id {
        Some(id) => id,
        None => {
            // Default-profile fallback: find the Zaprite provider on the
            // default profile, if any.
            let default = crate::merchant_profiles::require_default(&state.db).await?;
            let rows = crate::db::repo::list_payment_providers_for_profile(&state.db, &default.id)
                .await?;
            match rows.into_iter().find(|p| p.kind == "zaprite") {
                Some(row) => row.id,
                None => {
                    return Ok(Json(json!({
                        "ok": true,
                        "noop": true,
                        "message": "no Zaprite provider connected on the default merchant profile",
                    })));
                }
            }
        }
    };

    crate::db::repo::delete_payment_provider(&state.db, &provider_id).await?;
    // Clear the back-compat singleton if it happens to be the one we
    // just deleted. This is best-effort — the singleton may be holding
    // a different provider entirely.
    state.clear_payment_provider().await;

    let _ = crate::db::repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "payment_provider.disconnect",
        Some("payment_provider"),
        Some(&provider_id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "kind": "zaprite" }),
    )
    .await;

    Ok(Json(json!({
        "ok": true,
        "noop": false,
        "provider_id": provider_id,
        "message": "Zaprite provider disconnected. Don't forget to delete the corresponding webhook on Zaprite's side at app.zaprite.com.",
    })))
}

impl Default for DisconnectReq {
    fn default() -> Self {
        Self { provider_id: None }
    }
}

/// `GET /v1/admin/zaprite/status` — connection snapshot for the
/// default profile (back-compat with the existing admin UI's
/// payment-providers card). Multi-profile operators should use the
/// new `/v1/admin/merchant-profiles/{id}` endpoint instead, which
/// lists ALL providers across all profiles.
pub async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let default = crate::merchant_profiles::get_default(&state.db).await?;
    let connected_row = match &default {
        Some(profile) => {
            let rows = crate::db::repo::list_payment_providers_for_profile(&state.db, &profile.id)
                .await?;
            rows.into_iter().find(|p| p.kind == "zaprite")
        }
        None => None,
    };
    let webhook_url = match &connected_row {
        Some(row) => format!(
            "{}/v1/zaprite/webhook/{}",
            state.config.public_base_url.trim_end_matches('/'),
            row.id
        ),
        None => format!(
            "{}/v1/zaprite/webhook",
            state.config.public_base_url.trim_end_matches('/')
        ),
    };
    Ok(Json(json!({
        "connected": connected_row.is_some(),
        "provider_id": connected_row.as_ref().map(|r| r.id.clone()),
        "base_url": connected_row.as_ref().map(|r| r.base_url.clone()),
        "label": connected_row.as_ref().map(|r| r.label.clone()),
        "webhook_id": connected_row.as_ref().and_then(|r| r.webhook_id.clone()),
        "merchant_profile_id": default.as_ref().map(|p| p.id.clone()),
        "merchant_profile_name": default.as_ref().map(|p| p.name.clone()),
        "webhook_url": webhook_url,
        "webhook_explainer": "Zaprite doesn't sign webhook deliveries. \
            Keysat authenticates each delivery via the externalUniqId we attach \
            at order creation, so a webhook configured to ANY URL on your daemon \
            is safe even without a shared secret. Polling /v1/orders works as a \
            fallback if you don't register the webhook, but webhooks fire on \
            payment settle and let Keysat issue the license within a second \
            instead of the next reconcile-loop tick (every 60s).",
    })))
}
