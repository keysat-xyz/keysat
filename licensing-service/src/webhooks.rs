//! Outbound webhooks.
//!
//! When interesting things happen (a license is issued, revoked, suspended,
//! a machine activates, an invoice settles), the service can POST a signed
//! JSON payload to one or more URLs configured by the operator.
//!
//! Design:
//!
//! - Each endpoint has its own HMAC-SHA256 secret (32 random bytes, hex).
//! - Each delivery is a row in `webhook_deliveries`. Deliveries that fail are
//!   retried with exponential backoff up to 10 attempts.
//! - Deliveries are dispatched by a single background task that polls the
//!   table every 5 seconds for rows whose `next_attempt_at` is due.
//! - The signature scheme is the same shape as BTCPay's webhook signing
//!   (`sha256=<hex>`), so integrators who've already written BTCPay webhook
//!   receivers can adapt their code trivially.

use crate::api::AppState;
use crate::db::repo;
use crate::models::WebhookEndpoint;
use chrono::{Duration as ChronoDuration, Utc};
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;
use std::time::Duration;
use tokio::time::sleep;

type HmacSha256 = Hmac<Sha256>;

/// Signature header we attach to every outbound delivery. Receivers verify by
/// recomputing `HMAC-SHA256(body, secret)` and comparing in constant time.
pub const SIG_HEADER: &str = "X-Keysat-Signature";
/// Event-type header, mirrors `event_type` in the payload for convenience.
pub const EVENT_HEADER: &str = "X-Keysat-Event";
/// Idempotency key header — the delivery id, stable across retries.
pub const DELIVERY_HEADER: &str = "X-Keysat-Delivery";

/// Fire off a logical event. Persists one `webhook_deliveries` row per
/// active subscribed endpoint; the delivery worker handles the HTTP.
///
/// Infallible from the caller's perspective: any DB error is logged and
/// swallowed so event dispatch never blocks the main mutation.
pub async fn dispatch(state: &AppState, event_type: &str, data: &Value) {
    let envelope = serde_json::json!({
        "event_type": event_type,
        "timestamp": Utc::now().to_rfc3339(),
        "data": data,
    });
    let envelope_json = match serde_json::to_string(&envelope) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "webhook dispatch: failed to serialize envelope");
            return;
        }
    };

    let endpoints = match repo::list_active_webhook_endpoints(&state.db).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = ?e, "webhook dispatch: failed to list endpoints");
            return;
        }
    };
    for ep in endpoints {
        if !ep_wants(&ep, event_type) {
            continue;
        }
        if let Err(e) = repo::enqueue_delivery(&state.db, &ep.id, event_type, &envelope_json).await
        {
            tracing::warn!(error = ?e, endpoint = %ep.id, "failed to enqueue delivery");
        }
    }
}

fn ep_wants(ep: &WebhookEndpoint, event_type: &str) -> bool {
    ep.event_types.iter().any(|t| t == "*" || t == event_type)
}

/// Background task: every 5s, pick up to 25 deliveries whose `next_attempt_at`
/// is due, POST them, update the row.
pub fn spawn_delivery_worker(state: AppState) {
    tokio::spawn(async move {
        // Stagger startup slightly to avoid racing the initial reconcile loop.
        sleep(Duration::from_secs(5)).await;
        loop {
            if let Err(e) = tick(&state).await {
                tracing::warn!(error = %e, "webhook delivery tick failed");
            }
            sleep(Duration::from_secs(5)).await;
        }
    });
}

async fn tick(state: &AppState) -> anyhow::Result<()> {
    let due = repo::list_ready_deliveries(&state.db, 25)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    if due.is_empty() {
        return Ok(());
    }

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    for d in due {
        // Look up endpoint + secret.
        let ep = match repo::get_webhook_endpoint_by_id(&state.db, &d.endpoint_id, true).await {
            Ok(Some(ep)) if ep.active => ep,
            _ => {
                // Endpoint gone or disabled — mark delivery permanently failed.
                repo::mark_delivery_failure(
                    &state.db,
                    &d.id,
                    None,
                    "endpoint deleted or disabled",
                    None,
                )
                .await
                .ok();
                continue;
            }
        };
        let secret = ep.secret.as_deref().unwrap_or("");

        // Compute HMAC signature of the raw body.
        let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
            Ok(m) => m,
            Err(e) => {
                repo::mark_delivery_failure(
                    &state.db,
                    &d.id,
                    None,
                    &format!("bad HMAC key: {e}"),
                    None,
                )
                .await
                .ok();
                continue;
            }
        };
        mac.update(d.payload_json.as_bytes());
        let sig_hex = hex::encode(mac.finalize().into_bytes());
        let sig_header_val = format!("sha256={sig_hex}");

        let req = http
            .post(&ep.url)
            .header("content-type", "application/json")
            .header(SIG_HEADER, &sig_header_val)
            .header(EVENT_HEADER, &d.event_type)
            .header(DELIVERY_HEADER, &d.id)
            .body(d.payload_json.clone());

        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16() as i64;
                if resp.status().is_success() {
                    repo::mark_delivery_success(&state.db, &d.id, status).await.ok();
                } else {
                    let backoff = backoff_for(d.attempt_count + 1);
                    let next = backoff.map(|bs| (Utc::now() + bs).to_rfc3339());
                    let body_preview = resp.text().await.unwrap_or_default();
                    let trimmed: String = body_preview.chars().take(200).collect();
                    repo::mark_delivery_failure(
                        &state.db,
                        &d.id,
                        Some(status),
                        &format!("non-2xx response: {trimmed}"),
                        next.as_deref(),
                    )
                    .await
                    .ok();
                }
            }
            Err(e) => {
                let backoff = backoff_for(d.attempt_count + 1);
                let next = backoff.map(|bs| (Utc::now() + bs).to_rfc3339());
                repo::mark_delivery_failure(
                    &state.db,
                    &d.id,
                    None,
                    &format!("request error: {e}"),
                    next.as_deref(),
                )
                .await
                .ok();
            }
        }
    }

    Ok(())
}

/// Exponential backoff for delivery retries, capped at 10 attempts. Returns
/// `None` when the max is reached (meaning: do not reschedule).
fn backoff_for(attempts_after: i64) -> Option<ChronoDuration> {
    const MAX_ATTEMPTS: i64 = 10;
    if attempts_after >= MAX_ATTEMPTS {
        return None;
    }
    // 5s, 10s, 30s, 1m, 5m, 15m, 30m, 1h, 2h, 6h
    let minutes = match attempts_after {
        1 => 0,
        2 => 0,
        3 => 0,
        4 => 1,
        5 => 5,
        6 => 15,
        7 => 30,
        8 => 60,
        9 => 120,
        _ => 360,
    };
    let seconds = match attempts_after {
        1 => 5,
        2 => 10,
        3 => 30,
        _ => 0,
    };
    Some(ChronoDuration::seconds(seconds) + ChronoDuration::minutes(minutes))
}
