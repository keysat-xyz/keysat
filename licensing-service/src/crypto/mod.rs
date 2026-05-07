//! License key cryptography.
//!
//! # Key format
//!
//! A license key presented to users looks like:
//!
//! ```text
//! LIC1-<base32 payload>-<base32 signature>
//! ```
//!
//! The base32 alphabet is `BASE32_NOPAD` (RFC 4648, no padding, case-insensitive
//! decode). Signatures are always 64 bytes of Ed25519.
//!
//! ## Payload — version 1 (legacy, still accepted)
//!
//! A fixed 74-byte blob:
//!
//! | offset | size | field                                        |
//! |--------|------|----------------------------------------------|
//! | 0      | 1    | version = 1                                  |
//! | 1      | 1    | flags (bit 0: fingerprint-bound)             |
//! | 2      | 16   | product_id (UUID, big-endian bytes)          |
//! | 18     | 16   | license_id (UUID, big-endian bytes)          |
//! | 34     | 8    | issued_at (u64 unix seconds, BE)             |
//! | 42     | 32   | fingerprint_hash (SHA-256, zero if unbound)  |
//!
//! ## Payload — version 2 (current default)
//!
//! Variable-length. The fixed head is 83 bytes, followed by the entitlements
//! table. Every byte here is signed.
//!
//! | offset | size | field                                                   |
//! |--------|------|---------------------------------------------------------|
//! | 0      | 1    | version = 2                                             |
//! | 1      | 1    | flags                                                   |
//! | 2      | 16   | product_id                                              |
//! | 18     | 16   | license_id                                              |
//! | 34     | 8    | issued_at (u64 BE, unix seconds)                        |
//! | 42     | 8    | expires_at (u64 BE, unix seconds; 0 = perpetual)        |
//! | 50     | 32   | fingerprint_hash (SHA-256; zero iff flag bit unset)     |
//! | 82     | 1    | entitlements_count (N, 0..=255)                         |
//! | 83..   | ...  | entitlements: N × `<len: u8><ascii bytes>`              |
//!
//! Each entitlement is a short ASCII string ≤ 255 bytes; the canonical examples
//! are feature slugs (`"pro"`, `"cloud-sync"`, `"multi-seat"`). The list is
//! signed so offline verifiers can gate features without contacting the server.
//!
//! ## Flag bits (shared across versions)
//!
//! | bit | meaning                                                    |
//! |-----|------------------------------------------------------------|
//! | 0   | fingerprint-bound                                          |
//! | 1   | trial license (v2 only; best-effort — clients may warn)    |
//!
//! # Why versioned
//!
//! v2 adds expiry and entitlements, both of which need to be inside the signed
//! blob if we want offline enforcement (a stripped entitlement or pushed-back
//! expiry would have to match a valid signature, which the attacker can't
//! produce). Keeping the v1 parser in place means any keys already issued with
//! v1 continue to verify forever — the whole point of cryptographic licensing.
//!
//! # Offline verification
//!
//! Third-party clients ship the server's **public key** (not the private
//! key) bundled in their SDK. They can verify signatures, enforce expiry, and
//! gate features on entitlements entirely offline. Revocation, machine binding,
//! and suspension are authoritative server-side — clients that want true
//! strictness should call `/v1/validate` periodically.

pub mod keys;

use anyhow::{anyhow, Context, Result};
use data_encoding::BASE32_NOPAD;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Key format version currently issued by the server.
pub const KEY_VERSION: u8 = 2;

/// v1 format — legacy, still accepted on parse.
pub const KEY_VERSION_V1: u8 = 1;
/// v2 format — current default.
pub const KEY_VERSION_V2: u8 = 2;

/// Fixed-size of the v1 payload (for tests / legacy parsing).
pub const PAYLOAD_V1_LEN: usize = 1 + 1 + 16 + 16 + 8 + 32; // = 74

/// Minimum size of a v2 payload (head only, no entitlements).
pub const PAYLOAD_V2_HEAD_LEN: usize = 1 + 1 + 16 + 16 + 8 + 8 + 32 + 1; // = 83

/// Flag bit indicating the license is bound to a fingerprint hash.
pub const FLAG_FINGERPRINT_BOUND: u8 = 0b0000_0001;

/// Flag bit indicating the license was issued as a trial (comp/paid trial).
/// Clients that care may render a "Trial" badge; enforcement is via expiry.
pub const FLAG_TRIAL: u8 = 0b0000_0010;

/// Prefix that tags our key strings and future-proofs the envelope.
pub const KEY_PREFIX: &str = "LIC1";

