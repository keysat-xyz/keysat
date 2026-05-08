//! Persistent Zaprite connection state.
//!
//! Singleton row in `zaprite_config` (id = 1, see migration 0012).
//! Mirrors the BTCPay-config pattern: written on first connect,
//! read at startup to construct a `ZapriteProvider`.
//!
//! No webhook_secret column — Zaprite's webhook delivery is not
//! signed by the provider. See `payment::zaprite` module-level
//! comment for the security model.

use anyhow::{Context, Result};
use chrono::Utc;
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone)]
pub struct ZapriteConfig {
    pub api_key: String,
    pub base_url: String,
    pub webhook_id: Option<String>,
}

pub async fn load(pool: &SqlitePool) -> Result<Option<ZapriteConfig>> {
    let row = sqlx::query(
        "SELECT api_key, base_url, webhook_id FROM zaprite_config WHERE id = 1",
    )
    .fetch_optional(pool)
    .await
    .context("loading zaprite_config")?;
    Ok(row.map(|r| ZapriteConfig {
        api_key: r.get("api_key"),
        base_url: r.get("base_url"),
        webhook_id: r.get("webhook_id"),
    }))
}

pub async fn save(pool: &SqlitePool, cfg: &ZapriteConfig) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO zaprite_config(id, api_key, base_url, webhook_id, connected_at, updated_at) \
         VALUES(1, ?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET \
            api_key = excluded.api_key, \
            base_url = excluded.base_url, \
            webhook_id = excluded.webhook_id, \
            updated_at = excluded.updated_at",
    )
    .bind(&cfg.api_key)
    .bind(&cfg.base_url)
    .bind(&cfg.webhook_id)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .context("saving zaprite_config")?;
    Ok(())
}

pub async fn clear(pool: &SqlitePool) -> Result<()> {
    sqlx::query("DELETE FROM zaprite_config WHERE id = 1")
        .execute(pool)
        .await
        .context("clearing zaprite_config")?;
    Ok(())
}
