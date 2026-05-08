//! BTC / fiat exchange-rate fetcher.
//!
//! Reads the rate from a small chain of public sources, caches the
//! result in-memory with a 60-second TTL, and falls through on
//! per-source failure. The cache is shared across the daemon — every
//! call to `get_rate(&state, "USD")` either returns the cached value
//! (cheap) or refreshes it (one HTTP call per minute per currency).
//!
//! ## Source priority
//!
//! 1. **Kraken** — `https://api.kraken.com/0/public/Ticker?pair=XBT<CCY>`
//!    matches the operator's mental model since BTCPay uses Kraken
//!    as its default rate provider too. Means the daemon and BTCPay
//!    agree on the rate when we use Kraken on both ends.
//! 2. **Coinbase** — `https://api.coinbase.com/v2/exchange-rates?currency=BTC`
//!    Robust public API, no auth, simple JSON. Good fallback when
//!    Kraken is rate-limiting us or having an outage.
//! 3. **CoinGecko** — `https://api.coingecko.com/api/v3/simple/price?ids=bitcoin&vs_currencies=usd,eur`
//!    Last resort. Their public free tier has aggressive rate
//!    limits, so it's intentionally last.
//!
//! ## Test-mode pin
//!
//! The settings table key `manual_rate_pin_<CCY>` (e.g.
//! `manual_rate_pin_USD = "65000"`) overrides the fetcher entirely.
//! Used by integration tests that want a deterministic conversion
//! without hitting the network. Production operators can also set
//! this to lock the rate in for a maintenance window if a fetcher
//! glitch is producing weird quotes.

use crate::api::AppState;
use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;

/// How long a cached rate is considered fresh. 60s is a reasonable
/// trade-off — most BTC price moves under 60s are <0.1%, well below
/// any operator-meaningful threshold, and longer caches risk staleness
/// during volatility spikes.
const TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct CachedRate {
    /// "<currency>-per-BTC" — for USD this is the dollar price of 1 BTC.
    pub units_per_btc: f64,
    /// Where the rate came from: 'kraken' | 'coinbase' | 'coingecko' | 'manual_pin'.
    pub source: String,
    /// When the fetch happened.
    pub fetched_at: SystemTime,
}

/// Process-global cache. Keyed by uppercase currency code (e.g.
/// "USD", "EUR"). Held in `AppState` via `Arc` for cheap clones.
#[derive(Default)]
pub struct RateCache {
    inner: RwLock<HashMap<String, CachedRate>>,
}

impl RateCache {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Read-only snapshot of the current cache contents. Used by
    /// the admin UI to show "what's the daemon currently quoting."
    pub async fn snapshot(&self) -> HashMap<String, CachedRate> {
        self.inner.read().await.clone()
    }

    /// Drop a single currency's cached entry so the next `get_rate`
    /// call refetches from the source chain. Used by the
    /// `POST /v1/admin/rates/refresh` admin action.
    pub async fn invalidate(&self, currency: &str) {
        let mut cache = self.inner.write().await;
        cache.remove(&currency.to_uppercase());
    }
}

/// Fetch the current rate for `currency` (uppercase ISO code) against
/// BTC. Returns the cached value if fresh; otherwise hits the fallback
/// chain. Manual pins in the settings table win over the chain.
pub async fn get_rate(state: &AppState, currency: &str) -> Result<CachedRate> {
    let currency = currency.to_uppercase();
    if currency == "SAT" || currency == "BTC" {
        // Trivial conversion — the rest of the daemon shouldn't be
        // calling this for sat-currency products, but return a
        // sensible identity if it does.
        return Ok(CachedRate {
            units_per_btc: 100_000_000.0, // 1 BTC = 100M sats
            source: "identity".to_string(),
            fetched_at: SystemTime::now(),
        });
    }

    // Manual pin from settings table — wins over the cache + chain.
    // Always re-checked on every call (no TTL) so an operator can
    // un-pin and immediately fall back to live rates.
    let pin_key = format!("manual_rate_pin_{currency}");
    if let Ok(Some(raw)) = crate::db::repo::settings_get(&state.db, &pin_key).await {
        if let Ok(value) = raw.parse::<f64>() {
            if value > 0.0 {
                let pinned = CachedRate {
                    units_per_btc: value,
                    source: "manual_pin".to_string(),
                    fetched_at: SystemTime::now(),
                };
                // Mirror to cache so admin GET /v1/admin/rates
                // surfaces the pinned value (without it, the
                // snapshot would always show "no rates cached"
                // for pinned currencies).
                let mut cache = state.rates.inner.write().await;
                cache.insert(currency.clone(), pinned.clone());
                return Ok(pinned);
            }
        }
    }

    // Fast path: cached and fresh.
    {
        let cache = state.rates.inner.read().await;
        if let Some(cached) = cache.get(&currency) {
            if cached.fetched_at.elapsed().unwrap_or(TTL) < TTL {
                return Ok(cached.clone());
            }
        }
    }

    // Slow path: hit the chain.
    let fresh = fetch_with_fallback(&currency).await?;
    let mut cache = state.rates.inner.write().await;
    cache.insert(currency, fresh.clone());
    Ok(fresh)
}

