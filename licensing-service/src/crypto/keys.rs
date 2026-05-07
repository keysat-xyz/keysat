//! Server key lifecycle: generate on first boot, load on subsequent boots.
//!
//! Keys are stored in SQLite (rather than on the filesystem) so the same
//! backup mechanism that protects licenses also protects the signing key.
//! On StartOS, the database file lives under the service's encrypted data
//! volume, so at-rest encryption is handled by the OS.

use anyhow::{Context, Result};
use chrono::Utc;
use ed25519_dalek::pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey};
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use sqlx::SqlitePool;

/// Both halves of the server keypair.
#[derive(Clone)]
pub struct ServerKeypair {
    pub signing: SigningKey,
    pub verifying: VerifyingKey,
    /// PEM-encoded public key, for display / SDK bundling.
    pub public_key_pem: String,
}

/// Load the keypair from the DB, generating and persisting a new one if no
/// row exists. This function is idempotent and safe to call on every boot.
pub async fn load_or_generate(pool: &SqlitePool) -> Result<ServerKeypair> {
    // Try to load.
    let existing = sqlx::query_as::<_, (String, String)>(
        "SELECT public_key_pem, private_key_pem FROM server_keys WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?;

    if let Some((pub_pem, priv_pem)) = existing {
        let signing = SigningKey::from_pkcs8_pem(&priv_pem)
            .context("failed to parse stored private key")?;
        let verifying = VerifyingKey::from_public_key_pem(&pub_pem)
            .context("failed to parse stored public key")?;
        return Ok(ServerKeypair {
            signing,
            verifying,
            public_key_pem: pub_pem,
        });
    }

    // Generate a new keypair.
    let signing = SigningKey::generate(&mut OsRng);
    let verifying = signing.verifying_key();

    use pkcs8::LineEnding;
    let priv_pem = signing
        .to_pkcs8_pem(LineEnding::LF)
        .context("failed to encode private key to PEM")?
        .to_string();
    let pub_pem = verifying
        .to_public_key_pem(LineEnding::LF)
        .context("failed to encode public key to PEM")?;

    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO server_keys (id, algorithm, public_key_pem, private_key_pem, created_at)
         VALUES (1, 'ed25519', ?, ?, ?)",
    )
    .bind(&pub_pem)
    .bind(&priv_pem)
    .bind(&now)
    .execute(pool)
    .await?;

    tracing::info!("generated new Ed25519 server signing key");

    Ok(ServerKeypair {
        signing,
        verifying,
        public_key_pem: pub_pem,
    })
}
