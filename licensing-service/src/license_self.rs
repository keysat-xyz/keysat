//! Keysat-licenses-Keysat: dogfooded self-licensing layer.
//!
//! The Keysat package ships with the master public key embedded in
//! `TRUST_ROOT_PUBKEY_PEM` below. On every boot we look for a license
//! at `SELF_LICENSE_PATH` (or the `KEYSAT_LICENSE` env var), parse it
//! using the same wire-format machinery the daemon uses to issue
//! customer licenses, and verify its signature against the master
//! public key.
//!
//! Missing or invalid self-licenses log a warning and the daemon starts in
//! `Tier::Unlicensed`, which the admin UI labels "Creator" — the free tier
//! with the Creator caps applied (5 products, 5 policies per product, 10
//! active codes). The daemon is always functional out of the box; paying
//! lifts the caps and unlocks `recurring_billing` + `zaprite_payments`.
//!
//! The master pubkey is the *public* half of an Ed25519 keypair held by
//! the operator who issues Keysat-product licenses. It is not secret —
//! embedding it in source on GitHub is fine. Anyone with the *private*
//! half can mint Keysat self-licenses. On the master Keysat instance
//! that owner runs, the private half doubles as the per-instance
//! license-signing key (stored in the `server_keys` table); on every
//! other Keysat install the private half doesn't exist and the daemon
//! only ever verifies, never signs.

use crate::crypto::{parse_key, verify_payload};
use anyhow::{bail, Context, Result};
use ed25519_dalek::pkcs8::DecodePublicKey;
use ed25519_dalek::VerifyingKey;
use std::time::{SystemTime, UNIX_EPOCH};

/// Master public key for Keysat self-licensing. PEM-encoded Ed25519,
/// SubjectPublicKeyInfo wrapped (the format `openssl pkey -pubout`
/// emits). To rotate this in a future release: replace the const,
/// ship a new build, distribute fresh licenses to existing customers.
/// Existing customers' licenses won't verify against the new key —
/// that's the breaking event. Plan rotations carefully.
pub const TRUST_ROOT_PUBKEY_PEM: &str = "-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAgsromMy4osMJplX1rY0fd4ouS6wfkm/vfeY2gXEQHkA=
-----END PUBLIC KEY-----";

/// Where the daemon expects a self-license file. Single line, the raw
/// license-key string in `LIC1-…-…` format. Mounted from the
/// persistent data volume so it survives package upgrades.
pub const SELF_LICENSE_PATH: &str = "/data/keysat-license.txt";

#[derive(Debug, Clone)]
pub enum Tier {
    /// No self-license file, or verify failed. Surfaces as "Creator"
    /// in the admin UI — the free tier with the Creator caps applied.
    /// `reason` is for logs and the admin `/v1/admin/tier` payload, not
    /// shown to end users.
    Unlicensed { reason: String },
    /// Valid license verified against the trust-root.
    Licensed {
        license_id: uuid::Uuid,
        product_id: uuid::Uuid,
        /// Unix seconds; 0 means perpetual.
        expires_at: i64,
        entitlements: Vec<String>,
    },
}

impl Tier {
    /// String form for log / metrics labels. `Unlicensed` surfaces as
    /// "creator" since that's how the admin UI presents it — operators
    /// see one consistent name across logs and dashboard.
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::Unlicensed { .. } => "creator",
            Tier::Licensed { .. } => "licensed",
        }
    }
}

/// Boot-time check. Always returns `Ok` — Keysat boots into the Creator
/// (free) tier when no valid self-license is present, never refuses to
/// start. Logs a one-line info or warn line for operator visibility.
pub fn check_at_boot() -> Result<Tier> {
    let license_str = match read_license_string() {
        Some(s) => s,
        None => {
            let reason = format!(
                "no license at {} or KEYSAT_LICENSE env var; running Creator (free) tier",
                SELF_LICENSE_PATH
            );
            tracing::info!(tier = "creator", "Keysat self-license: {}", reason);
            return Ok(Tier::Unlicensed { reason });
        }
    };

    match verify_license(&license_str) {
        Ok(tier) => {
            log_licensed(&tier);
            Ok(tier)
        }
        Err(e) => {
            let reason = format!(
                "verification failed: {e:#} — falling back to Creator (free) tier"
            );
            tracing::warn!(tier = "creator", "Keysat self-license: {}", reason);
            Ok(Tier::Unlicensed { reason })
        }
    }
}

