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

use crate::crypto::{parse_key, verify_payload, LicensePayload};
use crate::error::AppResult;
use crate::models::License;
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

/// Parse and signature-verify a license key, **without** judging its expiry.
///
/// Split out of [`verify_license`] because that function folds two unrelated
/// verdicts into one `Err`: a key whose signature does not verify and a key
/// that verified perfectly but has expired both come back as an opaque
/// `anyhow::Error`, so a caller cannot tell them apart. They are not the same
/// event — one means the file is not a Keysat license at all, the other means
/// the operator's subscription lapsed — and the health summary has to report
/// them as different conditions with different remedies. `verify_license` is
/// written in terms of this so the two cannot drift.
fn verify_key_signature(license_key: &str) -> Result<LicensePayload> {
    let trust_key = parse_trust_root_pubkey()?;
    let (payload, signature, signed_bytes) =
        parse_key(license_key).context("license key parse failed")?;
    verify_payload(&trust_key, &signed_bytes, &signature)
        .context("license signature does not verify against master pubkey")?;
    Ok(payload)
}

/// Verify a license-key string against the embedded trust-root.
/// Returns the parsed `Tier::Licensed` on success.
pub fn verify_license(license_key: &str) -> Result<Tier> {
    let payload = verify_key_signature(license_key)?;

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
/// downgrades, and the key's own expiry — by re-verifying the on-disk
/// key and re-reading the `licenses` row by license_id. The signed key
/// stays authoritative: the DB row may *narrow* the tier but never
/// *widen* it beyond what the signature grants (see
/// `clamp_to_signed_ceiling`).
///
/// Behavior:
/// - On-disk tier is `Unlicensed` → no-op (no license_id to look up).
/// - Signed key no longer verifies (expired, tampered, corrupt) → demote
///   to `Unlicensed`.
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

    // Re-read and re-verify the on-disk/env self-license key on every
    // pass. This is what makes the key's own EXPIRY (and any tampering or
    // corruption) take effect on a *running* daemon, not just at the next
    // restart — mirroring how the licenses we issue are re-checked on
    // every `/v1/validate`. Done before the DB lookup so an expired key
    // demotes even when the daemon has no synced `licenses` row. The
    // verified entitlements double as the ceiling the DB row is clamped
    // to below.
    let signed_ceiling = match read_license_string() {
        Some(key) => match verify_license(&key) {
            Ok(tier) => Some(entitlements_of(&tier)),
            // Present but no longer verifies — expired, tampered, or
            // corrupt. Demote to Creator (free), same as revoked/suspended.
            // A read racing a concurrent `activate` file-write could trip
            // this transiently. It does NOT self-heal on the next pass:
            // once demoted, `current` is `Unlicensed` and the early return
            // at the top of this function fires before anything is re-read.
            // Recovering needs the "Activate Keysat license" action or a
            // daemon restart — the same two remedies the health summary's
            // `stale_tier` verdict names, and the reason it must never send
            // an operator to the "Refresh self-license tier" action.
            Err(e) => {
                tracing::warn!(
                    license_id = %license_id,
                    "self-tier refresh: self-license no longer verifies ({e:#}); demoting to Creator (free) tier"
                );
                return Tier::Unlicensed {
                    reason: format!("self-license re-verification failed: {e:#}"),
                };
            }
        },
        // No key on disk or in env though we booted Licensed — the source
        // was removed. Keep last-known entitlements as the ceiling (offline
        // grace), but log it.
        None => {
            tracing::warn!(
                license_id = %license_id,
                "self-tier refresh: self-license source missing; keeping last-known entitlements"
            );
            None
        }
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

    // Clamp the live DB row to the signed ceiling derived above: the row
    // may narrow the tier (an issuer-applied downgrade) but must never
    // widen it beyond what the signature authorizes. If the key source
    // was missing, fall back to the in-effect entitlements — themselves
    // already clamped on a prior pass — so a DB edit still can't widen.
    let ceiling = match &signed_ceiling {
        Some(c) => c.clone(),
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

// =====================================================================
// Self-license health — what `GET /v1/admin/health-summary` reports about
// the daemon's own Keysat license.
// =====================================================================

/// How far ahead of expiry the self-license condition starts warning.
///
/// Two weeks is the shortest lead time that still lets an operator notice on a
/// weekly cadence and act before the daemon silently drops to the free tier.
pub const EXPIRING_SOON_SECONDS: i64 = 14 * 24 * 60 * 60;

/// What the on-disk (or `KEYSAT_LICENSE`) self-license key was observed to be.
///
/// This is the seam that makes the rest of this section testable, and it is
/// deliberately the **only** thing that needs the master private half. Verifying
/// a key against [`TRUST_ROOT_PUBKEY_PEM`] requires a signature no test can
/// produce, so anything downstream of the verification — the whole decision
/// table in [`classify`], and the row lookup and clock in [`observe_with`] —
/// would be unreachable if the verification sat inline. Hand a `KeyState` in
/// instead and every one of those is exercisable. Same reasoning that made
/// [`clamp_to_signed_ceiling`] standalone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyState {
    /// No key at [`SELF_LICENSE_PATH`] and no `KEYSAT_LICENSE`.
    Absent,
    /// A key is present but is not a valid Keysat license: it does not parse,
    /// or its signature does not verify against the embedded trust root.
    /// Reported under one code because the remedy is identical — install a
    /// good key — and `reason` carries which of the two it was.
    Invalid { reason: String },
    /// Present, parsed, and signature-verified.
    Verified {
        /// The signed payload's own field: unix seconds, `0` meaning perpetual.
        expires_at: i64,
        /// The signed payload's `license_id`. [`classify`] does not read it —
        /// it is what [`observe_with`] looks the local `licenses` row up by,
        /// and carrying it here is what lets a test drive that lookup.
        license_id: uuid::Uuid,
    },
}

/// Severity of a [`SelfLicenseVerdict`]. Ordered, and derived from the code
/// rather than stored beside it, so "unlicensed never alerts" is one fact in
/// one place instead of an invariant two constructors have to agree on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SelfLicenseSeverity {
    Ok,
    Warn,
    Critical,
}

/// The eight mutually-exclusive things the daemon can say about its own
/// license. Evaluated in [`classify`] in the order listed there, first match
/// wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfLicenseCode {
    /// No key installed. Informational: running the free Creator tier is a
    /// legitimate configuration, not a fault.
    Unlicensed,
    SignatureInvalid,
    Revoked,
    Suspended,
    Expired,
    ExpiringSoon,
    /// A good key is installed and unexpired, yet the running daemon is still
    /// applying the free tier.
    StaleTier,
    Ok,
}

