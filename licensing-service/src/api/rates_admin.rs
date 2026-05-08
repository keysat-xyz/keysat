//! Admin endpoints for the BTC/fiat rate cache.
//!
//! Two surfaces:
//!   GET  /v1/admin/rates              — what's cached right now
//!                                        (operators can see what
//!                                        the daemon would quote and
//!                                        which source it came from)
//!   POST /v1/admin/rates/refresh      — force a fresh fetch for a
//!                                        given currency, bypassing
//!                                        the TTL cache. Useful
//!                                        after a rate-source
//!                                        outage to confirm the
//!                                        chain works end-to-end.

use crate::api::admin::{request_context, require_admin};
use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::rates;
use axum::{extract::State, http::HeaderMap, Json};
use serde::Deserialize;
use serde_json::{json, Value};

pub async fn get(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let snapshot = state.rates.snapshot().await;
    let rates_json: Vec<Value> = snapshot
        .into_iter()
        .map(|(currency, cached)| {
            json!({
                "currency": currency,
                "units_per_btc": cached.units_per_btc,
                "source": cached.source,
                "fetched_at_secs_ago": cached.fetched_at.elapsed()
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            })
        })
        .collect();
    Ok(Json(json!({ "rates": rates_json })))
}

#[derive(Debug, Deserialize)]
pub struct RefreshReq {
    pub currency: String,
}

pub async fn refresh(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RefreshReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    let currency = req.currency.to_uppercase();

    // Wipe the cache entry so the next get_rate hits the chain.
    state.rates.invalidate(&currency).await;

    // Fetch fresh — bubbles up source errors with full context.
    let fresh = rates::get_rate(&state, &currency).await.map_err(|e| {
        AppError::Upstream(format!("rate refresh failed: {e:#}"))
    })?;

    let _ = crate::db::repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "rate.refresh",
        Some("rate"),
        Some(&currency),
        ip.as_deref(),
        ua.as_deref(),
        &json!({
            "currency": currency,
            "source": fresh.source,
            "units_per_btc": fresh.units_per_btc,
        }),
    )
    .await;

    Ok(Json(json!({
        "currency": currency,
        "units_per_btc": fresh.units_per_btc,
        "source": fresh.source,
    })))
}
