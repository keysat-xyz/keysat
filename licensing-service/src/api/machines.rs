//! Machines — individual install records bound to a license.
//!
//! In the single-seat case (`licenses.max_machines = 1`), the first
//! successful `/v1/validate` call locks the fingerprint onto the license
//! and creates a `machines` row. Later validations keep heartbeating that
//! row.
//!
//! In the multi-seat case (`max_machines > 1` or `0` for unlimited),
//! validate auto-activates up to the cap. Beyond the cap, the client gets a
//! `too_many_machines` reject and is expected to call
//! `POST /v1/machines/deactivate` with the fingerprint of an old install to
//! free up a slot, then retry.
//!
//! Explicit activation endpoints (`POST /v1/machines/activate`) are offered
//! for apps that want to prompt the user about seat usage before starting up
//! for the first time. They behave identically to `/v1/validate`'s implicit
//! activation, just without requiring the full key check.
//!
//! Admin endpoints let operators look at who's using what and force-kick a
//! machine off a license.

use crate::api::admin::{request_context, require_scope};
use crate::api::AppState;
use crate::crypto;
use crate::db::repo;
use crate::error::{AppError, AppResult};
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ---------- Public endpoints (client-facing) ----------

#[derive(Debug, Deserialize)]
pub struct ActivateReq {
    pub key: String,
    pub fingerprint: String,
    pub hostname: Option<String>,
    pub platform: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ActivateResp {
    pub ok: bool,
    pub machine_id: Option<String>,
    pub active_count: i64,
    pub max_machines: i64,
    pub reason: Option<String>,
}

pub async fn activate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ActivateReq>,
) -> AppResult<Json<ActivateResp>> {
    let (payload, signature, signed_bytes) = crypto::parse_key(&req.key)
        .map_err(|e| AppError::BadRequest(format!("bad key: {e}")))?;
    crypto::verify_payload(&state.keypair.verifying, &signed_bytes, &signature)
        .map_err(|_| AppError::BadRequest("signature verification failed".into()))?;
    let license_id = payload.license_id.to_string();
    let license = repo::get_license_by_id(&state.db, &license_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("license {license_id}")))?;

    if license.status != "active" {
        return Ok(Json(ActivateResp {
            ok: false,
            machine_id: None,
            active_count: 0,
            max_machines: license.max_machines,
            reason: Some(license.status),
        }));
    }

    let client_ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or("").trim().to_string());

    let fp_hash = crate::hex_sha256(&req.fingerprint);

    if let Some(m) =
        repo::get_active_machine_by_fp(&state.db, &license_id, &fp_hash).await?
    {
        repo::heartbeat_machine(&state.db, &m.id, client_ip.as_deref()).await?;
        let active = repo::list_active_machines(&state.db, &license_id).await?;
        return Ok(Json(ActivateResp {
            ok: true,
            machine_id: Some(m.id),
            active_count: active.len() as i64,
            max_machines: license.max_machines,
            reason: None,
        }));
    }

    let active = repo::list_active_machines(&state.db, &license_id).await?;
    if license.max_machines > 0 && active.len() as i64 >= license.max_machines {
        return Ok(Json(ActivateResp {
            ok: false,
            machine_id: None,
            active_count: active.len() as i64,
            max_machines: license.max_machines,
            reason: Some("too_many_machines".into()),
        }));
    }

    let m = repo::activate_machine(
        &state.db,
        &license_id,
        &req.fingerprint,
        &fp_hash,
        req.hostname.as_deref(),
        req.platform.as_deref(),
        client_ip.as_deref(),
    )
    .await?;
    crate::webhooks::dispatch(
        &state,
        "machine.activated",
        &json!({
            "license_id": license_id,
            "machine_id": m.id,
            "fingerprint_hash": fp_hash,
        }),
    )
    .await;

    let active = repo::list_active_machines(&state.db, &license_id).await?;
    Ok(Json(ActivateResp {
        ok: true,
        machine_id: Some(m.id),
        active_count: active.len() as i64,
        max_machines: license.max_machines,
        reason: None,
    }))
}

#[derive(Debug, Deserialize)]
pub struct HeartbeatReq {
    pub key: String,
    pub fingerprint: String,
}

pub async fn heartbeat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<HeartbeatReq>,
) -> AppResult<Json<Value>> {
    let (payload, signature, signed_bytes) = crypto::parse_key(&req.key)
        .map_err(|e| AppError::BadRequest(format!("bad key: {e}")))?;
    crypto::verify_payload(&state.keypair.verifying, &signed_bytes, &signature)
        .map_err(|_| AppError::BadRequest("signature verification failed".into()))?;
    let license_id = payload.license_id.to_string();

    // Rate-limit heartbeats per-license to 60/hr.
    if !crate::rate_limit::consume(
        &state.db,
        "heartbeat_license",
        &license_id,
        /* capacity */ 60.0,
        /* refill_per_second */ 60.0 / 3600.0,
    )
    .await?
    {
        return Ok(Json(json!({ "ok": false, "reason": "rate_limited" })));
    }

    let fp_hash = crate::hex_sha256(&req.fingerprint);
    let client_ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or("").trim().to_string());

    match repo::get_active_machine_by_fp(&state.db, &license_id, &fp_hash).await? {
        Some(m) => {
            repo::heartbeat_machine(&state.db, &m.id, client_ip.as_deref()).await?;
            Ok(Json(json!({ "ok": true, "machine_id": m.id })))
        }
        None => Ok(Json(json!({ "ok": false, "reason": "not_activated" }))),
    }
}