impl SelfLicenseCode {
    /// Wire string. Stable — the admin card and any future StartOS health
    /// check branch on it.
    pub fn as_str(self) -> &'static str {
        match self {
            SelfLicenseCode::Unlicensed => "unlicensed",
            SelfLicenseCode::SignatureInvalid => "signature_invalid",
            SelfLicenseCode::Revoked => "revoked",
            SelfLicenseCode::Suspended => "suspended",
            SelfLicenseCode::Expired => "expired",
            SelfLicenseCode::ExpiringSoon => "expiring_soon",
            SelfLicenseCode::StaleTier => "stale_tier",
            SelfLicenseCode::Ok => "ok",
        }
    }

    pub fn severity(self) -> SelfLicenseSeverity {
        match self {
            // Deliberately `Ok`: an operator who never bought Keysat is not
            // having an incident. The code is what lets a renderer say
            // "Creator (free) tier" instead of a green tick.
            SelfLicenseCode::Unlicensed | SelfLicenseCode::Ok => SelfLicenseSeverity::Ok,
            SelfLicenseCode::ExpiringSoon | SelfLicenseCode::StaleTier => {
                SelfLicenseSeverity::Warn
            }
            SelfLicenseCode::SignatureInvalid
            | SelfLicenseCode::Revoked
            | SelfLicenseCode::Suspended
            | SelfLicenseCode::Expired => SelfLicenseSeverity::Critical,
        }
    }
}

/// One verdict on the daemon's own license.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfLicenseVerdict {
    pub code: SelfLicenseCode,
    /// **Effective** expiry in unix seconds — the earlier of the signed key's
    /// own expiry and the local `licenses` row's. `None` means perpetual, or
    /// that there is no verified key to have an expiry.
    pub effective_expiry: Option<i64>,
    /// Whether a local `licenses` row was found for this key's `license_id`.
    /// `false` is normal, not a fault: a daemon licensed by someone else's
    /// Keysat holds only the on-disk key.
    pub row_present: bool,
    /// Why, when there is a why: the verification error, or the row's
    /// revocation/suspension timestamp.
    pub detail: Option<String>,
}

impl SelfLicenseVerdict {
    fn new(code: SelfLicenseCode) -> Self {
        SelfLicenseVerdict {
            code,
            effective_expiry: None,
            row_present: false,
            detail: None,
        }
    }

    pub fn severity(&self) -> SelfLicenseSeverity {
        self.code.severity()
    }
}