/// Parsed, not-yet-verified key payload. This is a unified v1+v2 shape; on a
/// v1 parse we zero-fill the v2-only fields, so downstream code can be
/// version-agnostic as long as it reads `version` before trusting `expires_at`
/// or `entitlements`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LicensePayload {
    pub version: u8,
    pub flags: u8,
    pub product_id: Uuid,
    pub license_id: Uuid,
    pub issued_at: i64,
    /// Unix seconds; `0` means perpetual. Always 0 for v1.
    pub expires_at: i64,
    /// SHA-256 of the fingerprint, or zeros if `FLAG_FINGERPRINT_BOUND` is unset.
    pub fingerprint_hash: [u8; 32],
    /// Feature slugs ASCII; empty for v1 or v2 licenses with no entitlements.
    pub entitlements: Vec<String>,
}

impl LicensePayload {
    pub fn is_fingerprint_bound(&self) -> bool {
        self.flags & FLAG_FINGERPRINT_BOUND != 0
    }

    pub fn is_trial(&self) -> bool {
        self.flags & FLAG_TRIAL != 0
    }

    /// Has this license expired at the given instant? `expires_at == 0` means
    /// perpetual and returns `false`.
    pub fn is_expired_at(&self, now_unix: i64) -> bool {
        self.expires_at != 0 && now_unix >= self.expires_at
    }

    /// Does this license grant the given entitlement? Comparison is
    /// case-sensitive and exact — pick a canonical casing and stick with it.
    pub fn has_entitlement(&self, slug: &str) -> bool {
        self.entitlements.iter().any(|e| e == slug)
    }

    /// Serialize to the v2 wire format. Always emits v2 — v1 is parse-only.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(PAYLOAD_V2_HEAD_LEN + self.entitlements.len() * 16);
        buf.push(KEY_VERSION_V2);
        buf.push(self.flags);
        buf.extend_from_slice(self.product_id.as_bytes());
        buf.extend_from_slice(self.license_id.as_bytes());
        buf.extend_from_slice(&(self.issued_at as u64).to_be_bytes());
        buf.extend_from_slice(&(self.expires_at as u64).to_be_bytes());
        buf.extend_from_slice(&self.fingerprint_hash);
        // entitlement count — capped at 255 by u8
        let n: u8 = self
            .entitlements
            .len()
            .try_into()
            .expect("too many entitlements (max 255)");
        buf.push(n);
        for e in &self.entitlements {
            let bytes = e.as_bytes();
            let len: u8 = bytes
                .len()
                .try_into()
                .expect("entitlement slug too long (max 255 bytes)");
            buf.push(len);
            buf.extend_from_slice(bytes);
        }
        buf
    }

    /// Parse a payload blob. Dispatches on the first byte (version).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            return Err(anyhow!("empty payload"));
        }
        match bytes[0] {
            KEY_VERSION_V1 => Self::from_bytes_v1(bytes),
            KEY_VERSION_V2 => Self::from_bytes_v2(bytes),
            other => Err(anyhow!("unsupported key version: {other}")),
        }
    }

    fn from_bytes_v1(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != PAYLOAD_V1_LEN {
            return Err(anyhow!(
                "v1 payload length {} != expected {}",
                bytes.len(),
                PAYLOAD_V1_LEN
            ));
        }
        let flags = bytes[1];
        let product_id = Uuid::from_slice(&bytes[2..18])?;
        let license_id = Uuid::from_slice(&bytes[18..34])?;
        let issued_at = u64::from_be_bytes(bytes[34..42].try_into().unwrap()) as i64;
        let mut fingerprint_hash = [0u8; 32];
        fingerprint_hash.copy_from_slice(&bytes[42..74]);
        Ok(Self {
            version: KEY_VERSION_V1,
            flags,
            product_id,
            license_id,
            issued_at,
            expires_at: 0,
            fingerprint_hash,
            entitlements: Vec::new(),
        })
    }

    fn from_bytes_v2(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < PAYLOAD_V2_HEAD_LEN {
            return Err(anyhow!(
                "v2 payload length {} < head length {}",
                bytes.len(),
                PAYLOAD_V2_HEAD_LEN
            ));
        }
        let flags = bytes[1];
        let product_id = Uuid::from_slice(&bytes[2..18])?;
        let license_id = Uuid::from_slice(&bytes[18..34])?;
        let issued_at = u64::from_be_bytes(bytes[34..42].try_into().unwrap()) as i64;
        let expires_at = u64::from_be_bytes(bytes[42..50].try_into().unwrap()) as i64;
        let mut fingerprint_hash = [0u8; 32];
        fingerprint_hash.copy_from_slice(&bytes[50..82]);
        let n = bytes[82] as usize;

        let mut entitlements = Vec::with_capacity(n);
        let mut cursor = PAYLOAD_V2_HEAD_LEN;
        for i in 0..n {
            if cursor >= bytes.len() {
                return Err(anyhow!(
                    "truncated entitlement list at index {i} (cursor {cursor}, len {})",
                    bytes.len()
                ));
            }
            let len = bytes[cursor] as usize;
            cursor += 1;
            if cursor + len > bytes.len() {
                return Err(anyhow!(
                    "entitlement {i} length {len} runs past end of payload"
                ));
            }
            let slug = std::str::from_utf8(&bytes[cursor..cursor + len])
                .with_context(|| format!("entitlement {i} is not UTF-8"))?;
            entitlements.push(slug.to_string());
            cursor += len;
        }
        if cursor != bytes.len() {
            return Err(anyhow!(
                "trailing bytes after entitlement list ({} unread)",
                bytes.len() - cursor
            ));
        }
        Ok(Self {
            version: KEY_VERSION_V2,
            flags,
            product_id,
            license_id,
            issued_at,
            expires_at,
            fingerprint_hash,
            entitlements,
        })
    }
}

