//! The single most-hit endpoint: validate a license key.
//!
//! Clients — typically another piece of software starting up — call this
//! with their key and (optionally) the `product_slug` they expect the key
//! to cover and a `fingerprint` identifying the machine/installation.
//!
//! Response shape (HTTP always 200; `ok` + `reason` machine-readable):
//!
//! ```json
//! { "ok": true,  "license_id": "...", "product_id": "...", "entitlements": ["pro"], "status": "active" }
//! { "ok": false, "reason": "expired", "grace_until": "..." }
//! ```
//!
//! Machine cap handling:
//!
//! When a license allows more than one concurrent machine (`max_machines != 1`),
//! validate will auto-activate up to the cap. Beyond the cap, the call is
//! rejected with `too_many_machines` — the client is expected to either
//! prompt the user to deactivate another machine or to call
//! `POST /v1/machines/deactivate` first. `max_machines == 0` means unlimited.

use crate::api::AppState;
use crate::crypto::{self, hash_fingerprint};
use crate::db::repo;
use crate::error::AppResult;
use axum::{
    extract::State,
    http::{header, HeaderMap},
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct ValidateReq {
    pub key: String,
    /// Optional: the product slug the caller expects this key to cover.
    /// Rejects keys issued for a different product even if valid.
    pub product_slug: Option<String>,
    /// Optional: raw machine fingerprint. First successful validation binds
    /// this to the license row (if not already set); later validations
    /// succeed only if it matches.
    pub fingerprint: Option<String>,
    /// Optional client-supplied hostname for machine records.
    pub hostname: Option<String>,
    /// Optional client-supplied platform descriptor.
    pub platform: Option<String>,
}

#[derive(Debug, Serialize, Default)]
pub struct ValidateResp {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issued_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grace_until: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_grace_period: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_trial: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(default)]
    pub entitlements: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_machines: Option<i64>,
}

fn reject(reason: &str) -> ValidateResp {
    ValidateResp {
        ok: false,
        reason: Some(reason.to_string()),
        ..Default::default()
    }
}