/// The `licenses` row's expiry as unix seconds, or `None` for perpetual.
///
/// Two units meet here and the difference is easy to miss: the signed
/// payload's `expires_at` is an `i64` with `0` = perpetual
/// (`crypto::LicensePayload`), while `licenses.expires_at` is RFC-3339 **TEXT**
/// with NULL = perpetual (migration 0003). This is deliberately the same idiom
/// `refresh_self_tier_from_db` uses — including that an unparseable string
/// reads as perpetual rather than as an error — so the health summary and the
/// tier logic can never disagree about what a row says.
fn row_expiry_unix(row: &License) -> Option<i64> {
    row.expires_at
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.timestamp())
        .filter(|&t| t != 0)
}

/// The earlier of the key's expiry and the row's; `None` if both are perpetual.
///
/// The row's expiry is **unclamped** and `refresh_self_tier_from_db` never
/// looks at it, so an issuer who shortened a license by editing the row is
/// invisible to anything that reads the key alone. Every expiry judgement
/// below goes through here for that reason.
fn effective_expiry(key_expires_at: i64, row: Option<&License>) -> Option<i64> {
    [
        (key_expires_at != 0).then_some(key_expires_at),
        row.and_then(row_expiry_unix),
    ]
    .into_iter()
    .flatten()
    .min()
}

/// A timestamp plus the issuer's reason, when they left one.
///
/// The reason is the part an operator can act on — "card expired", "chargeback",
/// "moved to annual" are three very different next steps and the stamp alone
/// distinguishes none of them.
fn stamped(at: &str, reason: Option<&str>) -> String {
    match reason.map(str::trim).filter(|r| !r.is_empty()) {
        Some(reason) => format!("{at} — {reason}"),
        None => at.to_string(),
    }
}

/// Judge the daemon's own license from three already-gathered observations.
///
/// Pure, and that is the point — see [`KeyState`]. The plan sketched this as
/// taking `key_expiry: Option<i64>`; it takes a [`KeyState`] instead, because
/// `Option<i64>` cannot express the difference between "no key" and "a key
/// that does not verify", which are the first two rows of the table and the
/// two furthest apart in severity.
///
/// Evaluated in this order, first match wins:
///
/// | Observation | Verdict |
/// |---|---|
/// | No key | `unlicensed` (info, never alerts) |
/// | Key present, does not verify | `critical: signature_invalid` |
/// | Row has `revoked_at` | `critical: revoked` |
/// | Row has `suspended_at` | `critical: suspended` |
/// | **Effective** expiry in the past | `critical: expired` |
/// | **Effective** expiry within 14 days | `warn: expiring_soon` |
/// | Running tier is `Unlicensed` (**row-independent**) | `warn: stale_tier` |
/// | Otherwise, including a missing row and a perpetual key | `ok` |
///
/// Three orderings in that table are load-bearing:
///
/// 1. **Expiry is judged before the tier is read.** An expired license demotes
///    the running tier to `Unlicensed`, exactly as a revoked or suspended one
///    does, so a check that asked `self_tier` first would report an expired
///    master license as merely "stale" — or, if it trusted the tier alone, as
///    healthy.
/// 2. **`stale_tier` sits above the missing-row case and never consults the
///    row.** Phrased as "the row is fine" it would collide with "there is no
///    row", and a daemon holding only an on-disk key is the normal shape for
///    every downstream customer install — so that phrasing would swallow the
///    warning for the entire population it exists to serve.
/// 3. **Both expiry tests use the effective expiry**, never the key's own. The
///    row may shorten a license; the key cannot see that.
pub fn classify(
    key: &KeyState,
    row: Option<&License>,
    tier: &Tier,
    now: i64,
) -> SelfLicenseVerdict {
    let key_expires_at = match key {
        KeyState::Absent => {
            return SelfLicenseVerdict {
                // Never an alert, but the detail must not claim a tier the
                // daemon may not be running: `refresh_self_tier_from_db` KEEPS
                // a `Licensed` tier when the key source disappears (offline
                // grace, with its own warn log), so "no key" and "free tier"
                // are not the same statement. Asserting the second from the
                // first would put this card in direct contradiction with
                // `GET /v1/admin/self-license`.
                detail: Some(match tier {
                    Tier::Unlicensed { reason } => reason.clone(),
                    Tier::Licensed { .. } => "the daemon is still applying a license loaded \
                         earlier; with the key source gone it will fall back to the free \
                         Creator tier when it next restarts"
                        .to_string(),
                }),
                row_present: row.is_some(),
                ..SelfLicenseVerdict::new(SelfLicenseCode::Unlicensed)
            };
        }
        KeyState::Invalid { reason } => {
            return SelfLicenseVerdict {
                detail: Some(reason.clone()),
                row_present: row.is_some(),
                ..SelfLicenseVerdict::new(SelfLicenseCode::SignatureInvalid)
            }
        }
        KeyState::Verified { expires_at, .. } => *expires_at,
    };

    let expiry = effective_expiry(key_expires_at, row);
    let base = SelfLicenseVerdict {
        effective_expiry: expiry,
        row_present: row.is_some(),
        ..SelfLicenseVerdict::new(SelfLicenseCode::Ok)
    };

    if let Some(row) = row {
        // `.is_some()` on both, matching `refresh_self_tier_from_db` exactly:
        // the health summary must classify a row the same way the tier logic
        // acts on it, or the card contradicts the daemon's own behavior.
        if let Some(at) = &row.revoked_at {
            return SelfLicenseVerdict {
                code: SelfLicenseCode::Revoked,
                detail: Some(stamped(at, row.revocation_reason.as_deref())),
                ..base
            };
        }
        if let Some(at) = &row.suspended_at {
            return SelfLicenseVerdict {
                code: SelfLicenseCode::Suspended,
                detail: Some(stamped(at, row.suspension_reason.as_deref())),
                ..base
            };
        }
    }

    if let Some(expiry) = expiry {
        // `>=`, matching `LicensePayload::is_expired_at`.
        if now >= expiry {
            return SelfLicenseVerdict {
                code: SelfLicenseCode::Expired,
                ..base
            };
        }
        if expiry.saturating_sub(now) <= EXPIRING_SOON_SECONDS {
            return SelfLicenseVerdict {
                code: SelfLicenseCode::ExpiringSoon,
                ..base
            };
        }
    }

    // Row-independent and deliberately last of the unhappy tests: every case
    // above also demotes the running tier, so asking the tier first would
    // report an expired, revoked or suspended license as merely stale.
    if let Tier::Unlicensed { reason } = tier {
        return SelfLicenseVerdict {
            code: SelfLicenseCode::StaleTier,
            detail: Some(reason.clone()),
            ..base
        };
    }

    base
}