#[derive(Debug, Deserialize)]
pub struct DeactivateReq {
    pub key: String,
    pub fingerprint: String,
    #[serde(default)]
    pub reason: Option<String>,
}

pub async fn deactivate(
    State(state): State<AppState>,
    Json(req): Json<DeactivateReq>,
) -> AppResult<Json<Value>> {
    let (payload, signature, signed_bytes) = crypto::parse_key(&req.key)
        .map_err(|e| AppError::BadRequest(format!("bad key: {e}")))?;
    crypto::verify_payload(&state.keypair.verifying, &signed_bytes, &signature)
        .map_err(|_| AppError::BadRequest("signature verification failed".into()))?;
    let license_id = payload.license_id.to_string();
    let fp_hash = crate::hex_sha256(&req.fingerprint);

    let m = repo::get_active_machine_by_fp(&state.db, &license_id, &fp_hash).await?;
    let Some(m) = m else {
        return Ok(Json(json!({ "ok": false, "reason": "not_found" })));
    };
    let reason = req
        .reason
        .unwrap_or_else(|| "client_requested".to_string());
    repo::deactivate_machine(&state.db, &m.id, &reason).await?;
    crate::webhooks::dispatch(
        &state,
        "machine.deactivated",
        &json!({
            "license_id": license_id,
            "machine_id": m.id,
            "reason": reason,
        }),
    )
    .await;
    // Single-seat legacy: also clear licenses.fingerprint so the next client
    // can re-bind cleanly.
    let license = repo::get_license_by_id(&state.db, &license_id).await?;
    if let Some(lic) = license {
        if lic.max_machines == 1 {
            let _ = sqlx::query("UPDATE licenses SET fingerprint = NULL WHERE id = ?")
                .bind(&license_id)
                .execute(&state.db)
                .await;
        }
    }
    Ok(Json(json!({ "ok": true })))
}

// ---------- Admin endpoints ----------

/// Query for the admin Machines list. All filters are optional and
/// conjunctive — leaving them all blank returns every machine across
/// every license, default-sorted by most-recent heartbeat. The admin UI
/// Machines tab uses this default-no-filter form to render a global
/// view; the Licenses-tab drill-down sets `license_id`.
#[derive(Debug, Deserialize)]
pub struct AdminListQuery {
    #[serde(default)]
    pub license_id: Option<String>,
    #[serde(default)]
    pub product_id: Option<String>,
    #[serde(default)]
    pub product_slug: Option<String>,
    #[serde(default)]
    pub include_inactive: bool,
    /// Cap on result size; defaults to 500. Admin UI paginates client-side.
    #[serde(default)]
    pub limit: Option<i64>,
}

pub async fn admin_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<AdminListQuery>,
) -> AppResult<Json<Value>> {
    require_scope(&state, &headers, "machines:read").await?;

    // Resolve product_slug → product_id if the caller passed the slug
    // form. Either works; product_id takes precedence on conflict.
    let resolved_product_id: Option<String> = if let Some(pid) = q.product_id.as_deref() {
        Some(pid.to_string())
    } else if let Some(slug) = q.product_slug.as_deref() {
        match repo::get_product_by_slug(&state.db, slug).await? {
            Some(p) => Some(p.id),
            None => return Err(AppError::NotFound(format!("product '{slug}'"))),
        }
    } else {
        None
    };

    let machines = repo::list_machines_admin(
        &state.db,
        resolved_product_id.as_deref(),
        q.license_id.as_deref(),
        q.include_inactive,
        q.limit.unwrap_or(500).clamp(1, 5000),
    )
    .await?;
    Ok(Json(json!({ "machines": machines })))
}

#[derive(Debug, Deserialize)]
pub struct AdminDeactivateReq {
    #[serde(default)]
    pub reason: String,
}

pub async fn admin_deactivate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<AdminDeactivateReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_scope(&state, &headers, "machines:write").await?;
    let (ip, ua) = request_context(&headers);
    let reason = if req.reason.is_empty() {
        "admin deactivate".to_string()
    } else {
        req.reason
    };
    let m = repo::get_machine_by_id(&state.db, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("machine {id}")))?;
    repo::deactivate_machine(&state.db, &id, &reason).await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "machine.deactivate",
        Some("machine"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "license_id": m.license_id, "reason": reason }),
    )
    .await;
    crate::webhooks::dispatch(
        &state,
        "machine.deactivated",
        &json!({
            "license_id": m.license_id,
            "machine_id": id,
            "reason": reason,
        }),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}
