//! Admin endpoints for managing the daemon's own self-license
//! (Keysat-licenses-Keysat).
//!
//! - `GET /v1/admin/self-license`   — current tier (licensed / unlicensed)
//! - `POST /v1/admin/self-license`  — activate a new license. Validates
//!     against the embedded master pubkey, writes the file to
//!     `SELF_LICENSE_PATH`, and swaps the runtime tier in app state.
//!
//! These run *only* when authenticated by the admin API key — same gate
//! as every other `/v1/admin/*` route.

use crate::api::AppState;
use crate::error::AppResult;
use crate::license_self::{self, Tier};
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
#[serde(tag = "tier", rename_all = "snake_case")]
pub enum TierStatus {
    Unlicensed {
        reason: String,
        mode: &'static str,
    },
    Licensed {
        license_id: String,
        product_id: String,
        /// Unix seconds; 0 means perpetual.
        expires_at: i64,
        entitlements: Vec<String>,
        mode: &'static str,
    },
}

fn tier_to_status(tier: &Tier) -> TierStatus {
    let mode = match license_self::mode() {
        license_self::Mode::Permissive => "permissive",
        license_self::Mode::Enforce => "enforce",
    };
    match tier {
        Tier::Unlicensed { reason } => TierStatus::Unlicensed {
            reason: reason.clone(),
            mode,
        },
        Tier::Licensed {
            license_id,
            product_id,
            expires_at,
            entitlements,
        } => TierStatus::Licensed {
            license_id: license_id.to_string(),
            product_id: product_id.to_string(),
            expires_at: *expires_at,
            entitlements: entitlements.clone(),
            mode,
        },
    }
}

pub async fn status(State(state): State<AppState>) -> Json<TierStatus> {
    let tier = state.self_tier.read().await.clone();
    Json(tier_to_status(&tier))
}

#[derive(Deserialize)]
pub struct ActivateBody {
    pub license_key: String,
}

#[derive(Serialize)]
pub struct ActivateResponse {
    pub ok: bool,
    pub tier: TierStatus,
    pub message: String,
}

pub async fn activate(
    State(state): State<AppState>,
    Json(body): Json<ActivateBody>,
) -> AppResult<impl IntoResponse> {
    let key = body.license_key.trim().to_string();
    if key.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "license_key is required"
            })),
        )
            .into_response());
    }

    // Verify against the embedded master pubkey before persisting.
    let new_tier = match license_self::verify_license(&key) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("self-license activation rejected: {e:#}");
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "license_invalid",
                    "detail": format!("{e:#}"),
                })),
            )
                .into_response());
        }
    };

    // Persist to /data/keysat-license.txt.
    if let Err(e) = license_self::write_license_file(&key) {
        tracing::error!("self-license file write failed: {e:#}");
        return Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "write_failed",
                "detail": format!("{e:#}"),
            })),
        )
            .into_response());
    }

    // Swap the runtime tier.
    {
        let mut guard = state.self_tier.write().await;
        *guard = new_tier.clone();
    }

    let status_resp = tier_to_status(&new_tier);
    let summary = match &status_resp {
        TierStatus::Licensed {
            license_id,
            expires_at,
            entitlements,
            ..
        } => {
            let exp = if *expires_at == 0 {
                "perpetual".to_string()
            } else {
                format!("expires unix={}", expires_at)
            };
            let ents = if entitlements.is_empty() {
                "(none)".to_string()
            } else {
                entitlements.join(",")
            };
            format!(
                "License {} verified — {}, entitlements={}.",
                license_id, exp, ents
            )
        }
        TierStatus::Unlicensed { .. } => {
            // Should be unreachable; verify_license never returns Unlicensed.
            "License processed.".to_string()
        }
    };

    tracing::info!("self-license activated: {summary}");

    Ok((
        StatusCode::OK,
        Json(ActivateResponse {
            ok: true,
            tier: status_resp,
            message: summary,
        }),
    )
        .into_response())
}

/// `POST /v1/admin/self-license/refresh` — re-read the daemon's
/// own license row from the local DB and update `state.self_tier`
/// with the live entitlements. Useful right after an admin
/// Change Tier when the operator doesn't want to wait for the
/// hourly background refresher.
pub async fn refresh(State(state): State<AppState>) -> Json<TierStatus> {
    let current = state.self_tier.read().await.clone();
    let next = license_self::refresh_self_tier_from_db(&state.db, &current).await;
    *state.self_tier.write().await = next.clone();
    Json(tier_to_status(&next))
}