/// Read the self-license key and say what it is. The one step no test can
/// reach past — see [`KeyState`].
fn read_key_state() -> KeyState {
    match read_license_string() {
        None => KeyState::Absent,
        Some(s) => match verify_key_signature(&s) {
            Ok(payload) => KeyState::Verified {
                expires_at: payload.expires_at,
                license_id: payload.license_id,
            },
            Err(e) => KeyState::Invalid {
                reason: format!("{e:#}"),
            },
        },
    }
}

/// Gather the three observations [`classify`] needs and judge them.
///
/// Takes the key state rather than reading it, which is what makes the row
/// lookup, the clock and the tier plumbing testable: hand in a
/// [`KeyState::Verified`] naming a row a test has inserted and every branch
/// below this line runs for real, against the real `get_license_by_id`. With
/// the verification inlined here, none of it could be driven at all without the
/// master private key.
///
/// The row is looked up by the **key's** `license_id` rather than the running
/// tier's, so a daemon whose tier has already demoted still gets its row read.
pub async fn observe_with(
    key: KeyState,
    pool: &sqlx::SqlitePool,
    tier: &Tier,
    now: i64,
) -> AppResult<SelfLicenseVerdict> {
    // Only a verified key names a row worth reading; the other two verdicts
    // short-circuit before the row is consulted anyway. A DB failure
    // propagates rather than reading as "no row" — "no row" is reported as
    // healthy (offline grace), so swallowing the error here would turn a broken
    // database into a green card.
    let row = match &key {
        KeyState::Verified { license_id, .. } => {
            crate::db::repo::get_license_by_id(pool, &license_id.to_string()).await?
        }
        KeyState::Absent | KeyState::Invalid { .. } => None,
    };

    Ok(classify(&key, row.as_ref(), tier, now))
}