fn read_license_string() -> Option<String> {
    if let Ok(s) = std::env::var("KEYSAT_LICENSE") {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    let path = std::path::Path::new(SELF_LICENSE_PATH);
    if let Ok(s) = std::fs::read_to_string(path) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// Verify a license-key string against the embedded trust-root.
/// Returns the parsed `Tier::Licensed` on success.
pub fn verify_license(license_key: &str) -> Result<Tier> {
    let trust_key = parse_trust_root_pubkey()?;
    let (payload, signature, signed_bytes) =
        parse_key(license_key).context("license key parse failed")?;
    verify_payload(&trust_key, &signed_bytes, &signature)
        .context("license signature does not verify against master pubkey")?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if payload.is_expired_at(now) {
        bail!(
            "license expired at unix={} (now unix={})",
            payload.expires_at,
            now
        );
    }

    Ok(Tier::Licensed {
        license_id: payload.license_id,
        product_id: payload.product_id,
        expires_at: payload.expires_at,
        entitlements: payload.entitlements,
    })
}

/// Persist a verified license string to `SELF_LICENSE_PATH`. Caller
/// is expected to have run `verify_license` first.
pub fn write_license_file(license_key: &str) -> Result<()> {
    let path = std::path::Path::new(SELF_LICENSE_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent directory {}", parent.display()))?;
    }
    std::fs::write(path, format!("{}\n", license_key.trim()))
        .with_context(|| format!("writing license to {}", path.display()))?;
    Ok(())
}

fn parse_trust_root_pubkey() -> Result<VerifyingKey> {
    let pem = TRUST_ROOT_PUBKEY_PEM.trim();
    if pem.is_empty() {
        bail!("trust-root pubkey not embedded in this build");
    }
    let vk = VerifyingKey::from_public_key_pem(pem)
        .context("trust-root pubkey PEM parse failed")?;
    Ok(vk)
}

fn log_licensed(tier: &Tier) {
    if let Tier::Licensed {
        license_id,
        product_id,
        expires_at,
        entitlements,
    } = tier
    {
        let exp = if *expires_at == 0 {
            "perpetual".to_string()
        } else {
            format!("expires_at_unix={expires_at}")
        };
        let ents = if entitlements.is_empty() {
            "(none)".to_string()
        } else {
            entitlements.join(",")
        };
        tracing::info!(
            tier = "licensed",
            license = %license_id,
            product = %product_id,
            "Keysat self-license: VERIFIED — {exp}, entitlements={ents}"
        );
    }
}

/// Live-refresh the daemon's self-tier from the local `licenses` row.
///
/// `check_at_boot` verifies the on-disk LIC1 key against the embedded
/// trust root and reads its entitlements from the signed payload. That
/// signed set is the ceiling. This function lets issuer-applied changes
/// reach a running daemon without a restart — revocations, suspensions,
/// and downgrades — by re-reading the `licenses` row by license_id and
/// applying its current state. The signed key stays authoritative: the
/// DB row may *narrow* the tier but never *widen* it beyond what the
/// signature grants (see `clamp_to_signed_ceiling`).
///
/// Behavior:
/// - On-disk tier is `Unlicensed` → no-op (no license_id to look up).
/// - `licenses` row missing → keep the signed-payload tier as last-known
///   (legitimate for a daemon that's never synced its row).
/// - Row revoked or suspended → demote to `Unlicensed`.
/// - Otherwise → keep the signed product/expiry, with entitlements taken
///   from the DB row clamped to the signed ceiling.
///
/// Run from main.rs at boot (after `check_at_boot`) and on a 1-hour
/// interval thereafter. Also surfaced as an admin "Refresh self-license
/// tier" action for an immediate pass instead of waiting for the tick.
///
/// Non-master operators in v0.3+ can extend this to consult
/// `https://licensing.keysat.xyz/v1/validate` in addition to the local
/// DB. For v0.2.x it is local-DB-only; an honest downstream operator's
/// DB row matches its signed key, so the clamp is a no-op there.
pub async fn refresh_self_tier_from_db(
    pool: &sqlx::SqlitePool,
    current: &Tier,
) -> Tier {
    let license_id = match current {
        Tier::Licensed { license_id, .. } => license_id.to_string(),
        Tier::Unlicensed { .. } => return current.clone(),
    };

    let row = match crate::db::repo::get_license_by_id(pool, &license_id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            // Unknown to local DB — keep signed-payload tier. Could
            // happen if the daemon was issued elsewhere and only has
            // the on-disk key, no row in `licenses`.
            return current.clone();
        }
        Err(e) => {
            tracing::warn!(error = %e, "self-tier refresh: DB lookup failed; keeping last-known");
            return current.clone();
        }
    };

    if row.revoked_at.is_some() {
        let reason = format!(
            "license revoked at {}",
            row.revoked_at.as_deref().unwrap_or("?")
        );
        tracing::warn!(
            license_id = %license_id,
            "self-tier refresh: license is revoked; demoting to Unlicensed"
        );
        return Tier::Unlicensed { reason };
    }
    if row.suspended_at.is_some() {
        return Tier::Unlicensed {
            reason: format!(
                "license suspended at {}",
                row.suspended_at.as_deref().unwrap_or("?")
            ),
        };
    }

    // The signed key is the ceiling. Re-derive the entitlements it
    // grants — re-verifying it against the embedded trust root — and
    // clamp the live DB row to that set: the local row may narrow the
    // tier (a downgrade applied by the issuer) but must never widen it
    // beyond what the signature authorizes. Activation keeps the on-disk
    // key in sync, so this tracks the current license at boot and at
    // runtime. If the key can't be re-read mid-run, fall back to the
    // in-effect entitlements — themselves already clamped on a prior
    // pass — so a DB edit still can't widen the tier.
    let signed = read_license_string().and_then(|s| verify_license(&s).ok());
    let ceiling = match &signed {
        Some(tier) => entitlements_of(tier),
        None => entitlements_of(current),
    };
    let entitlements = clamp_to_signed_ceiling(row.entitlements.clone(), &ceiling);

    // Same product / license / expiry — only the entitlement set is
    // live. Cheap rebuild.
    let product_id = uuid::Uuid::parse_str(&row.product_id).ok();
    let license_id_uuid = uuid::Uuid::parse_str(&row.id).ok();
    let expires_at_unix = row
        .expires_at
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.timestamp())
        .unwrap_or(0);

    if let (Some(product_id), Some(license_id)) = (product_id, license_id_uuid) {
        Tier::Licensed {
            license_id,
            product_id,
            expires_at: expires_at_unix,
            entitlements,
        }
    } else {
        current.clone()
    }
}

