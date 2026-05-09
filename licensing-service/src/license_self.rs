//! Keysat-licenses-Keysat: dogfooded self-licensing layer.
//!
//! The Keysat package ships with the master public key embedded in
//! `TRUST_ROOT_PUBKEY_PEM` below. On every boot we look for a license
//! at `SELF_LICENSE_PATH` (or the `KEYSAT_LICENSE` env var), parse it
//! using the same wire-format machinery the daemon uses to issue
//! customer licenses, and verify its signature against the master
//! public key.
//!
//! Two modes:
//!   - `Permissive` (default for dev builds): missing or invalid
//!     licenses log a warning and the daemon starts in
//!     `Tier::Unlicensed`. No features are gated yet — that's a
//!     future v0.2.x flip.
//!   - `Enforce`: missing or invalid licenses cause the daemon to
//!     refuse to start. Set at compile time via the
//!     `KEYSAT_LICENSE_ENFORCE=1` env var. Marketplace builds set
//!     this; local dev builds don't.
//!
//! The master pubkey is the *public* half of an Ed25519 keypair held
//! offline by the keysat.xyz team. It is not secret — embedding it in
//! source on GitHub is fine. Anyone with the *private* half can mint
//! Keysat self-licenses; the private half lives on paper backup +
//! hardware-token storage and never touches a connected machine
//! except briefly when a master Keysat instance is being initialized.

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

/// Build-time enforcement toggle. `KEYSAT_LICENSE_ENFORCE=1` at
/// `cargo build` time enables enforce mode.
const ENFORCE_FLAG: Option<&str> = option_env!("KEYSAT_LICENSE_ENFORCE");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Missing/invalid license logs a warning and continues. Default.
    Permissive,
    /// Missing/invalid license refuses to start the daemon.
    Enforce,
}

pub fn mode() -> Mode {
    match ENFORCE_FLAG {
        Some("1") | Some("true") | Some("yes") => Mode::Enforce,
        _ => Mode::Permissive,
    }
}

#[derive(Debug, Clone)]
pub enum Tier {
    /// No license configured, or license verify failed in permissive mode.
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
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::Unlicensed { .. } => "unlicensed",
            Tier::Licensed { .. } => "licensed",
        }
    }
}

/// Boot-time check. In permissive mode this always returns `Ok`; in
/// enforce mode it returns `Err` on missing / invalid / expired
/// licenses, which causes `main` to bail out before we open any
/// network sockets.
pub fn check_at_boot() -> Result<Tier> {
    let mode = mode();
    tracing::info!(
        mode = mode.as_str(),
        "Keysat self-license check (mode={})",
        mode.as_str()
    );

    let license_str = match read_license_string() {
        Some(s) => s,
        None => {
            let reason = format!(
                "no license at {} or KEYSAT_LICENSE env var",
                SELF_LICENSE_PATH
            );
            return handle_missing_or_invalid(mode, reason, None);
        }
    };

    match verify_license(&license_str) {
        Ok(tier) => {
            log_licensed(&tier);
            Ok(tier)
        }
        Err(e) => {
            let reason = format!("verification failed: {e:#}");
            handle_missing_or_invalid(mode, reason, Some(e))
        }
    }
}

fn handle_missing_or_invalid(
    mode: Mode,
    reason: String,
    err: Option<anyhow::Error>,
) -> Result<Tier> {
    match mode {
        Mode::Permissive => {
            tracing::warn!(
                tier = "unlicensed",
                "Keysat self-license: {} — running unlicensed (permissive build)",
                reason
            );
            Ok(Tier::Unlicensed { reason })
        }
        Mode::Enforce => {
            tracing::error!(
                "Keysat self-license: {} — refusing to start. \
                 Activate via StartOS → Keysat → Actions → Activate Keysat license.",
                reason
            );
            match err {
                Some(e) => Err(e.context("self-license invalid (enforce mode)")),
                None => bail!("self-license missing (enforce mode): {reason}"),
            }
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

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Permissive => "permissive",
            Mode::Enforce => "enforce",
        }
    }
}

/// Live-refresh the daemon's self-tier from the local `licenses` row.
///
/// Why this exists: `check_at_boot` parses the on-disk LIC1 key and
/// extracts entitlements from the SIGNED PAYLOAD. Those entitlements
/// are immutable for the life of that key — the operator can't ever
/// downgrade themselves by editing the DB row, because the daemon
/// trusts the signature, not the DB.
///
/// In practice that means tier upgrades / downgrades / revocations
/// applied via admin (or eventually, via an upstream master) don't
/// propagate to a running daemon — even though the daemon is online
/// and the data is right there in its own DB. This function is the
/// fix: re-read the licenses row by license_id and use the LIVE
/// entitlements + revocation status. The on-disk signed key is kept
/// as proof-of-authenticity (signature still verifies) but the live
/// DB row is the source of tier truth.
///
/// Behavior:
/// - If the on-disk tier is `Unlicensed`, do nothing — there's no
///   license_id to look up.
/// - If the licenses row is missing in the DB (legitimate for a
///   daemon that's never been online to sync, e.g.), keep the
///   signed-payload tier as last-known.
/// - If the row is revoked, demote to `Unlicensed { reason: "revoked" }`.
/// - Otherwise, replace the entitlements vec with whatever the DB
///   row currently says.
///
/// Run from main.rs at boot (after `check_at_boot`) and on a 1-hour
/// interval thereafter. Also surfaced as an admin "Refresh
/// self-license tier" action for operators who want to trigger
/// immediately after a change instead of waiting for the next tick.
///
/// Non-master operators in v0.3+ can extend this to call
/// `https://licensing.keysat.xyz/v1/validate` instead of (or in
/// addition to) the local DB. For v0.2.x, local-DB-only — which is
/// the right thing for the master Keysat (which is selling its own
/// licenses) and a no-op-but-safe for downstream operators (their
/// own DB row hasn't been mutated, so live read returns the same
/// thing as the boot-time signed-payload extraction).
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

    // Pull the LIVE entitlements from the DB. These can differ from
    // the signed payload's entitlements (which were baked at signing
    // time) if an admin has done a Change Tier on this license.
    let entitlements = row.entitlements.clone();

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