async fn fetch_with_fallback(currency: &str) -> Result<CachedRate> {
    // Sources in priority order. Each closure returns the rate if
    // it succeeds, propagates the error otherwise. We collect
    // errors so a final failure surfaces all three causes for
    // debugging.
    let mut errors: Vec<String> = Vec::new();

    match fetch_kraken(currency).await {
        Ok(r) => return Ok(r),
        Err(e) => errors.push(format!("kraken: {e:#}")),
    }
    match fetch_coinbase(currency).await {
        Ok(r) => return Ok(r),
        Err(e) => errors.push(format!("coinbase: {e:#}")),
    }
    match fetch_coingecko(currency).await {
        Ok(r) => return Ok(r),
        Err(e) => errors.push(format!("coingecko: {e:#}")),
    }
    Err(anyhow!(
        "all rate sources failed for {currency}: {}",
        errors.join("; ")
    ))
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .context("build reqwest client")
}

async fn fetch_kraken(currency: &str) -> Result<CachedRate> {
    // Kraken pair codes use 'XBT' for BTC and 'Z' prefixes for
    // legacy fiat (ZUSD, ZEUR). The c[0] field is the latest
    // closed-trade price.
    let pair = match currency {
        "USD" => "XXBTZUSD",
        "EUR" => "XXBTZEUR",
        _ => return Err(anyhow!("kraken: unsupported currency {currency}")),
    };
    let url = format!("https://api.kraken.com/0/public/Ticker?pair={pair}");
    let body: Value = http_client()?.get(&url).send().await?.error_for_status()?.json().await?;
    let errors = body.get("error").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    if !errors.is_empty() {
        return Err(anyhow!("kraken returned errors: {errors:?}"));
    }
    let price_str = body
        .pointer(&format!("/result/{pair}/c/0"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("kraken: response missing /result/{pair}/c/0"))?;
    let value: f64 = price_str.parse().context("kraken: parse price")?;
    Ok(CachedRate {
        units_per_btc: value,
        source: "kraken".to_string(),
        fetched_at: SystemTime::now(),
    })
}

async fn fetch_coinbase(currency: &str) -> Result<CachedRate> {
    let url = "https://api.coinbase.com/v2/exchange-rates?currency=BTC";
    let body: Value = http_client()?.get(url).send().await?.error_for_status()?.json().await?;
    let rate_str = body
        .pointer(&format!("/data/rates/{currency}"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("coinbase: response missing /data/rates/{currency}"))?;
    let value: f64 = rate_str.parse().context("coinbase: parse rate")?;
    Ok(CachedRate {
        units_per_btc: value,
        source: "coinbase".to_string(),
        fetched_at: SystemTime::now(),
    })
}

async fn fetch_coingecko(currency: &str) -> Result<CachedRate> {
    let cur_lower = currency.to_lowercase();
    let url = format!(
        "https://api.coingecko.com/api/v3/simple/price?ids=bitcoin&vs_currencies={cur_lower}"
    );
    let body: Value = http_client()?.get(&url).send().await?.error_for_status()?.json().await?;
    let value = body
        .pointer(&format!("/bitcoin/{cur_lower}"))
        .and_then(|v| v.as_f64())
        .ok_or_else(|| anyhow!("coingecko: response missing /bitcoin/{cur_lower}"))?;
    Ok(CachedRate {
        units_per_btc: value,
        source: "coingecko".to_string(),
        fetched_at: SystemTime::now(),
    })
}

/// Convert a fiat amount (smallest unit, e.g. cents) to sats using
/// the cached/fetched rate. Returns the sat amount as i64 (rounded
/// to nearest sat — fractional sats don't exist).
///
/// `value` is in the smallest unit of `currency` (cents for USD).
/// Returns an `(sats, rate_centibps)` pair so callers can pin both
/// on the invoice row for audit.
pub async fn convert_to_sats(
    state: &AppState,
    currency: &str,
    value: i64,
) -> Result<ConversionResult> {
    let currency = currency.to_uppercase();
    if currency == "SAT" {
        return Ok(ConversionResult {
            sats: value,
            rate_centibps: None,
            source: "identity".to_string(),
        });
    }
    let rate = get_rate(state, &currency).await?;
    // value is cents (for USD/EUR). 1 BTC = 100_000_000 sats.
    // sats = value / units_per_btc * 100_000_000 / 100
    //      = value * 100_000_000 / (units_per_btc * 100)
    //      = value * 1_000_000 / units_per_btc
    // (the /100 cancels half of 100_000_000 since `value` is in
    //  cents — the smallest unit is 1/100 of the main unit).
    let sats_f = (value as f64) * 1_000_000.0 / rate.units_per_btc;
    let sats = sats_f.round() as i64;

    // Encode the rate as centibps (rate × 10,000) for the invoice
    // row. See migrations/0010_multi_currency.sql for the encoding
    // rationale.
    let rate_centibps = (rate.units_per_btc * 10_000.0).round() as i64;

    Ok(ConversionResult {
        sats,
        rate_centibps: Some(rate_centibps),
        source: rate.source,
    })
}

#[derive(Debug, Clone)]
pub struct ConversionResult {
    pub sats: i64,
    /// rate × 10,000 in operator-currency-per-BTC units. `None` for
    /// identity (SAT-currency) conversions.
    pub rate_centibps: Option<i64>,
    pub source: String,
}
