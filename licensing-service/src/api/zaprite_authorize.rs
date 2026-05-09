//! Zaprite connect / disconnect / status admin endpoints.
//!
//! Zaprite doesn't expose an OAuth-style consent flow the way
//! BTCPay does — there's no `/authorize` redirect chain. Operators
//! just create an API key in their Zaprite dashboard and paste it
//! in. So this module is much smaller than `btcpay_authorize.rs`:
//! a single connect endpoint validates + stores the key, a
//! disconnect endpoint wipes it, a status endpoint reports state.
//!
//! The active provider on `AppState` is swapped atomically as part
//! of connect/disconnect so request handlers immediately see the
//! new state without a daemon restart.

use crate::api::admin::{request_context, require_admin};
use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::payment::zaprite::{
    config as zaprite_config, ZapriteClient, ZapriteProvider,
};
use axum::{extract::State, http::HeaderMap, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

const DEFAULT_BASE_URL: &str = "https://api.zaprite.com";

#[derive(Debug, Deserialize)]
pub struct ConnectReq {
    pub api_key: String,
    /// Optional override — defaults to https://api.zaprite.com.
    /// Useful for sandbox orgs that point at a different host or
    /// for future regional endpoints.
    #[serde(default)]
    pub base_url: Option<String>,
}

/// `POST /v1/admin/zaprite/connect` — validate + store an API
/// key, then swap the active payment provider to Zaprite. The
/// operator pastes the key from
/// `app.zaprite.com/.../settings/api`.
///
/// Validates the key by calling `GET /v1/orders?limit=1` against
/// Zaprite — auth-guarded, so a 200 confirms the key works for
/// the right org. A 401 / 403 / network error short-circuits
/// before we persist anything.
pub async fn connect(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ConnectReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);

    let api_key = req.api_key.trim().to_string();
    if api_key.is_empty() {
        return Err(AppError::BadRequest("api_key is required".into()));
    }

    // Short-circuit: refuse to overwrite an existing config silently.
    // Operators get confused when they re-run Connect after already
    // being connected — they expect a "you're already set up" message,
    // not a form re-prompt that can clobber their working config.
    if let Ok(Some(_)) = zaprite_config::load(&state.db).await {
        return Err(AppError::Conflict(
            "Zaprite is already connected. Run 'Disconnect Zaprite' first \
             if you want to rotate the API key or switch organizations."
                .into(),
        ));
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

    // Smoke-test the key before saving anything. Zaprite will
    // 401 a bad key — surface that as a clean operator-facing
    // error rather than letting it crash later in the purchase
    // flow.
    let client = ZapriteClient::new(&base_url, &api_key);
    client.ping().await.map_err(|e| {
        AppError::Upstream(format!(
            "Zaprite key validation failed (key may be invalid or revoked): {e:#}"
        ))
    })?;

    // Persist + swap.
    zaprite_config::save(
        &state.db,
        &zaprite_config::ZapriteConfig {
            api_key: api_key.clone(),
            base_url: base_url.clone(),
            webhook_id: None, // operator configures the webhook in Zaprite's dashboard
        },
    )
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("save zaprite_config: {e:#}")))?;

    let provider = ZapriteProvider::new(client);
    state
        .set_payment_provider(Arc::new(provider))
        .await;
    // Persist the operator's preference so the boot-time loader
    // picks Zaprite on next restart, even if BTCPay's config row
    // is also still in the DB.
    crate::payment::write_active_provider_preference(
        &state.db,
        crate::payment::ProviderKind::Zaprite,
    )
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("write provider preference: {e:#}")))?;

    let _ = crate::db::repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "zaprite.connect",
        Some("payment_provider"),
        Some("zaprite"),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "base_url": base_url }),
    )
    .await;

    // Compute the absolute webhook URL so the StartOS Action can
    // surface the full https://... endpoint to the operator. They
    // paste this into the Zaprite dashboard exactly. Zaprite's
    // webhook form requires a full URL, not a path; the previous
    // copy showed a placeholder which was confusing.
    let webhook_url = format!(
        "{}/v1/zaprite/webhook",
        state.config.public_base_url.trim_end_matches('/')
    );

    Ok(Json(json!({
        "ok": true,
        "provider": "zaprite",
        "base_url": base_url,
        "webhook_url": webhook_url,
    })))
}

/// `POST /v1/admin/zaprite/disconnect` — wipe the stored key,
/// clear the active provider. Operator should also delete the
/// corresponding webhook on Zaprite's side, but we don't reach
/// out to Zaprite to delete it — the operator uses Zaprite's
/// dashboard for that. We can't delete it programmatically because
/// Zaprite's webhook-management endpoints aren't on the public
/// OpenAPI we have access to.
pub async fn disconnect(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);

    // No-op if nothing's connected.
    let existing = zaprite_config::load(&state.db).await.map_err(|e| {
        AppError::Internal(anyhow::anyhow!("load zaprite_config: {e:#}"))
    })?;
    if existing.is_none() {
        return Ok(Json(json!({
            "ok": true,
            "noop": true,
            "message": "Zaprite was not connected",
        })));
    }

    zaprite_config::clear(&state.db).await.map_err(|e| {
        AppError::Internal(anyhow::anyhow!("clear zaprite_config: {e:#}"))
    })?;
    state.clear_payment_provider().await;
    // If the active-provider preference was Zaprite, clear it.
    // Don't blindly clear if it was BTCPay — that's a different
    // operator's choice we shouldn't undo just because they ran
    // Disconnect Zaprite.
    if matches!(
        crate::payment::read_active_provider_preference(&state.db).await,
        Some(crate::payment::ProviderKind::Zaprite)
    ) {
        let _ = crate::db::repo::settings_set(
            &state.db,
            crate::payment::SETTING_ACTIVE_PROVIDER,
            None,
        )
        .await;
    }

    let _ = crate::db::repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "zaprite.disconnect",
        Some("payment_provider"),
        Some("zaprite"),
        ip.as_deref(),
        ua.as_deref(),
        &json!({}),
    )
    .await;

    Ok(Json(json!({
        "ok": true,
        "noop": false,
        "message": "Zaprite disconnected. Don't forget to delete the corresponding webhook on Zaprite's side at app.zaprite.com.",
    })))
}

/// `GET /v1/admin/zaprite/status` — operator-facing connection
/// snapshot. Reports whether Zaprite is the active provider, the
/// base URL, and whether a webhook id has been recorded. Does NOT
/// return the API key (mirroring how btcpay/status redacts).
pub async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let cfg = zaprite_config::load(&state.db).await.map_err(|e| {
        AppError::Internal(anyhow::anyhow!("load zaprite_config: {e:#}"))
    })?;
    let active_provider = match state.payment.read().await.as_ref() {
        Some(p) => Some(p.kind().as_str().to_string()),
        None => None,
    };
    let webhook_url = format!(
        "{}/v1/zaprite/webhook",
        state.config.public_base_url.trim_end_matches('/')
    );
    Ok(Json(json!({
        "connected": cfg.is_some(),
        "active_provider": active_provider,
        "base_url": cfg.as_ref().map(|c| c.base_url.clone()),
        "webhook_id": cfg.as_ref().and_then(|c| c.webhook_id.clone()),
        // Surfaced unconditionally so an operator who lost the
        // first-connect message can still find the URL to paste
        // into Zaprite's dashboard. Webhook-not-yet-registered
        // doesn't change the URL — it's the same address Zaprite
        // would POST to once registered.
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
