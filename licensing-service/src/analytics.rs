//! Opt-in community analytics.
//!
//! Off by default. When the operator toggles it on (via the admin UI),
//! the daemon periodically POSTs a small anonymous heartbeat to a
//! configurable collector URL. The shape is designed to be useful
//! for "is Keysat being used and growing?" without exposing anything
//! that identifies a specific operator.
//!
//! ## What's sent (and what isn't)
//!
//! Sent:
//! - `install_uuid` — random UUIDv4 generated on first opt-in,
//!   stored in the settings table. NOT derived from operator
//!   identity, store id, or any user-supplied value. Resetting
//!   analytics opt-in regenerates it.
//! - `daemon_version` — e.g. `"0.1.0:46"`.
//! - `tier` — `"unlicensed" | "creator" | "pro" | "patron"`.
//! - `counts` — rounded down to the nearest 5 to prevent
//!   fingerprinting an operator by exact license count.
//! - `uptime_seconds` — bucketed to "<1d" / "1-7d" / "1-4w" / ">4w".
//!
//! Not sent:
//! - Operator name, public URL, BTCPay URL, store id.
//! - Any product or policy slug, name, or description.
//! - Any buyer email, license id, invoice id, or fingerprint.
//! - Admin API key, webhook secrets, or any other credential.
//!
//! The opt-in toggle lives in the admin UI (Overview page), with a
//! "what gets sent" disclosure and a one-click opt-out. The daemon
//! never starts the heartbeat task speculatively — the toggle has
//! to be on AND a collector URL has to be configured.

use crate::api::AppState;
use crate::db::repo;
use serde::Serialize;
use std::time::Duration;
use uuid::Uuid;

pub const SETTING_ENABLED: &str = "community_analytics_enabled";
pub const SETTING_INSTALL_UUID: &str = "community_install_uuid";
pub const SETTING_COLLECTOR_URL: &str = "community_collector_url";

/// Default upstream collector. v0.1.0:47 ships with this empty —
/// no URL means no requests, even if `enabled = true`. We'll set
/// the public collector URL on a future release once the
/// keysat.xyz/community endpoint is live.
const DEFAULT_COLLECTOR_URL: Option<&str> = None;

#[derive(Debug, Serialize)]
pub struct Heartbeat {
    pub install_uuid: String,
    pub daemon_version: &'static str,
    pub tier: &'static str,
    pub counts: HeartbeatCounts,
    pub uptime_bucket: &'static str,
    pub schema_version: u32,
}

#[derive(Debug, Serialize)]
pub struct HeartbeatCounts {
    pub products: i64,
    pub active_licenses: i64,
    pub settled_invoices: i64,
}

const HEARTBEAT_SCHEMA_VERSION: u32 = 1;

/// Round down to the nearest `step`. `floor_to(23, 5) == 20`. Used
/// to prevent fingerprinting an operator by their exact license
/// count — a heartbeat that says "20-24 active licenses" is
/// sufficient signal without being unique.
fn floor_to(value: i64, step: i64) -> i64 {
    if step <= 0 {
        return value;
    }
    (value / step) * step
}

fn uptime_bucket(secs: u64) -> &'static str {
    let day = 86_400;
    let week = 7 * day;
    let four_weeks = 4 * week;
    if secs < day {
        "<1d"
    } else if secs < week {
        "1-7d"
    } else if secs < four_weeks {
        "1-4w"
    } else {
        ">4w"
    }
}

/// Build a heartbeat snapshot from current state. Always callable —
/// returns the snapshot synchronously without sending anything.
/// `started_at_secs_since_epoch` is the daemon's start time (used
/// to compute the uptime bucket).
pub async fn build_heartbeat(
    state: &AppState,
    install_uuid: &str,
    started_at_secs: u64,
) -> anyhow::Result<Heartbeat> {
    let products: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM products")
        .fetch_one(&state.db)
        .await?;
    let active_licenses: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM licenses WHERE status = 'active'")
            .fetch_one(&state.db)
            .await?;
    let settled_invoices: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM invoices WHERE status = 'settled'")
            .fetch_one(&state.db)
            .await?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(started_at_secs);
    let uptime = now.saturating_sub(started_at_secs);

    let tier_label = crate::api::tier::current(state).await.label;

    Ok(Heartbeat {
        install_uuid: install_uuid.to_string(),
        daemon_version: env!("CARGO_PKG_VERSION"),
        tier: tier_label,
        counts: HeartbeatCounts {
            products: floor_to(products, 5),
            active_licenses: floor_to(active_licenses, 5),
            settled_invoices: floor_to(settled_invoices, 5),
        },
        uptime_bucket: uptime_bucket(uptime),
        schema_version: HEARTBEAT_SCHEMA_VERSION,
    })
}

/// Read the opt-in flag. Returns Ok(false) on any storage error so
/// we always default to "off" — never accidentally beacon because
/// the settings table is unreachable.
pub async fn is_enabled(state: &AppState) -> bool {
    match repo::settings_get(&state.db, SETTING_ENABLED).await {
        Ok(Some(v)) => v == "1" || v.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// Get-or-create the install UUID. Idempotent; the first call after
/// opt-in writes a fresh UUIDv4 to the settings table, all later
/// calls read it back.
pub async fn ensure_install_uuid(state: &AppState) -> anyhow::Result<String> {
    if let Some(existing) = repo::settings_get(&state.db, SETTING_INSTALL_UUID).await? {
        if !existing.is_empty() {
            return Ok(existing);
        }
    }
    let fresh = Uuid::new_v4().to_string();
    repo::settings_set(&state.db, SETTING_INSTALL_UUID, Some(&fresh)).await?;
    Ok(fresh)
}

/// Spawn the heartbeat-sending background task. No-op every tick if
/// the opt-in toggle is off OR no collector URL is configured.
///
/// Tick cadence: every 24 hours, with a small initial delay so we
/// don't hit the collector during boot if many operators restart at
/// once. Aligned roughly to the day so heartbeats don't cluster.
pub fn spawn(state: AppState) {
    let started_at_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    tokio::spawn(async move {
        // 5-minute grace period after boot before the first
        // heartbeat. Avoids beaconing during the warm-up window
        // when caches are empty and counts might be misleading.
        tokio::time::sleep(Duration::from_secs(300)).await;
        loop {
            if let Err(e) = tick(&state, started_at_secs).await {
                tracing::warn!(error = %e, "community-analytics heartbeat failed");
            }
            tokio::time::sleep(Duration::from_secs(86_400)).await;
        }
    });
}

async fn tick(state: &AppState, started_at_secs: u64) -> anyhow::Result<()> {
    if !is_enabled(state).await {
        return Ok(());
    }
    let collector_url = match repo::settings_get(&state.db, SETTING_COLLECTOR_URL).await? {
        Some(u) if !u.is_empty() => u,
        _ => match DEFAULT_COLLECTOR_URL {
            Some(u) => u.to_string(),
            None => return Ok(()), // explicitly opted in but no URL configured — silent no-op
        },
    };

    let install_uuid = ensure_install_uuid(state).await?;
    let payload = build_heartbeat(state, &install_uuid, started_at_secs).await?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let resp = client
        .post(&collector_url)
        .json(&payload)
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "collector responded with HTTP {}: {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        );
    }
    tracing::info!(
        collector = %collector_url,
        install_uuid = %install_uuid,
        "community-analytics heartbeat sent"
    );
    Ok(())
}
