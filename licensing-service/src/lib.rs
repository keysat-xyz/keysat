//! Keysat library — the daemon's internal modules, exposed as a library
//! so integration tests under `tests/` can drive the API directly without
//! re-implementing the bootstrap.
//!
//! The binary at `src/main.rs` is a thin wrapper that loads runtime config
//! from environment variables, starts the HTTP server, and spawns
//! background tasks. Tests bypass that wrapper and construct `AppState`
//! programmatically.

pub mod analytics;
pub mod api;
pub mod btcpay;
pub mod config;
pub mod crypto;
pub mod db;
pub mod error;
pub mod license_self;
pub mod models;
pub mod payment;
pub mod rate_limit;
pub mod rates;
pub mod reconcile;
pub mod tipping;
pub mod webhooks;

/// Hex-encoded SHA-256 of a string — used everywhere we need a deterministic
/// id from a raw value (machine fingerprints, admin key hashes).
pub fn hex_sha256(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}