pub async fn validate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ValidateReq>,
) -> AppResult<Json<ValidateResp>> {
    let client_ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or("").trim().to_string());
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Rate limit by client IP if available, else by license key prefix as a
    // last-ditch bucket key. Cap at 60 req / minute / bucket.
    let bucket_key = client_ip.clone().unwrap_or_else(|| {
        req.key
            .chars()
            .take(24)
            .collect::<String>()
    });
    if !crate::rate_limit::consume(
        &state.db,
        "validate_ip",
        &bucket_key,
        /* capacity */ 60.0,
        /* refill_per_second */ 1.0,
    )
    .await?
    {
        return Ok(Json(reject("rate_limited")));
    }

    // Step 1: parse & verify signature offline-style, using the server's own
    // verifying key (same key the SDK will ship).
    let (payload, signature, signed_bytes) = match crypto::parse_key(&req.key) {
        Ok(ok) => ok,
        Err(e) => {
            repo::log_validation(
                &state.db,
                None,
                None,
                req.fingerprint.as_deref(),
                "bad_format",
                client_ip.as_deref(),
                user_agent.as_deref(),
                None,
                Some(&e.to_string()),
            )
            .await
            .ok();
            tracing::debug!(error = %e, "rejected malformed key");
            return Ok(Json(reject("bad_format")));
        }
    };

    if crypto::verify_payload(&state.keypair.verifying, &signed_bytes, &signature).is_err() {
        repo::log_validation(
            &state.db,
            Some(&payload.license_id.to_string()),
            Some(&payload.product_id.to_string()),
            req.fingerprint.as_deref(),
            "bad_signature",
            client_ip.as_deref(),
            user_agent.as_deref(),
            None,
            None,
        )
        .await
        .ok();
        return Ok(Json(reject("bad_signature")));
    }

    let license_id = payload.license_id.to_string();
    let product_id = payload.product_id.to_string();

    // Step 2: look up the license row.
    let license = match repo::get_license_by_id(&state.db, &license_id).await? {
        Some(l) => l,
        None => {
            repo::log_validation(
                &state.db,
                Some(&license_id),
                Some(&product_id),
                req.fingerprint.as_deref(),
                "not_found",
                client_ip.as_deref(),
                user_agent.as_deref(),
                None,
                None,
            )
            .await
            .ok();
            return Ok(Json(reject("not_found")));
        }
    };

    // Step 3: status checks — authoritative server-side.
    match license.status.as_str() {
        "active" => {}
        "revoked" => {
            repo::log_validation(
                &state.db,
                Some(&license_id),
                Some(&product_id),
                req.fingerprint.as_deref(),
                "revoked",
                client_ip.as_deref(),
                user_agent.as_deref(),
                None,
                license.revocation_reason.as_deref(),
            )
            .await
            .ok();
            return Ok(Json(reject("revoked")));
        }
        "suspended" => {
            repo::log_validation(
                &state.db,
                Some(&license_id),
                Some(&product_id),
                req.fingerprint.as_deref(),
                "suspended",
                client_ip.as_deref(),
                user_agent.as_deref(),
                None,
                license.suspension_reason.as_deref(),
            )
            .await
            .ok();
            return Ok(Json(reject("suspended")));
        }
        other => {
            tracing::warn!(status = other, license_id, "unknown license status");
            return Ok(Json(reject("invalid_state")));
        }
    }

    // Step 4: product match (optional).
    let product = repo::get_product_by_id(&state.db, &license.product_id).await?;
    if let (Some(expected_slug), Some(p)) = (&req.product_slug, &product) {
        if &p.slug != expected_slug {
            repo::log_validation(
                &state.db,
                Some(&license_id),
                Some(&product_id),
                req.fingerprint.as_deref(),
                "product_mismatch",
                client_ip.as_deref(),
                user_agent.as_deref(),
                None,
                None,
            )
            .await
            .ok();
            return Ok(Json(reject("product_mismatch")));
        }
    }

    // Step 5: expiry + grace.
    let now = Utc::now();
    let mut in_grace_period = false;
    let mut grace_until: Option<String> = None;
    if let Some(exp_str) = &license.expires_at {
        if let Ok(exp_dt) = DateTime::parse_from_rfc3339(exp_str) {
            let exp_utc = exp_dt.with_timezone(&Utc);
            let grace_cutoff = exp_utc + chrono::Duration::seconds(license.grace_seconds);
            if now >= grace_cutoff {
                repo::log_validation(
                    &state.db,
                    Some(&license_id),
                    Some(&product_id),
                    req.fingerprint.as_deref(),
                    "expired",
                    client_ip.as_deref(),
                    user_agent.as_deref(),
                    None,
                    Some(&format!("expired at {exp_str}")),
                )
                .await
                .ok();
                return Ok(Json(ValidateResp {
                    ok: false,
                    reason: Some("expired".into()),
                    license_id: Some(license_id),
                    product_id: Some(product_id),
                    expires_at: Some(exp_str.clone()),
                    ..Default::default()
                }));
            } else if now >= exp_utc {
                in_grace_period = true;
                grace_until = Some(grace_cutoff.to_rfc3339());
            }
        }
    }

    // Step 6: fingerprint + machine binding.
    // - Single-seat (max_machines == 1): preserve legacy column-based TOFU
    //   on `licenses.fingerprint` for backwards compatibility, AND also
    //   write/update a `machines` row so admins see a consistent view.
    // - Multi-seat: look up / auto-activate in the machines table, enforce
    //   the cap.
    let mut machine_id: Option<String> = None;
    if let Some(fp) = req.fingerprint.as_deref() {
        let fp_hash = crate::hex_sha256(fp);

        if license.max_machines == 1 {
            match &license.fingerprint {
                Some(stored) if stored != fp => {
                    repo::log_validation(
                        &state.db,
                        Some(&license_id),
                        Some(&product_id),
                        Some(fp),
                        "fingerprint_mismatch",
                        client_ip.as_deref(),
                        user_agent.as_deref(),
                        None,
                        None,
                    )
                    .await
                    .ok();
                    return Ok(Json(reject("fingerprint_mismatch")));
                }
                Some(_) => {
                    // Already bound and matches — touch heartbeat on any machine row.
                    if let Some(m) =
                        repo::get_active_machine_by_fp(&state.db, &license_id, &fp_hash).await?
                    {
                        repo::heartbeat_machine(&state.db, &m.id, client_ip.as_deref()).await?;
                        machine_id = Some(m.id);
                    }
                }
                None => {
                    repo::bind_fingerprint_if_unset(&state.db, &license_id, fp).await?;
                    let m = repo::activate_machine(
                        &state.db,
                        &license_id,
                        fp,
                        &fp_hash,
                        req.hostname.as_deref(),
                        req.platform.as_deref(),
                        client_ip.as_deref(),
                    )
                    .await?;
                    crate::webhooks::dispatch(
                        &state,
                        "machine.activated",
                        &serde_json::json!({
                            "license_id": license_id,
                            "machine_id": m.id,
                            "fingerprint_hash": fp_hash,
                        }),
                    )
                    .await;
                    machine_id = Some(m.id);
                }
            }
        } else {
            // Multi-seat: consult machines table.
            match repo::get_active_machine_by_fp(&state.db, &license_id, &fp_hash).await? {
                Some(m) => {
                    repo::heartbeat_machine(&state.db, &m.id, client_ip.as_deref()).await?;
                    machine_id = Some(m.id);
                }
                None => {
                    // Count existing active machines. max_machines = 0 means unlimited.
                    let active = repo::list_active_machines(&state.db, &license_id).await?;
                    if license.max_machines > 0 && active.len() as i64 >= license.max_machines {
                        repo::log_validation(
                            &state.db,
                            Some(&license_id),
                            Some(&product_id),
                            Some(fp),
                            "too_many_machines",
                            client_ip.as_deref(),
                            user_agent.as_deref(),
                            None,
                            Some(&format!(
                                "cap {} already reached",
                                license.max_machines
                            )),
                        )
                        .await
                        .ok();
                        return Ok(Json(ValidateResp {
                            ok: false,
                            reason: Some("too_many_machines".into()),
                            license_id: Some(license_id),
                            product_id: Some(product_id),
                            max_machines: Some(license.max_machines),
                            ..Default::default()
                        }));
                    }
                    let m = repo::activate_machine(
                        &state.db,
                        &license_id,
                        fp,
                        &fp_hash,
                        req.hostname.as_deref(),
                        req.platform.as_deref(),
                        client_ip.as_deref(),
                    )
                    .await?;
                    crate::webhooks::dispatch(
                        &state,
                        "machine.activated",
                        &serde_json::json!({
                            "license_id": license_id,
                            "machine_id": m.id,
                            "fingerprint_hash": fp_hash,
                        }),
                    )
                    .await;
                    machine_id = Some(m.id);
                }
            }
        }

        // If the signed payload is itself fingerprint-bound, enforce hash
        // match against the signed blob (an extra belt-and-braces check).
        if payload.is_fingerprint_bound() && payload.fingerprint_hash != hash_fingerprint(fp) {
            return Ok(Json(reject("fingerprint_mismatch")));
        }
    }

    repo::log_validation(
        &state.db,
        Some(&license_id),
        Some(&product_id),
        req.fingerprint.as_deref(),
        "ok",
        client_ip.as_deref(),
        user_agent.as_deref(),
        machine_id.as_deref(),
        if in_grace_period {
            Some("in_grace_period")
        } else {
            None
        },
    )
    .await
    .ok();

    Ok(Json(ValidateResp {
        ok: true,
        reason: None,
        license_id: Some(license_id),
        product_id: Some(product_id),
        product_slug: product.map(|p| p.slug),
        issued_at: Some(license.issued_at),
        expires_at: license.expires_at,
        grace_until,
        in_grace_period: if in_grace_period { Some(true) } else { None },
        is_trial: if license.is_trial { Some(true) } else { None },
        entitlements: license.entitlements,
        status: Some(license.status),
        machine_id,
        max_machines: Some(license.max_machines),
    }))
}