/// Hash a raw fingerprint string. We hash so that the full fingerprint never
/// travels inside the key (only its hash), making keys shorter and hiding
/// information like MAC addresses from anyone who intercepts a key string.
pub fn hash_fingerprint(fp: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(fp.as_bytes());
    hasher.finalize().into()
}

/// Encode a payload + signature into a user-facing key string.
pub fn encode_key(payload: &LicensePayload, signature: &Signature) -> String {
    let payload_b32 = BASE32_NOPAD.encode(&payload.to_bytes());
    let sig_b32 = BASE32_NOPAD.encode(&signature.to_bytes());
    format!("{KEY_PREFIX}-{payload_b32}-{sig_b32}")
}

/// Parse a user-provided key string into its payload + signature components
/// (plus the raw signed bytes, which the caller needs to verify against).
/// Does *not* verify the signature — call `verify_key` for that.
pub fn parse_key(s: &str) -> Result<(LicensePayload, Signature, Vec<u8>)> {
    let s = s.trim();
    let mut parts = s.splitn(3, '-');
    let prefix = parts.next().context("key is empty")?;
    if prefix != KEY_PREFIX {
        return Err(anyhow!("unrecognized key prefix: {prefix}"));
    }
    let payload_b32 = parts.next().context("missing payload section")?;
    let sig_b32 = parts.next().context("missing signature section")?;

    let payload_bytes = BASE32_NOPAD
        .decode(payload_b32.to_ascii_uppercase().as_bytes())
        .context("invalid base32 in payload")?;
    let sig_bytes = BASE32_NOPAD
        .decode(sig_b32.to_ascii_uppercase().as_bytes())
        .context("invalid base32 in signature")?;

    let payload = LicensePayload::from_bytes(&payload_bytes)?;
    let sig_array: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("signature length != 64"))?;
    let signature = Signature::from_bytes(&sig_array);
    Ok((payload, signature, payload_bytes))
}

/// Sign a payload with the server's private key.
pub fn sign_payload(signing_key: &SigningKey, payload: &LicensePayload) -> Signature {
    signing_key.sign(&payload.to_bytes())
}