/// [`observe_with`] against the key this daemon actually has. The endpoint's
/// entry point.
pub async fn observe_self_license(
    pool: &sqlx::SqlitePool,
    tier: &Tier,
    now: i64,
) -> AppResult<SelfLicenseVerdict> {
    observe_with(read_key_state(), pool, tier, now).await
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

    #[test]
    fn partial_downgrade_keeps_the_still_granted_entitlements() {
        // Multi-entitlement signed key; the DB row drops one of them
        // (an issuer-applied partial downgrade) and keeps the rest.
        let signed = v(&["unlimited_products", "recurring_billing", "zaprite_payments"]);
        let db = v(&["unlimited_products", "zaprite_payments"]);
        assert_eq!(
            clamp_to_signed_ceiling(db, &signed),
            v(&["unlimited_products", "zaprite_payments"])
        );
    }

    // -----------------------------------------------------------------
    // Self-license health: every row of `classify`'s decision table.
    //
    // All eight are exercised here rather than through the endpoint
    // because six of them need a key signed by the master private half,
    // which exists on exactly one machine and is not in this repo.
    // `KeyState` is what makes them reachable.
    // -----------------------------------------------------------------

    const NOW: i64 = 1_800_000_000; // 2027-01-15T08:00:00Z, an arbitrary fixed clock
    const DAY: i64 = 24 * 60 * 60;

    /// A `licenses` row in the shape the daemon's own license has: active,
    /// perpetual, never touched by the issuer.
    fn healthy_row() -> License {
        License {
            id: "0f9e8d7c-6b5a-4938-8271-605f4e3d2c1b".into(),
            product_id: "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d".into(),
            invoice_id: None,
            status: "active".into(),
            fingerprint: None,
            bound_identity: None,
            issued_at: "2026-01-01T00:00:00+00:00".into(),
            revoked_at: None,
            revocation_reason: None,
            metadata: serde_json::Value::Null,
            policy_id: None,
            expires_at: None,
            grace_seconds: 0,
            max_machines: 1,
            suspended_at: None,
            suspension_reason: None,
            entitlements: v(&["unlimited_products"]),
            is_trial: false,
            nostr_npub: None,
            buyer_email: None,
        }
    }

    /// RFC-3339 TEXT, the way the `licenses` table stores an expiry — the
    /// conversion `classify` has to get right.
    fn rfc3339(unix: i64) -> String {
        chrono::DateTime::from_timestamp(unix, 0)
            .expect("in-range timestamp")
            .to_rfc3339()
    }

    fn verified(expires_at: i64) -> KeyState {
        KeyState::Verified {
            expires_at,
            license_id: uuid::Uuid::nil(),
        }
    }

    /// A running tier that is applying a license — the healthy shape.
    fn licensed_tier() -> Tier {
        Tier::Licensed {
            license_id: uuid::Uuid::nil(),
            product_id: uuid::Uuid::nil(),
            expires_at: 0,
            entitlements: v(&["unlimited_products"]),
        }
    }

    fn creator_tier() -> Tier {
        Tier::Unlicensed {
            reason: "running the free tier".into(),
        }
    }

    // --- Row 1: no key. ---

    #[test]
    fn no_key_reports_unlicensed_and_never_alerts() {
        // The free Creator tier is a legitimate configuration and the most
        // common one downstream. Reporting it as a fault would train every
        // operator who never bought Keysat to ignore this card.
        let verdict = classify(&KeyState::Absent, None, &creator_tier(), NOW);
        assert_eq!(verdict.code, SelfLicenseCode::Unlicensed);
        assert_eq!(verdict.severity(), SelfLicenseSeverity::Ok);
        assert_eq!(verdict.effective_expiry, None);
        assert!(!verdict.row_present);
    }

    // --- Row 2: present but does not verify. ---

    #[test]
    fn an_unverifiable_key_is_critical_signature_invalid() {
        let key = KeyState::Invalid {
            reason: "license key parse failed: bad prefix".into(),
        };
        let verdict = classify(&key, None, &creator_tier(), NOW);
        assert_eq!(verdict.code, SelfLicenseCode::SignatureInvalid);
        assert_eq!(verdict.severity(), SelfLicenseSeverity::Critical);
        assert_eq!(
            verdict.detail.as_deref(),
            Some("license key parse failed: bad prefix"),
            "the operator needs to know which way it failed"
        );
    }

    #[test]
    fn signature_invalid_outranks_everything_the_row_could_say() {
        // A key that does not verify is not this daemon's license at all, so
        // whatever a row with the same id says about revocation is beside the
        // point. (The IO wrapper does not even look the row up in this case;
        // the ordering is pinned here so it stays true if that changes.)
        let mut row = healthy_row();
        row.revoked_at = Some("2027-01-01T00:00:00+00:00".into());
        let key = KeyState::Invalid {
            reason: "signature does not verify".into(),
        };
        let verdict = classify(&key, Some(&row), &creator_tier(), NOW);
        assert_eq!(verdict.code, SelfLicenseCode::SignatureInvalid);
    }

    // --- Rows 3 and 4: the issuer revoked or suspended it. ---

    #[test]
    fn a_revoked_row_is_critical() {
        let mut row = healthy_row();
        row.revoked_at = Some("2027-01-10T09:00:00+00:00".into());
        let verdict = classify(&verified(0), Some(&row), &licensed_tier(), NOW);
        assert_eq!(verdict.code, SelfLicenseCode::Revoked);
        assert_eq!(verdict.severity(), SelfLicenseSeverity::Critical);
        assert_eq!(verdict.detail.as_deref(), Some("2027-01-10T09:00:00+00:00"));
        assert!(verdict.row_present);
    }

    #[test]
    fn a_suspended_row_is_critical() {
        let mut row = healthy_row();
        row.suspended_at = Some("2027-01-10T09:00:00+00:00".into());
        let verdict = classify(&verified(0), Some(&row), &licensed_tier(), NOW);
        assert_eq!(verdict.code, SelfLicenseCode::Suspended);
        assert_eq!(verdict.severity(), SelfLicenseSeverity::Critical);
        assert_eq!(verdict.detail.as_deref(), Some("2027-01-10T09:00:00+00:00"));
    }

    #[test]
    fn revocation_and_suspension_outrank_expiry() {
        // Both states demote the tier the same way expiry does, and all three
        // can be true at once. Revocation is the one the operator has to act
        // on first, so it is the one reported.
        let mut row = healthy_row();
        row.revoked_at = Some("2027-01-10T09:00:00+00:00".into());
        row.suspended_at = Some("2027-01-09T09:00:00+00:00".into());
        let expired_key = verified(NOW - DAY);
        assert_eq!(
            classify(&expired_key, Some(&row), &creator_tier(), NOW).code,
            SelfLicenseCode::Revoked
        );

        row.revoked_at = None;
        assert_eq!(
            classify(&expired_key, Some(&row), &creator_tier(), NOW).code,
            SelfLicenseCode::Suspended
        );
    }

    // --- Row 5: expired. ---

    #[test]
    fn an_expired_key_is_critical_expired() {
        let verdict = classify(&verified(NOW - DAY), None, &creator_tier(), NOW);
        assert_eq!(verdict.code, SelfLicenseCode::Expired);
        assert_eq!(verdict.severity(), SelfLicenseSeverity::Critical);
        assert_eq!(verdict.effective_expiry, Some(NOW - DAY));
    }

    #[test]
    fn an_expired_key_reports_expired_rather_than_stale_tier() {
        // The trap this ordering exists for: an expired license demotes the
        // running tier to Unlicensed, so an expired daemon and a daemon that
        // simply has not picked its key up look identical from `self_tier`.
        // Reading the tier first would report the lapsed subscription as a
        // restart-fixable annoyance.
        let verdict = classify(&verified(NOW - DAY), None, &creator_tier(), NOW);
        assert_eq!(verdict.code, SelfLicenseCode::Expired);
    }

    #[test]
    fn expiry_exactly_now_counts_as_expired() {
        // Same boundary as `LicensePayload::is_expired_at`, which the daemon
        // enforces with `now >= expires_at`. A card that said "expiring soon"
        // at the instant the daemon stopped honoring the license would be
        // contradicting the daemon.
        assert_eq!(
            classify(&verified(NOW), None, &licensed_tier(), NOW).code,
            SelfLicenseCode::Expired
        );
    }

    // --- Row 6: expiring soon. ---

    #[test]
    fn an_expiry_inside_fourteen_days_warns() {
        let verdict = classify(&verified(NOW + 3 * DAY), None, &licensed_tier(), NOW);
        assert_eq!(verdict.code, SelfLicenseCode::ExpiringSoon);
        assert_eq!(verdict.severity(), SelfLicenseSeverity::Warn);
        assert_eq!(verdict.effective_expiry, Some(NOW + 3 * DAY));
    }

    #[test]
    fn the_fourteen_day_boundary_is_inclusive_and_fifteen_days_is_quiet() {
        assert_eq!(
            classify(&verified(NOW + 14 * DAY), None, &licensed_tier(), NOW).code,
            SelfLicenseCode::ExpiringSoon
        );
        assert_eq!(
            classify(&verified(NOW + 15 * DAY), None, &licensed_tier(), NOW).code,
            SelfLicenseCode::Ok
        );
    }

    // --- Row 7: the key is fine but the daemon is not applying it. ---

    #[test]
    fn a_verified_key_with_an_unlicensed_running_tier_warns_stale_tier() {
        let verdict = classify(&verified(0), Some(&healthy_row()), &creator_tier(), NOW);
        assert_eq!(verdict.code, SelfLicenseCode::StaleTier);
        assert_eq!(verdict.severity(), SelfLicenseSeverity::Warn);
        assert_eq!(
            verdict.detail.as_deref(),
            Some("running the free tier"),
            "the tier's own reason string is the most useful detail here"
        );
    }

    #[test]
    fn stale_tier_is_row_independent_and_outranks_the_missing_row_case() {
        // The precedence that matters most. A daemon licensed by someone
        // else's Keysat holds only the on-disk key and legitimately has no
        // local row — which is every downstream customer. If the missing-row
        // case were checked first, or if `stale_tier` required a healthy row,
        // this warning would be swallowed for that entire population.
        let verdict = classify(&verified(0), None, &creator_tier(), NOW);
        assert_eq!(verdict.code, SelfLicenseCode::StaleTier);
        assert!(!verdict.row_present);
    }

    #[test]
    fn expiring_soon_outranks_stale_tier() {
        // Both are warnings, so the order decides only which remedy the
        // operator is given — and renewing beats restarting.
        let verdict = classify(&verified(NOW + DAY), None, &creator_tier(), NOW);
        assert_eq!(verdict.code, SelfLicenseCode::ExpiringSoon);
    }

    // --- Rows 8 and 9: healthy. ---

    #[test]
    fn a_verified_key_with_no_local_row_is_ok_offline_grace() {
        // Documented normal shape for a daemon issued elsewhere. `row_present`
        // is what lets a renderer say so instead of implying the row is lost.
        let verdict = classify(&verified(NOW + 365 * DAY), None, &licensed_tier(), NOW);
        assert_eq!(verdict.code, SelfLicenseCode::Ok);
        assert_eq!(verdict.severity(), SelfLicenseSeverity::Ok);
        assert!(!verdict.row_present);
        assert_eq!(verdict.effective_expiry, Some(NOW + 365 * DAY));
    }

    #[test]
    fn a_perpetual_key_with_a_healthy_row_is_ok() {
        let verdict = classify(&verified(0), Some(&healthy_row()), &licensed_tier(), NOW);
        assert_eq!(verdict.code, SelfLicenseCode::Ok);
        assert_eq!(
            verdict.effective_expiry, None,
            "perpetual is null, not epoch zero"
        );
        assert!(verdict.row_present);
    }

    // --- Effective expiry: the unit conversion and the min(). ---

    #[test]
    fn a_row_shortened_expiry_already_past_is_expired_not_expiring_soon() {
        // The whole reason expiry is effective rather than key-only. The
        // issuer shortened this license by editing the row; the signed key
        // still says it runs for another year, and `refresh_self_tier_from_db`
        // never looks at the row's expiry at all. Reading the key alone
        // reports a dead license as healthy.
        let mut row = healthy_row();
        row.expires_at = Some(rfc3339(NOW - 2 * DAY));
        let verdict = classify(&verified(NOW + 365 * DAY), Some(&row), &creator_tier(), NOW);
        assert_eq!(verdict.code, SelfLicenseCode::Expired);
        assert_eq!(verdict.effective_expiry, Some(NOW - 2 * DAY));
    }

    #[test]
    fn the_earlier_of_key_and_row_expiry_wins_in_both_directions() {
        // Key sooner than row.
        let mut row = healthy_row();
        row.expires_at = Some(rfc3339(NOW + 60 * DAY));
        let verdict = classify(&verified(NOW + 3 * DAY), Some(&row), &licensed_tier(), NOW);
        assert_eq!(verdict.code, SelfLicenseCode::ExpiringSoon);
        assert_eq!(verdict.effective_expiry, Some(NOW + 3 * DAY));

        // Row sooner than key.
        row.expires_at = Some(rfc3339(NOW + 3 * DAY));
        let verdict = classify(&verified(NOW + 60 * DAY), Some(&row), &licensed_tier(), NOW);
        assert_eq!(verdict.code, SelfLicenseCode::ExpiringSoon);
        assert_eq!(verdict.effective_expiry, Some(NOW + 3 * DAY));
    }

    #[test]
    fn a_perpetual_key_still_takes_an_expiry_from_the_row() {
        // `0` on the key means perpetual, not "expires at the epoch" — if the
        // units were confused, this would read as long expired.
        let mut row = healthy_row();
        row.expires_at = Some(rfc3339(NOW + 3 * DAY));
        let verdict = classify(&verified(0), Some(&row), &licensed_tier(), NOW);
        assert_eq!(verdict.code, SelfLicenseCode::ExpiringSoon);
        assert_eq!(verdict.effective_expiry, Some(NOW + 3 * DAY));
    }

    #[test]
    fn a_null_row_expiry_means_perpetual_and_leaves_the_key_authoritative() {
        // NULL in TEXT is the row's perpetual, the mirror of `0` on the key.
        let row = healthy_row();
        assert_eq!(row.expires_at, None);
        let verdict = classify(&verified(NOW + 3 * DAY), Some(&row), &licensed_tier(), NOW);
        assert_eq!(verdict.effective_expiry, Some(NOW + 3 * DAY));
    }

    #[test]
    fn an_epoch_zero_row_expiry_reads_as_perpetual() {
        // The two perpetual spellings meet here: `0` on the signed key and
        // NULL in the row. A row that literally stores the epoch is treated as
        // the former rather than as an expiry 57 years in the past, which is
        // what `refresh_self_tier_from_db`'s `unwrap_or(0)` also means by it.
        let mut row = healthy_row();
        row.expires_at = Some(rfc3339(0));
        let verdict = classify(&verified(0), Some(&row), &licensed_tier(), NOW);
        assert_eq!(verdict.code, SelfLicenseCode::Ok);
        assert_eq!(verdict.effective_expiry, None);
    }

    #[test]
    fn an_unparseable_row_expiry_falls_back_to_the_key() {
        // Same as `refresh_self_tier_from_db` treats it, deliberately: if the
        // two disagreed about a garbled row, the card would contradict the
        // tier the daemon is actually running.
        let mut row = healthy_row();
        row.expires_at = Some("not a timestamp".into());
        let verdict = classify(&verified(NOW + 3 * DAY), Some(&row), &licensed_tier(), NOW);
        assert_eq!(verdict.code, SelfLicenseCode::ExpiringSoon);
        assert_eq!(verdict.effective_expiry, Some(NOW + 3 * DAY));
    }

    // --- The decomposed verifier. ---

    #[test]
    fn a_well_formed_key_signed_by_the_wrong_keypair_does_not_verify() {
        // The half of `verify_license` that had no test at all: nothing in
        // `tests/` ever built a key and put it through the trust root, so
        // deleting the signature check would have gone unnoticed. This mints a
        // structurally perfect LIC1 key with a keypair that is not the master
        // and requires it to be rejected — the property the whole
        // self-licensing scheme rests on.
        use ed25519_dalek::SigningKey;
        let impostor = SigningKey::from_bytes(&[7u8; 32]);
        let payload = crate::crypto::LicensePayload {
            version: 2,
            flags: 0,
            product_id: uuid::Uuid::nil(),
            license_id: uuid::Uuid::nil(),
            issued_at: NOW,
            expires_at: 0,
            fingerprint_hash: [0u8; 32],
            entitlements: v(&["unlimited_products"]),
        };
        let signature = crate::crypto::sign_payload(&impostor, &payload);
        let key = crate::crypto::encode_key(&payload, &signature);

        // It parses — so this really does reach the signature check.
        crate::crypto::parse_key(&key).expect("the impostor key is well-formed");

        let err = verify_key_signature(&key)
            .expect_err("a key not signed by the master private half must not verify");
        assert!(
            format!("{err:#}").contains("does not verify against master pubkey"),
            "got {err:#}"
        );
    }

    #[test]
    fn a_malformed_key_fails_at_parse_rather_than_at_verification() {
        let err = verify_key_signature("LIC1-not-a-real-key-at-all")
            .expect_err("garbage must not verify");
        assert!(format!("{err:#}").contains("parse failed"), "got {err:#}");
    }

    #[test]
    fn verify_license_checks_the_signature_before_the_expiry() {
        // The order the split preserved: `verify_license` is now written in
        // terms of `verify_key_signature` and adds the expiry bail after it.
        // A key that is both expired and wrongly signed must be rejected as
        // wrongly signed — "your license expired" would be a confusing thing
        // to tell someone holding a forgery.
        //
        // The expiry bail itself cannot be exercised here: reaching it needs a
        // key signed by the master private half, which exists on one machine
        // and not in this repo. That is what `classify` and `KeyState` are for.
        use ed25519_dalek::SigningKey;
        let impostor = SigningKey::from_bytes(&[9u8; 32]);
        let payload = crate::crypto::LicensePayload {
            version: 2,
            flags: 0,
            product_id: uuid::Uuid::nil(),
            license_id: uuid::Uuid::nil(),
            issued_at: 1,
            expires_at: 2,
            fingerprint_hash: [0u8; 32],
            entitlements: Vec::new(),
        };
        let signature = crate::crypto::sign_payload(&impostor, &payload);
        let key = crate::crypto::encode_key(&payload, &signature);
        let err = verify_license(&key).expect_err("expired AND wrongly signed");
        assert!(
            format!("{err:#}").contains("does not verify against master pubkey"),
            "the signature is checked before the expiry; got {err:#}"
        );
    }

    // --- The wire strings the admin card and StartOS health check bind to. ---

    #[test]
    fn every_code_has_its_own_wire_string() {
        let codes = [
            (SelfLicenseCode::Unlicensed, "unlicensed"),
            (SelfLicenseCode::SignatureInvalid, "signature_invalid"),
            (SelfLicenseCode::Revoked, "revoked"),
            (SelfLicenseCode::Suspended, "suspended"),
            (SelfLicenseCode::Expired, "expired"),
            (SelfLicenseCode::ExpiringSoon, "expiring_soon"),
            (SelfLicenseCode::StaleTier, "stale_tier"),
            (SelfLicenseCode::Ok, "ok"),
        ];
        for (code, wire) in codes {
            assert_eq!(code.as_str(), wire);
        }
        let distinct: std::collections::HashSet<_> =
            codes.iter().map(|(_, wire)| *wire).collect();
        assert_eq!(distinct.len(), codes.len(), "two codes share a wire string");
    }
}
