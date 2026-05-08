//! Bridges cookie-based web-UI sessions onto the existing API-key
//! `require_admin` guard.
//!
//! When an incoming request has no `Authorization` header but does carry
//! a valid `keysat_session` cookie, this middleware injects
//! `Authorization: Bearer <api_key>` on the request. Downstream the
//! `require_admin` guard sees a bearer token and treats the call as
//! authenticated — no per-handler changes required.
//!
//! Public endpoints (buy page, /v1/purchase, /v1/redeem, /v1/validate,
//! /v1/issuer/public-key, etc.) don't read the Authorization header, so
//! injecting it for them is benign — and the middleware short-circuits
//! anyway when there's no session cookie present.

use crate::api::AppState;
use crate::db::repo;
use axum::{
    extract::{Request, State},
    http::{header, HeaderValue},
    middleware::Next,
    response::Response,
};

pub async fn session_to_bearer(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    // Fast path: caller already supplied an Authorization header — leave
    // the request alone.
    if req.headers().contains_key(header::AUTHORIZATION) {
        return next.run(req).await;
    }
    // Fast path: no cookie at all.
    let token = match crate::api::auth::extract_session_cookie(req.headers()) {
        Some(t) => t,
        None => return next.run(req).await,
    };
    // DB hit only when there IS a session cookie.
    let valid = repo::is_session_valid(&state.db, &token)
        .await
        .unwrap_or(false);
    if valid {
        let api_key = state.config.admin_api_key.clone();
        if let Ok(hv) = HeaderValue::from_str(&format!("Bearer {api_key}")) {
            req.headers_mut().insert(header::AUTHORIZATION, hv);
        }
    }
    next.run(req).await
}
