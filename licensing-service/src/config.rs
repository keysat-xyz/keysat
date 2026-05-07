//! Runtime configuration.
//!
//! Loaded once at startup from environment variables. A `.env` file is read
//! if present (via `dotenvy`) so local development is frictionless. In
//! production on StartOS, the same variables are set by the service manifest.

use anyhow::{anyhow, Context, Result};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    /// Where the HTTP server binds.
    pub bind: SocketAddr,

    /// Path to the SQLite database file (e.g. `/data/keysat.db` inside a
    /// Start9 container; `./data/keysat.db` in dev).
    pub db_path: PathBuf,

    /// Shared secret required on admin endpoints via `Authorization: Bearer ...`.
    /// Generated once by the operator and kept secret.
    pub admin_api_key: String,

    /// BTCPay Server base URL used for daemon → BTCPay API calls. On
    /// StartOS this is the internal-network hostname like
    /// `http://btcpayserver.startos:23000`, which is only resolvable from
    /// inside other StartOS containers.
    pub btcpay_url: String,

    /// BTCPay Server base URL used for the OPERATOR'S BROWSER. The
    /// authorize flow redirects the operator's browser to BTCPay's
    /// consent page; that target must be reachable from the LAN /
    /// clearnet, not the internal-network hostname. The wrapper sets
    /// this to BTCPay's preferred operator-facing URL — typically
    /// mDNS (`https://immense-voyage.local:49347`) since the operator
    /// is on the same LAN as the Start9.
    pub btcpay_browser_url: Option<String>,

    /// BTCPay Server PUBLIC URL used for BUYER-facing redirects.
    /// The daemon rewrites checkout URLs returned by BTCPay's API so
    /// they point at this URL — random internet buyers can't reach
    /// mDNS or LAN URLs, so this needs to be a real clearnet domain
    /// like `https://btcpay.your-domain.com`. Falls back to
    /// `btcpay_browser_url` if unset (useful for local testing only).
    pub btcpay_public_url: Option<String>,

    /// Seed BTCPay API key, used only on first boot before the operator has
    /// completed the authorize flow. Leave empty in the normal case.
    pub btcpay_api_key: Option<String>,

    /// Seed BTCPay store id. Same rules as `btcpay_api_key` — empty in the
    /// normal case.
    pub btcpay_store_id: Option<String>,

    /// Seed webhook secret. Only used when bootstrapping from env vars.
    pub btcpay_webhook_secret: Option<String>,

    /// Public base URL of *this* Keysat instance, used when constructing
    /// invoice redirect / webhook URLs (e.g. `https://license.example.com`).
    pub public_base_url: String,

    /// Optional human-readable operator name shown in `/` index responses.
    pub operator_name: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        // Best-effort load of .env in dev. Missing file is not an error.
        let _ = dotenvy::dotenv();

        // All runtime knobs live under `KEYSAT_*`. For older installs and
        // dev shells that predate the rename we still honour the original
        // `LICENSING_*` names as a silent fallback.
        let bind_str = env_with_fallback("KEYSAT_BIND", "LICENSING_BIND")
            .unwrap_or_else(|| "0.0.0.0:8080".to_string());
        let bind: SocketAddr = bind_str
            .parse()
            .with_context(|| format!("KEYSAT_BIND is not a valid socket address: {bind_str}"))?;

        let db_path = PathBuf::from(
            env_with_fallback("KEYSAT_DB_PATH", "LICENSING_DB_PATH")
                .unwrap_or_else(|| "./data/keysat.db".into()),
        );

        let admin_api_key = required_with_fallback("KEYSAT_ADMIN_API_KEY", "LICENSING_ADMIN_API_KEY")?;
        if admin_api_key.len() < 32 {
            return Err(anyhow!(
                "KEYSAT_ADMIN_API_KEY must be at least 32 characters (use `openssl rand -hex 32`)"
            ));
        }

        let btcpay_url = required("BTCPAY_URL")?;
        let btcpay_browser_url = optional_nonempty("BTCPAY_BROWSER_URL")
            .map(|s| s.trim_end_matches('/').to_string());
        let btcpay_public_url = optional_nonempty("BTCPAY_PUBLIC_URL")
            .map(|s| s.trim_end_matches('/').to_string())
            // Fallback: if no public URL is plumbed, use browser URL.
            // Won't work for real customers but is fine for local testing.
            .or_else(|| btcpay_browser_url.clone());
        let btcpay_api_key = optional_nonempty("BTCPAY_API_KEY");
        let btcpay_store_id = optional_nonempty("BTCPAY_STORE_ID");
        let btcpay_webhook_secret = optional_nonempty("BTCPAY_WEBHOOK_SECRET");
        let public_base_url = required_with_fallback("KEYSAT_PUBLIC_URL", "LICENSING_PUBLIC_URL")?;
        let operator_name = env_with_fallback("KEYSAT_OPERATOR_NAME", "LICENSING_OPERATOR_NAME");

        Ok(Self {
            bind,
            db_path,
            admin_api_key,
            btcpay_url: btcpay_url.trim_end_matches('/').to_string(),
            btcpay_browser_url,
            btcpay_public_url,
            btcpay_api_key,
            btcpay_store_id,
            btcpay_webhook_secret,
            public_base_url: public_base_url.trim_end_matches('/').to_string(),
            operator_name,
        })
    }
}

fn optional_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn required(name: &str) -> Result<String> {
    std::env::var(name).map_err(|_| anyhow!("missing required env var: {name}"))
}

/// Look up a var under its current (KEYSAT_*) name, falling back to the
/// pre-rename (LICENSING_*) name if unset.
fn env_with_fallback(primary: &str, fallback: &str) -> Option<String> {
    optional_nonempty(primary).or_else(|| optional_nonempty(fallback))
}

fn required_with_fallback(primary: &str, fallback: &str) -> Result<String> {
    env_with_fallback(primary, fallback)
        .ok_or_else(|| anyhow!("missing required env var: {primary} (or {fallback})"))
}
