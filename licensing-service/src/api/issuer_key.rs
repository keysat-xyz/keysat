//! Issuer-key endpoints — public read of the public key, admin-only import.
//!
//! Used exactly once, by exactly one operator: when bootstrapping a
//! "master Keysat" instance (the one that issues licenses for the Keysat
//! package itself). The master operator pre-generated an Ed25519 keypair
//! offline; this endpoint takes the PEM-encoded private half and stores
//! it as the daemon's signing keypair, replacing the auto-generated one
//! that gets created on first boot.
//!
//! ## Why this isn't a StartOS Action
//!
//! 95% of Keysat operators install Keysat to sell their own software.
//! Their auto-generated issuer key is exactly what they want; they never
//! need this endpoint. Surfacing an "import issuer key" button in every
//! operator's StartOS Actions tab would create cognitive load (am I
//! supposed to do this?) for zero benefit. So this lives as an admin
//! API endpoint only — invisible by default, callable via curl during
//! the master-bootstrap procedure documented in
//! `MASTER_KEYPAIR_PROCEDURE.md`.
//!
//! ## Safety guards
//!
//! Replacing the issuer key after licenses have been issued would
//! invalidate every previously-signed customer license. To prevent that
//! footgun, the endpoint refuses if any license rows exist in the
//! database. The master Keysat instance hasn't issued anything when it
//! gets bootstrapped, so this guard never trips during legitimate use
//! and prevents the worst-case mistake.
//!
//! ## After successful import
//!
//! The new keypair lands in the `server_keys` table immediately, but the
//! daemon's in-memory `AppState.keypair` still holds the old one until
//! restart. The endpoint returns a `restart_required: true` so the
//! operator (or their orchestration) knows to bounce the service before
//! the new key takes effect.

use crate::api::admin::{request_context, require_admin};
use crate::api::AppState;
use crate::error::{AppError, AppResult};
use axum::{body::Bytes, extract::State, http::HeaderMap, Json};
use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey};
use ed25519_dalek::SigningKey;
use serde_json::{json, Value};

pub async fn import(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);

    let pem = std::str::from_utf8(&body)
        .map_err(|_| AppError::BadRequest("body is not valid UTF-8".into()))?
        .trim();
    if pem.is_empty() {
        return Err(AppError::BadRequest("body is empty".into()));
    }
    if !pem.contains("-----BEGIN") || !pem.contains("PRIVATE KEY-----") {
        return Err(AppError::BadRequest(
            "expected a PEM-encoded private key (must contain BEGIN/END PRIVATE KEY)".into(),
        ));
    }

    // Parse + validate the supplied PEM.
    let signing = SigningKey::from_pkcs8_pem(pem).map_err(|e| {
        AppError::BadRequest(format!("could not parse Ed25519 private key: {e}"))
    })?;
    let verifying = signing.verifying_key();

    // Re-encode through pkcs8 so we always store a normalized form. This
    // also catches any encoding oddity on the input side that would have
    // tripped a future load.
    use pkcs8::LineEnding;
    let priv_pem = signing
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("re-encode private key: {e}")))?
        .to_string();
    let pub_pem = verifying
        .to_public_key_pem(LineEnding::LF)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encode public key: {e}")))?;

    // Safety guard: refuse if any licenses have already been issued by
    // this Keysat. Replacing the issuer key would invalidate them.
    let licenses_exist: bool =
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM licenses LIMIT 1)")
            .fetch_one(&state.db)
            .await?;
    if licenses_exist {
        return Err(AppError::Conflict(
            "this Keysat has already issued at least one license; importing a new \
             issuer key would invalidate every previously-signed license. Refusing. \
             Use this endpoint only on a fresh master-Keysat install before any \
             licenses have been issued."
                .into(),
        ));
    }

    // Upsert the keypair into server_keys row id=1. SQLite's INSERT ON
    // CONFLICT is the idiomatic way to do this in one statement.
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO server_keys (id, algorithm, public_key_pem, private_key_pem, created_at)
         VALUES (1, 'ed25519', ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
             algorithm = excluded.algorithm,
             public_key_pem = excluded.public_key_pem,
             private_key_pem = excluded.private_key_pem,
             created_at = excluded.created_at",
    )
    .bind(&pub_pem)
    .bind(&priv_pem)
    .bind(&now)
    .execute(&state.db)
    .await?;

    // Audit-log this prominently. There is no scenario where a regular
    // operator should be running this; if it shows up in the audit log
    // unexpectedly, that's a red flag worth investigating.
    let _ = crate::db::repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "issuer_key.import",
        Some("server_key"),
        None,
        ip.as_deref(),
        ua.as_deref(),
        &json!({
            "public_key_pem": pub_pem,
            "note": "master-bootstrap import",
        }),
    )
    .await;

    tracing::warn!(
        public_key = %pub_pem.lines().nth(1).unwrap_or(""),
        "issuer key imported via admin endpoint — restart the service for the new key to take effect"
    );

    Ok(Json(json!({
        "ok": true,
        "public_key_pem": pub_pem,
        "restart_required": true,
        "message": "Issuer key imported. Restart the Keysat service for the new \
                    key to take effect — until then, in-memory state still holds \
                    the previous keypair."
    })))
}


/// PUBLIC: GET /v1/issuer/public-key — returns the daemon's signing
/// public key in PEM and a couple of conveniences. No auth required —
/// the public key is, by definition, public. Used by SDK consumers and
/// by the admin Overview's "Embed your public key" tip card.
pub async fn public(
    axum::extract::State(state): axum::extract::State<crate::api::AppState>,
) -> Json<serde_json::Value> {
    Json(json!({
        "public_key_pem": state.keypair.public_key_pem,
        "key_algorithm": "ed25519",
        "key_format_version": crate::crypto::KEY_VERSION,
    }))
}
