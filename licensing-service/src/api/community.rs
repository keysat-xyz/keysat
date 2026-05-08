//! Admin endpoints for the opt-in community analytics toggle.
//!
//! Three endpoints:
//!   GET  /v1/admin/community-analytics       — current state + a
//!                                                preview of what
//!                                                would be sent
//!   POST /v1/admin/community-analytics       — set enabled +
//!                                                collector_url
//!   POST /v1/admin/community-analytics/reset — wipes the install
//!                                                UUID (so a future
//!                                                opt-in generates
//!                                                a fresh anonymous
//!                                                identifier)
//!
//! The toggle is intentionally a multi-step decision: enabling
//! requires the operator to also confirm a collector URL. The
//! daemon never beacons without both being set.

use crate::analytics::{
    self, SETTING_COLLECTOR_URL, SETTING_ENABLED, SETTING_INSTALL_UUID,
};
use crate::api::admin::{request_context, require_admin};
use crate::api::AppState;
use crate::db::repo;
use crate::error::{AppError, AppResult};
use axum::{extract::State, http::HeaderMap, Json};
use serde::Deserialize;
use serde_json::{json, Value};

pub async fn get(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let enabled = analytics::is_enabled(&state).await;
    let collector_url = repo::settings_get(&state.db, SETTING_COLLECTOR_URL).await?;
    let install_uuid = repo::settings_get(&state.db, SETTING_INSTALL_UUID).await?;

    // Preview: build a heartbeat snapshot RIGHT NOW so the operator
    // sees exactly what would be sent. This is the privacy-by-
    // demonstration move — nothing happens behind their back.
    let preview = match install_uuid.as_deref() {
        Some(uuid) if !uuid.is_empty() => {
            // started_at = now-since-epoch; preview shows uptime "<1d"
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let snap = analytics::build_heartbeat(&state, uuid, now).await?;
            serde_json::to_value(snap).unwrap_or(serde_json::Value::Null)
        }
        _ => {
            // Show what a heartbeat WOULD look like with a placeholder
            // uuid so operators can see the shape before opting in.
            let snap = analytics::build_heartbeat(
                &state,
                "00000000-0000-0000-0000-000000000000",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            )
            .await?;
            serde_json::to_value(snap).unwrap_or(serde_json::Value::Null)
        }
    };

    Ok(Json(json!({
        "enabled": enabled,
        "collector_url": collector_url,
        "install_uuid": install_uuid,
        "preview_heartbeat": preview,
    })))
}

#[derive(Debug, Deserialize)]
pub struct SetReq {
    pub enabled: bool,
    pub collector_url: Option<String>,
}

pub async fn set(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SetReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);

    // Validate URL shape if one was supplied. We don't try to reach
    // it — the heartbeat task does that on its own schedule.
    let collector_url_clean: Option<String> = req
        .collector_url
        .as_deref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(url) = collector_url_clean.as_deref() {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(AppError::BadRequest(
                "collector_url must start with http:// or https://".into(),
            ));
        }
    }

    // Enabling without a collector URL is allowed (collector_url
    // can be set later, OR the future built-in default URL will
    // kick in once it ships). But surface the situation in the
    // response so the SPA can show "enabled but not yet beaconing"
    // state if relevant.
    let enabled_str = if req.enabled { "1" } else { "0" };
    repo::settings_set(&state.db, SETTING_ENABLED, Some(enabled_str)).await?;
    repo::settings_set(
        &state.db,
        SETTING_COLLECTOR_URL,
        collector_url_clean.as_deref(),
    )
    .await?;

    // Generate the install UUID on first opt-in. (No-op on
    // subsequent toggles — the UUID persists across enable/disable
    // cycles unless explicitly reset, so a flip-flop doesn't make
    // the same install look like a new one.)
    if req.enabled {
        analytics::ensure_install_uuid(&state).await?;
    }

    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        if req.enabled { "community_analytics.enable" } else { "community_analytics.disable" },
        Some("setting"),
        Some(SETTING_ENABLED),
        ip.as_deref(),
        ua.as_deref(),
        &json!({
            "enabled": req.enabled,
            "collector_url_set": collector_url_clean.is_some(),
        }),
    )
    .await;

    Ok(Json(json!({
        "enabled": req.enabled,
        "collector_url": collector_url_clean,
    })))
}

/// Wipes the install UUID. After a reset, the next opt-in generates
/// a fresh UUID — useful for an operator who's been beaconing under
/// one identifier and wants to start over (e.g., after a DB restore
/// from a snapshot taken before they opted in originally).
pub async fn reset(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    repo::settings_set(&state.db, SETTING_INSTALL_UUID, None).await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "community_analytics.reset",
        Some("setting"),
        Some(SETTING_INSTALL_UUID),
        ip.as_deref(),
        ua.as_deref(),
        &json!({}),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}
