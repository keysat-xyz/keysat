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
use crate::error::{AppError, AppResult};
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::{Ipv4Addr, Ipv6Addr};
use url::{Host, Url};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateEndpointReq {
    pub url: String,
    /// Event types this endpoint is interested in. Use `["*"]` to receive all
    /// events. Examples: `license.issued`, `license.revoked`,
    /// `license.suspended`, `machine.activated`, `machine.deactivated`,
    /// `invoice.settled`. Accepts the `events` alias for the same field; a
    /// mistyped field name is rejected (422 — a deserialization/data error via
    /// `deny_unknown_fields`, not a 400 syntax error) rather than silently
    /// defaulting to `["*"]` (subscribe-to-all).
    #[serde(default = "default_event_types", alias = "events")]
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

/// Validate a webhook target URL before we persist it, to blunt SSRF: the
/// daemon later POSTs to this URL from inside the operator's network, so an
/// attacker who can register an endpoint could otherwise coerce it into
/// hitting internal-only services or non-http schemes.
///
/// Rejected: anything not parseable, any scheme other than http/https (blocks
/// `file://`, `ftp://`, `gopher://`, …), and loopback/link-local hosts
/// (`localhost`, `127.0.0.0/8`, `::1`, `169.254.0.0/16`, `fe80::/10`), including
/// the IPv6 rewrites of a blocked v4 address (mapped, compatible, NAT64).
///
/// This runs at REGISTRATION only. The delivery client in `webhooks.rs` therefore
/// refuses to follow redirects — otherwise a receiver could 302 the daemon to a
/// host this function would have rejected.
///
/// Deliberately still allowed: RFC-1918 / ULA private ranges (`10/8`,
/// `192.168/16`, `172.16-31/12`, `fc00::/7`). A self-hosted operator may
/// legitimately webhook a LAN service on the same box or network.
fn validate_webhook_url(raw: &str) -> Result<(), AppError> {
    let url = Url::parse(raw)
        .map_err(|_| AppError::BadRequest(format!("invalid webhook url: {raw}")))?;

    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(AppError::BadRequest(format!(
                "webhook url scheme must be http or https, got {other}"
            )))
        }
    }

    let blocked = match url.host() {
        // Loopback by name — the url crate keeps a trailing FQDN dot
        // (`localhost.`), so strip trailing dots before comparing.
        Some(Host::Domain(d)) => d.trim_end_matches('.').eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ipv4_is_blocked(&ip),
        Some(Host::Ipv6(ip)) => ipv6_is_blocked(&ip),
        // No host at all (can't normally happen for http/https, but be safe).
        None => {
            return Err(AppError::BadRequest(
                "webhook url must include a host".into(),
            ))
        }
    };
    if blocked {
        return Err(AppError::BadRequest(format!(
            "webhook url host may not be loopback/link-local/unspecified: {raw}"
        )));
    }
    Ok(())
}

/// v4 hosts we refuse to webhook: loopback (`127.0.0.0/8`), link-local
/// (`169.254.0.0/16`), and the unspecified `0.0.0.0` — Linux routes a
/// `connect(0.0.0.0)` to `127.0.0.1`, so it's a standard SSRF-to-localhost
/// vector even though Rust doesn't classify it as loopback.
fn ipv4_is_blocked(ip: &Ipv4Addr) -> bool {
    ip.is_loopback() || ip.is_link_local() || ip.is_unspecified()
}

/// v6 hosts we refuse to webhook: `::1` loopback, `::` unspecified, `fe80::/10`
/// link-local, AND any form embedding a blocked IPv4 address —
/// IPv4-mapped/compatible (`::ffff:127.0.0.1`, `::127.0.0.1`) or wrapped in the
/// NAT64 well-known prefix (`64:ff9b::7f00:1`). Without those, a mapped or
/// translated loopback slips past `is_loopback()` (which only matches `::1`) and
/// the `fe80::/10` prefix test (which sees a `0` first segment for a mapped addr).
fn ipv6_is_blocked(ip: &Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || is_ipv6_link_local(ip) {
        return true;
    }
    // `to_ipv4()` unwraps both `::ffff:a.b.c.d` (mapped) and `::a.b.c.d`
    // (compatible) into the embedded v4 address; re-run the v4 blocklist on it.
    if matches!(ip.to_ipv4(), Some(v4) if ipv4_is_blocked(&v4)) {
        return true;
    }
    matches!(nat64_embedded_ipv4(ip), Some(v4) if ipv4_is_blocked(&v4))
}

/// `64:ff9b::/96` is the NAT64 well-known prefix (RFC 6052): on a network running
/// NAT64/DNS64, `64:ff9b::7f00:1` is translated to `127.0.0.1`. Rust's `to_ipv4()`
/// only unwraps the mapped/compatible forms, so the trailing 32 bits have to be
/// extracted by hand or the whole v4 blocklist is bypassed by rewriting the
/// address. Only reachable on a network actually doing NAT64 translation, which is
/// uncommon for a self-hosted box — but the check costs nothing and can only make
/// the filter stricter.
fn nat64_embedded_ipv4(ip: &Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    if s[0] != 0x0064 || s[1] != 0xff9b || s[2] != 0 || s[3] != 0 || s[4] != 0 || s[5] != 0 {
        return None;
    }
    Some(Ipv4Addr::new(
        (s[6] >> 8) as u8,
        (s[6] & 0xff) as u8,
        (s[7] >> 8) as u8,
        (s[7] & 0xff) as u8,
    ))
}

/// `fe80::/10` — the first 10 bits are `1111111010`. `Ipv6Addr` has no stable
/// `is_unicast_link_local()` (still behind the unstable `ip` feature), so check
/// the prefix by hand. (IPv4-mapped loopback/link-local is handled separately in
/// [`ipv6_is_blocked`] via `to_ipv4()`, not here.)
fn is_ipv6_link_local(ip: &Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateEndpointReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_scope(&state, &headers, "webhooks:write").await?;
    let (ip, ua) = request_context(&headers);
    validate_webhook_url(&req.url)?;
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
