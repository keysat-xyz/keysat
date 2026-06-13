//! Admin CRUD for webhook endpoints.
//!
//! Operators register one or more URLs that will receive signed JSON
//! notifications of interesting events (`license.issued`, `license.revoked`,
//! `machine.activated`, etc.). Each endpoint has its own HMAC-SHA256 secret;
//! the delivery worker in [`crate::webhooks`] signs bodies with it.
//!
//! The secret is only returned to the operator in plaintext on create — once
//! they've stored it somewhere safe, later reads return the secret masked.
//! (If they lose it, they can rotate by deleting + recreating the endpoint.)

use crate::api::admin::{request_context, require_scope};
use crate::api::AppState;
use crate::db::repo;
use crate::error::AppResult;
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
pub struct CreateEndpointReq {
    pub url: String,
    /// Event types this endpoint is interested in. Use `["*"]` to receive all
    /// events. Examples: `license.issued`, `license.revoked`,
    /// `license.suspended`, `machine.activated`, `machine.deactivated`,
    /// `invoice.settled`.
    #[serde(default = "default_event_types")]
    pub event_types: Vec<String>,
    #[serde(default)]
    pub description: String,
    /// Optional explicit secret (hex, 32+ bytes). If omitted, the server
    /// generates a fresh 32-byte secret and returns it in the response.
    #[serde(default)]
    pub secret: Option<String>,
}

fn default_event_types() -> Vec<String> {
    vec!["*".to_string()]
}

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateEndpointReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_scope(&state, &headers, "webhooks:write").await?;
    let (ip, ua) = request_context(&headers);
    let secret = req.secret.unwrap_or_else(generate_secret);
    let ep = repo::create_webhook_endpoint(
        &state.db,
        &req.url,
        &secret,
        &req.event_types,
        &req.description,
    )
    .await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "webhook_endpoint.create",
        Some("webhook_endpoint"),
        Some(&ep.id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({
            "url": ep.url,
            "event_types": ep.event_types,
        }),
    )
    .await;
    // Return the full endpoint (including the plaintext secret) on create —
    // this is the only chance the operator gets to see it.
    Ok(Json(json!(ep)))
}

fn generate_secret() -> String {
    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    hex::encode(raw)
}

#[derive(Debug, Deserialize)]
pub struct ListEndpointsQuery {
    #[serde(default)]
    pub include_secret: bool,
}

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListEndpointsQuery>,
) -> AppResult<Json<Value>> {
    require_scope(&state, &headers, "webhooks:read").await?;
    let rows = repo::list_webhook_endpoints(&state.db, q.include_secret).await?;
    Ok(Json(json!({ "endpoints": rows })))
}

#[derive(Debug, Deserialize)]
pub struct SetActiveReq {
    pub active: bool,
}

pub async fn set_active(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<SetActiveReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_scope(&state, &headers, "webhooks:write").await?;
    let (ip, ua) = request_context(&headers);
    repo::set_webhook_active(&state.db, &id, req.active).await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "webhook_endpoint.set_active",
        Some("webhook_endpoint"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "active": req.active }),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_scope(&state, &headers, "webhooks:write").await?;
    let (ip, ua) = request_context(&headers);
    repo::delete_webhook_endpoint(&state.db, &id).await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "webhook_endpoint.delete",
        Some("webhook_endpoint"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({}),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}