/// Entitlements a tier carries; `Unlicensed` carries none.
fn entitlements_of(tier: &Tier) -> Vec<String> {
    match tier {
        Tier::Licensed { entitlements, .. } => entitlements.clone(),
        Tier::Unlicensed { .. } => Vec::new(),
    }
}

/// Restrict a DB-sourced entitlement set to the signed ceiling.
///
/// The signed self-license key bounds what the tier may grant. The
/// local `licenses` row may *narrow* the tier — an issuer-applied
/// downgrade — but anything in it that the signature does not grant is
/// dropped, so the row can never *widen* the tier past the ceiling.
/// Kept standalone so the invariant is unit-testable without the
/// offline signing key needed to mint a verifiable self-license.
fn clamp_to_signed_ceiling(db_entitlements: Vec<String>, signed: &[String]) -> Vec<String> {
    db_entitlements
        .into_iter()
        .filter(|e| signed.iter().any(|s| s == e))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn db_row_cannot_widen_beyond_signed_ceiling() {
        // Signed key grants only the free tier; a tampered DB row
        // claiming top-tier entitlements is stripped to the signed set.
        let signed = v(&["creator_only"]);
        let tampered = v(&[
            "unlimited_products",
            "unlimited_policies",
            "recurring_billing",
            "zaprite_payments",
            "patron",
            "creator_only",
        ]);
        assert_eq!(
            clamp_to_signed_ceiling(tampered, &signed),
            v(&["creator_only"])
        );
    }

    #[test]
    fn db_row_may_narrow_below_signed_ceiling() {
        // Signed key grants a broad set; an issuer-applied downgrade to
        // a smaller set in the DB row is honored (narrowing is allowed).
        let signed = v(&["unlimited_products", "recurring_billing", "zaprite_payments"]);
        let downgraded = v(&["unlimited_products"]);
        assert_eq!(
            clamp_to_signed_ceiling(downgraded, &signed),
            v(&["unlimited_products"])
        );
    }

    #[test]
    fn matching_entitlements_pass_through_unchanged() {
        let signed = v(&["unlimited_products", "recurring_billing"]);
        let db = v(&["unlimited_products", "recurring_billing"]);
        assert_eq!(clamp_to_signed_ceiling(db.clone(), &signed), db);
    }

    #[test]
    fn empty_signed_ceiling_strips_everything() {
        let db = v(&["unlimited_products", "patron"]);
        assert!(clamp_to_signed_ceiling(db, &[]).is_empty());
    }
}