/// Verify a parsed payload's signature against a public key.
///
/// For v2 keys, `signed_bytes` is the raw payload blob that was parsed from
/// the wire. For v1 keys it's the 74-byte v1 blob. Always pass the blob you
/// got out of `parse_key` directly — never re-serialize a `LicensePayload`,
/// because we always serialize as v2 and that will break v1 signatures.
pub fn verify_payload(
    verifying_key: &VerifyingKey,
    signed_bytes: &[u8],
    signature: &Signature,
) -> Result<()> {
    verifying_key
        .verify(signed_bytes, signature)
        .context("signature verification failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    fn test_payload() -> LicensePayload {
        LicensePayload {
            version: KEY_VERSION_V2,
            flags: 0,
            product_id: Uuid::new_v4(),
            license_id: Uuid::new_v4(),
            issued_at: 1_700_000_000,
            expires_at: 0,
            fingerprint_hash: [0u8; 32],
            entitlements: Vec::new(),
        }
    }

    #[test]
    fn roundtrip_unbound_perpetual_v2() {
        let signing = SigningKey::generate(&mut OsRng);
        let verifying = signing.verifying_key();

        let payload = test_payload();
        let sig = sign_payload(&signing, &payload);
        let encoded = encode_key(&payload, &sig);

        let (parsed, parsed_sig, signed_bytes) = parse_key(&encoded).unwrap();
        assert_eq!(parsed, payload);
        verify_payload(&verifying, &signed_bytes, &parsed_sig).unwrap();
    }

    #[test]
    fn roundtrip_with_entitlements_and_expiry() {
        let signing = SigningKey::generate(&mut OsRng);
        let verifying = signing.verifying_key();

        let payload = LicensePayload {
            expires_at: 1_900_000_000,
            entitlements: vec![
                "pro".to_string(),
                "cloud-sync".to_string(),
                "multi-seat".to_string(),
            ],
            ..test_payload()
        };
        let sig = sign_payload(&signing, &payload);
        let encoded = encode_key(&payload, &sig);

        let (parsed, parsed_sig, signed_bytes) = parse_key(&encoded).unwrap();
        assert_eq!(parsed, payload);
        assert!(parsed.has_entitlement("pro"));
        assert!(parsed.has_entitlement("cloud-sync"));
        assert!(!parsed.has_entitlement("enterprise"));
        assert!(!parsed.is_expired_at(1_800_000_000));
        assert!(parsed.is_expired_at(1_900_000_000));
        verify_payload(&verifying, &signed_bytes, &parsed_sig).unwrap();
    }

    #[test]
    fn tampered_payload_fails_verification() {
        let signing = SigningKey::generate(&mut OsRng);
        let verifying = signing.verifying_key();

        let payload = LicensePayload {
            entitlements: vec!["free".to_string()],
            ..test_payload()
        };
        let sig = sign_payload(&signing, &payload);
        let encoded = encode_key(&payload, &sig);
        let (_, parsed_sig, signed_bytes) = parse_key(&encoded).unwrap();

        // Flip a bit in the signed blob.
        let mut tampered = signed_bytes.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;

        assert!(verify_payload(&verifying, &tampered, &parsed_sig).is_err());
    }

    #[test]
    fn fingerprint_bound_roundtrip() {
        let signing = SigningKey::generate(&mut OsRng);
        let fp = "machine-abc-123";
        let payload = LicensePayload {
            flags: FLAG_FINGERPRINT_BOUND,
            fingerprint_hash: hash_fingerprint(fp),
            ..test_payload()
        };
        let sig = sign_payload(&signing, &payload);
        let encoded = encode_key(&payload, &sig);
        let (parsed, _, _) = parse_key(&encoded).unwrap();
        assert!(parsed.is_fingerprint_bound());
        assert_eq!(parsed.fingerprint_hash, hash_fingerprint(fp));
    }

    #[test]
    fn trial_flag_roundtrip() {
        let signing = SigningKey::generate(&mut OsRng);
        let payload = LicensePayload {
            flags: FLAG_TRIAL,
            expires_at: 1_710_000_000,
            ..test_payload()
        };
        let sig = sign_payload(&signing, &payload);
        let encoded = encode_key(&payload, &sig);
        let (parsed, _, _) = parse_key(&encoded).unwrap();
        assert!(parsed.is_trial());
    }

    #[test]
    fn v1_parse_still_works() {
        // Hand-craft a v1-shaped payload (the wire format that old service
        // versions emitted) and confirm we still parse it, zero-filling the
        // v2-only fields.
        let product_id = Uuid::new_v4();
        let license_id = Uuid::new_v4();
        let mut v1 = Vec::with_capacity(PAYLOAD_V1_LEN);
        v1.push(KEY_VERSION_V1);
        v1.push(FLAG_FINGERPRINT_BOUND);
        v1.extend_from_slice(product_id.as_bytes());
        v1.extend_from_slice(license_id.as_bytes());
        v1.extend_from_slice(&1_700_000_000u64.to_be_bytes());
        v1.extend_from_slice(&hash_fingerprint("rig-1"));
        assert_eq!(v1.len(), PAYLOAD_V1_LEN);

        let parsed = LicensePayload::from_bytes(&v1).unwrap();
        assert_eq!(parsed.version, KEY_VERSION_V1);
        assert!(parsed.is_fingerprint_bound());
        assert_eq!(parsed.expires_at, 0);
        assert!(parsed.entitlements.is_empty());
        assert_eq!(parsed.product_id, product_id);
        assert_eq!(parsed.license_id, license_id);
    }

    #[test]
    fn truncated_entitlement_list_is_rejected() {
        // v2 payload head claiming 2 entitlements but only 1 supplied.
        let mut buf = Vec::new();
        buf.push(KEY_VERSION_V2);
        buf.push(0);
        buf.extend_from_slice(&[0u8; 16]);
        buf.extend_from_slice(&[0u8; 16]);
        buf.extend_from_slice(&0u64.to_be_bytes()); // issued_at
        buf.extend_from_slice(&0u64.to_be_bytes()); // expires_at
        buf.extend_from_slice(&[0u8; 32]); // fingerprint
        buf.push(2); // count = 2
        buf.push(3); // len = 3
        buf.extend_from_slice(b"pro");
        // missing the second entitlement entirely
        assert!(LicensePayload::from_bytes(&buf).is_err());
    }
}
