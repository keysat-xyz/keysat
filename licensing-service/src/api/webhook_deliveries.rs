//! Admin views over the outbound webhook delivery queue.
//!
//! Companion to `webhook_endpoints.rs`: that module manages the
//! configured subscriber URLs; this one exposes the row-level history
//! of attempts (success, in-flight retries, dead-lettered failures)
//! and lets operators manually re-queue a dead delivery for another
//! pass through the worker.
//!
//! Why this exists: the worker in `crate::webhooks` retries failed
//! deliveries with exponential backoff up to 10 attempts, then sets
//! `next_attempt_at = NULL` and walks away. Pre-this-module, those
//! "dead-lettered" rows were invisible — operators had no surface to
//! discover, inspect, or recover from them. A subscriber endpoint
//! that was down for >6h during a license-issuance burst would
//! silently lose those events forever.

use crate::api::admin::{request_context, require_admin};
use crate::api::AppState;
use crate::db::repo::{self, DeliveryStatusFilter};
use crate::error::{AppError, AppResult};
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

const DEFAULT_LIMIT: i64 = 100;
const MAX_LIMIT: i64 = 500;

#[derive(Debug, Deserialize)]
pub struct ListDeliveriesQuery {
    /// Filter by configured endpoint id. Omit for all endpoints.
    pub endpoint_id: Option<String>,
    /// One of `pending` | `delivered` | `failed` | `all`. Defaults to
    /// `all`. The `failed` filter is the dead-letter queue — rows
    /// where the worker exhausted retries.
    pub status: Option<String>,
    /// Cap on rows returned. Defaults to 100; max 500.
    pub limit: Option<i64>,
}

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListDeliveriesQuery>,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let status = match q.status.as_deref() {
        Some(s) => DeliveryStatusFilter::parse(s).ok_or_else(|| {
            AppError::BadRequest(format!(
                "invalid status filter '{s}'; expected pending|delivered|failed|all"
            ))
        })?,
        None => DeliveryStatusFilter::All,
    };
    let limit = q
        .limit
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);
    let rows = repo::list_deliveries(
        &state.db,
        q.endpoint_id.as_deref(),
        status,
        limit,
    )
    .await?;
    Ok(Json(json!({ "deliveries": rows })))
}

/// Manual re-queue for a dead-lettered (or otherwise stuck)
/// delivery. The worker will pick it up on the next 5s tick.
///
/// 404 if the delivery id doesn't exist; 200 on success with the
/// updated row in the body so the SPA can re-render the list with
/// the new state immediately.
pub async fn retry(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    let delivery = repo::requeue_delivery(&state.db, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("webhook delivery '{id}'")))?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "webhook_delivery.retry",
        Some("webhook_delivery"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({
            "endpoint_id": delivery.endpoint_id,
            "event_type": delivery.event_type,
        }),
    )
    .await;
    Ok(Json(json!(delivery)))
}
