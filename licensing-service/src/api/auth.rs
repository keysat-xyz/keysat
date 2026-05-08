//! Web-UI password authentication. Sits alongside the existing admin
//! API key path; admin endpoints accept either credential.
//!
//! Flow:
//!   1. Operator sets a password via the StartOS action "Set web UI
//!      password" (which POSTs to /v1/admin/web-password using the API
//!      key). Daemon argon2id-hashes the password and stores it under
//!      the settings key `web_ui_password_hash`.
//!   2. SPA login form POSTs `{password}` to /admin/login. Daemon
//!      verifies, mints a 32-byte random session token, persists in
//!      sessions table, sets it as `keysat_session` HttpOnly +
//!      SameSite=Strict cookie. Token TTL: 24h, sliding via last_seen_at
//!      bump on every authenticated request.
//!   3. Subsequent admin calls present the cookie OR the API key.
//!      `require_admin_or_session` accepts either.
//!   4. /admin/logout deletes the session row + clears the cookie.
//!   5. A background task in main.rs reaps expired sessions hourly.

use crate::api::admin::{request_context, require_admin};
use crate::api::AppState;
use crate::db::repo;
use crate::error::{AppError, AppResult};
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use rand::rngs::OsRng;
use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::Response,
    Json,
};
use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};

/// Settings key for the argon2id password hash (PHC-format string).
pub const SETTING_WEB_UI_PASSWORD_HASH: &str = "web_ui_password_hash";
/// Cookie name for the session token.
pub const SESSION_COOKIE: &str = "keysat_session";
/// Default session TTL — 24 hours from creation. Renewed on every
/// authenticated request via last_seen_at bump (sliding window).
pub const SESSION_TTL_SECS: i64 = 60 * 60 * 24;

/// Hash a plaintext password using Argon2id with the PHC-recommended
/// parameters. Returns a PHC-format string suitable for storage.
pub fn hash_password(plaintext: &str) -> AppResult<String> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    argon2
        .hash_password(plaintext.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("argon2 hash failed: {e}")))
}

/// Verify a plaintext password against a stored PHC-format hash.
pub fn verify_password(plaintext: &str, phc_hash: &str) -> bool {
    PasswordHash::new(phc_hash)
        .and_then(|parsed| Argon2::default().verify_password(plaintext.as_bytes(), &parsed))
        .is_ok()
}

/// Generate a cryptographically random 32-byte session token, URL-safe
/// base64-encoded (no padding). 256 bits of entropy.
fn new_session_token() -> String {
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

#[derive(Debug, Deserialize)]
pub struct SetPasswordReq {
    /// Plaintext password. Minimum 12 chars enforced server-side.
    pub password: String,
}

/// Admin-only (via API key): sets or rotates the web UI password.
/// Invalidates all existing sessions when the password changes so that
/// stale browsers re-authenticate. Audit-logged.
pub async fn set_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SetPasswordReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    if req.password.len() < 12 {
        return Err(AppError::BadRequest(
            "password must be at least 12 characters".into(),
        ));
    }
    let hash = hash_password(&req.password)?;
    repo::settings_set(&state.db, SETTING_WEB_UI_PASSWORD_HASH, Some(&hash)).await?;
    // Invalidate all existing sessions on rotation so the new password
    // takes effect everywhere immediately.
    repo::delete_all_sessions(&state.db).await?;
    let _ = repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "web_ui.set_password",
        Some("settings"),
        Some(SETTING_WEB_UI_PASSWORD_HASH),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "ok": true }),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
pub struct LoginReq {
    pub password: String,
}

