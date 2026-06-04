//! Persistent BTCPay connection state.
//!
//! Runtime credentials (API key, store, webhook secret) live in the DB so that
//! the operator can reconfigure BTCPay from the StartOS dashboard without
//! editing env vars or restarting the container.
//!
//! Written on first connect (via the authorize flow) and on explicit
//! reconnects. Read at startup to construct the `BtcpayClient`.

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone)]
pub struct BtcpayConfig {
    pub base_url: String,
    pub api_key: String,
    pub store_id: String,
    pub webhook_id: Option<String>,
    pub webhook_secret: String,
}

/// Load the current BTCPay config. Returns `None` if the operator has not
/// completed the authorize flow yet.
pub async fn load(pool: &SqlitePool) -> Result<Option<BtcpayConfig>> {
    let row = sqlx::query(
        "SELECT base_url, api_key, store_id, webhook_id, webhook_secret \
         FROM btcpay_config WHERE id = 1",
    )
    .fetch_optional(pool)
    .await
    .context("loading btcpay_config")?;

    Ok(row.map(|r| BtcpayConfig {
        base_url: r.get("base_url"),
        api_key: r.get("api_key"),
        store_id: r.get("store_id"),
        webhook_id: r.get("webhook_id"),
        webhook_secret: r.get("webhook_secret"),
    }))
}

/// Delete the entire BTCPay config row. Used by the Disconnect flow.
/// Subsequent calls to `load` return `None` until the operator
/// re-authorizes.
pub async fn clear(pool: &SqlitePool) -> Result<()> {
    sqlx::query("DELETE FROM btcpay_config WHERE id = 1")
        .execute(pool)
        .await
        .context("clearing btcpay_config")?;
    Ok(())
}

/// Upsert the full config. Called by the authorize-callback path after the
/// service has fetched/created everything it needs from BTCPay.
pub async fn save(pool: &SqlitePool, cfg: &BtcpayConfig) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO btcpay_config \
            (id, base_url, api_key, store_id, webhook_id, webhook_secret, connected_at) \
         VALUES (1, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET \
            base_url = excluded.base_url, \
            api_key = excluded.api_key, \
            store_id = excluded.store_id, \
            webhook_id = excluded.webhook_id, \
            webhook_secret = excluded.webhook_secret, \
            connected_at = excluded.connected_at",
    )
    .bind(&cfg.base_url)
    .bind(&cfg.api_key)
    .bind(&cfg.store_id)
    .bind(cfg.webhook_id.as_deref())
    .bind(&cfg.webhook_secret)
    .bind(&now)
    .execute(pool)
    .await
    .context("saving btcpay_config")?;
    Ok(())
}

/// Record a new in-flight authorize state token. `merchant_profile_id`
/// (multi-provider model, migration 0022) names which merchant profile
/// the resulting provider row should attach to when the callback fires
/// — None falls back to "the default profile" at consume-time.
pub async fn record_authorize_state(
    pool: &SqlitePool,
    token: &str,
    merchant_profile_id: Option<&str>,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO btcpay_authorize_state (state_token, merchant_profile_id, created_at) \
         VALUES (?, ?, ?)",
    )
    .bind(token)
    .bind(merchant_profile_id)
    .bind(&now)
    .execute(pool)
    .await
    .context("recording btcpay authorize state")?;
    // Best-effort prune of rows older than 30 minutes.
    let cutoff = (Utc::now() - chrono::Duration::minutes(30)).to_rfc3339();
    let _ = sqlx::query("DELETE FROM btcpay_authorize_state WHERE created_at < ?")
        .bind(&cutoff)
        .execute(pool)
        .await;
    Ok(())
}

/// Validate that `token` was issued recently and has not been consumed.
/// Consumes (deletes) the token on success so a replay fails, and
/// returns the `merchant_profile_id` recorded at start-connect time so
/// the callback knows which profile to attach the new provider to.
pub async fn consume_authorize_state(
    pool: &SqlitePool,
    token: &str,
) -> Result<Option<String>> {
    use sqlx::Row;
    let cutoff = (Utc::now() - chrono::Duration::minutes(30)).to_rfc3339();
    let row = sqlx::query(
        "SELECT state_token, merchant_profile_id FROM btcpay_authorize_state \
         WHERE state_token = ? AND created_at >= ?",
    )
    .bind(token)
    .bind(&cutoff)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Err(anyhow!("unknown or expired authorize state token"));
    };
    let merchant_profile_id: Option<String> = row.try_get("merchant_profile_id").ok().flatten();

    sqlx::query("DELETE FROM btcpay_authorize_state WHERE state_token = ?")
        .bind(token)
        .execute(pool)
        .await?;
    Ok(merchant_profile_id)
}
