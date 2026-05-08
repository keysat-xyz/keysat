//! Active-provider swap endpoint.
//!
//! When an operator has both BTCPay AND Zaprite configured (i.e.,
//! they ran Connect on both at some point), this lets them flip
//! the active one without re-authorizing. The Connect flows are
//! still where credentials live; this endpoint only changes which
//! credentials the daemon currently routes through.

use crate::api::admin::{request_context, require_admin};
use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::payment::{
    self, btcpay::BtcpayProvider, zaprite::ZapriteProvider, ProviderKind,
};
use axum::{extract::State, http::HeaderMap, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct ActivateReq {
    /// `'btcpay'` or `'zaprite'`. Other values → 400.
    pub provider: String,
}

/// `GET /v1/admin/payment-provider/status` — both providers'
/// configuration state at a glance, plus the active preference.
/// Lets the SPA render a "BTCPay [active] / Zaprite [configured,
/// not active]" header without two separate fetches.
pub async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let btcpay_configured = crate::btcpay::config::load(&state.db)
        .await
        .map(|o| o.is_some())
        .unwrap_or(false);
    let zaprite_configured = payment::zaprite::config::load(&state.db)
        .await
        .map(|o| o.is_some())
        .unwrap_or(false);
    let preference = payment::read_active_provider_preference(&state.db).await;
    let active_runtime = match state.payment.read().await.as_ref() {
        Some(p) => Some(p.kind().as_str().to_string()),
        None => None,
    };
    Ok(Json(json!({
        "btcpay_configured": btcpay_configured,
        "zaprite_configured": zaprite_configured,
        "preferred": preference.map(|k| k.as_str().to_string()),
        "active": active_runtime,
    })))
}

/// `POST /v1/admin/payment-provider/activate` — swap the active
/// provider to whichever already-configured one the operator
/// names. 400 if the named provider isn't configured (run Connect
/// first).
pub async fn activate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ActivateReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);

    let kind = match req.provider.to_lowercase().as_str() {
        "btcpay" => ProviderKind::Btcpay,
        "zaprite" => ProviderKind::Zaprite,
        other => {
            return Err(AppError::BadRequest(format!(
                "unknown provider '{other}'; accepted: btcpay, zaprite"
            )))
        }
    };

    // Build the provider from its persisted config. Refuse if the
    // config row isn't there — operator has to run Connect first.
    match kind {
        ProviderKind::Btcpay => {
            let cfg = crate::btcpay::config::load(&state.db)
                .await
                .map_err(AppError::Internal)?
                .ok_or_else(|| {
                    AppError::BadRequest(
                        "BTCPay not configured. Run Connect BTCPay first.".into(),
                    )
                })?;
            let client = crate::btcpay::client::BtcpayClient::new(
                &cfg.base_url,
                &cfg.api_key,
                &cfg.store_id,
            );
            let provider = Arc::new(
                BtcpayProvider::new(client, cfg.webhook_secret)
                    .with_public_base(state.config.btcpay_public_url.clone()),
            );
            state.set_payment_provider(provider).await;
        }
        ProviderKind::Zaprite => {
            let cfg = payment::zaprite::config::load(&state.db)
                .await
                .map_err(|e| AppError::Internal(anyhow::anyhow!("{e:#}")))?
                .ok_or_else(|| {
                    AppError::BadRequest(
                        "Zaprite not configured. Run Connect Zaprite first.".into(),
                    )
                })?;
            let client = payment::zaprite::ZapriteClient::new(&cfg.base_url, &cfg.api_key);
            let provider = Arc::new(ZapriteProvider::new(client));
            state.set_payment_provider(provider).await;
        }
    }

    // Persist the preference so the boot-time loader picks the
    // same one on next restart.
    payment::write_active_provider_preference(&state.db, kind)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write preference: {e:#}")))?;

    let _ = crate::db::repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "payment_provider.activate",
        Some("payment_provider"),
        Some(kind.as_str()),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "provider": kind.as_str() }),
    )
    .await;

    Ok(Json(json!({
        "ok": true,
        "active": kind.as_str(),
    })))
}