/// Public login endpoint. Verifies the password against the stored hash;
/// on success, issues a session and returns it as an HttpOnly cookie.
/// Per-IP rate limiting on bad attempts is enforced via the existing
/// rate_limit module. Returns 204 No Content on success (no body — the
/// cookie is the credential), 401 on bad password, 503 when no password
/// is configured yet.
pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<LoginReq>,
) -> Result<Response, AppError> {
    let (ip, ua) = request_context(&headers);

    // Brute-force protection: token-bucket per client IP. Capacity 5,
    // refills at 1 token / 180s (so a sustained brute-forcer is throttled
    // to ~20 attempts/hour after the initial burst). Backed by SQLite via
    // the existing rate_limit::consume helper.
    let bucket_key = ip.as_deref().unwrap_or("unknown");
    let allowed = crate::rate_limit::consume(
        &state.db,
        "web_login",
        bucket_key,
        5.0,
        1.0 / 180.0,
    )
    .await?;
    if !allowed {
        return Err(AppError::TooManyRequests(
            "too many login attempts; try again in a few minutes".into(),
        ));
    }

    let stored = repo::settings_get(&state.db, SETTING_WEB_UI_PASSWORD_HASH).await?;
    let Some(hash) = stored else {
        return Err(AppError::ServiceUnavailable(
            "web UI password is not configured. Set one via the StartOS \"Set web UI password\" action."
                .into(),
        ));
    };

    if !verify_password(&req.password, &hash) {
        // Audit failed attempt — useful for spotting brute-force.
        let _ = repo::insert_audit(
            &state.db,
            "web_ui",
            None,
            "web_ui.login_failed",
            Some("settings"),
            Some(SETTING_WEB_UI_PASSWORD_HASH),
            ip.as_deref(),
            ua.as_deref(),
            &json!({}),
        )
        .await;
        return Err(AppError::Unauthorized);
    }

    let token = new_session_token();
    let now = chrono::Utc::now();
    let expires = now + chrono::Duration::seconds(SESSION_TTL_SECS);
    repo::create_session(
        &state.db,
        &token,
        &now.to_rfc3339(),
        &expires.to_rfc3339(),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;

    let _ = repo::insert_audit(
        &state.db,
        "web_ui",
        None,
        "web_ui.login_ok",
        Some("session"),
        Some(&token),
        ip.as_deref(),
        ua.as_deref(),
        &json!({}),
    )
    .await;

    // HttpOnly + Secure + SameSite=Strict + path=/ keeps the cookie out
    // of JS and out of cross-site contexts. Max-Age in seconds.
    let cookie = format!(
        "{name}={token}; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age={ttl}",
        name = SESSION_COOKIE,
        token = token,
        ttl = SESSION_TTL_SECS,
    );
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(header::SET_COOKIE, cookie)
        .body(Body::empty())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("response build failed: {e}")))
}

/// Logs the caller out. Reads the session cookie, deletes the matching
/// session row, and emits a Set-Cookie that clears the cookie on the
/// browser side. Idempotent: returns 204 even if the cookie is missing
/// or already invalid.
pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if let Some(token) = extract_session_cookie(&headers) {
        let _ = repo::delete_session(&state.db, &token).await;
    }
    let cleared = format!(
        "{name}=; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=0",
        name = SESSION_COOKIE,
    );
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(header::SET_COOKIE, cleared)
        .body(Body::empty())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("response build failed: {e}")))
}

/// Lightweight status probe used by the SPA on first load. Tells the
/// client whether a password has been configured (so it can show "Set a
/// password via StartOS Actions" if not) and whether the current session
/// cookie is valid (so it can skip the login form).
pub async fn login_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let has_password = repo::settings_get(&state.db, SETTING_WEB_UI_PASSWORD_HASH)
        .await?
        .is_some();
    let logged_in = if let Some(token) = extract_session_cookie(&headers) {
        repo::is_session_valid(&state.db, &token).await?
    } else {
        false
    };
    Ok(Json(json!({
        "has_password": has_password,
        "logged_in": logged_in,
    })))
}

/// Read the session token out of the Cookie header, if present. Naive
/// parser — handles the typical `Cookie: a=1; keysat_session=…; b=2`
/// shape and is robust to quoted values and stray whitespace.
pub fn extract_session_cookie(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for pair in raw.split(';') {
        let pair = pair.trim();
        if let Some((k, v)) = pair.split_once('=') {
            if k.trim() == SESSION_COOKIE {
                return Some(v.trim().trim_matches('"').to_string());
            }
        }
    }
    None
}
