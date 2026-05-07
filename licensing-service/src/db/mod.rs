//! Database layer. Runs migrations on startup and provides typed repository
//! helpers for each table. Using `sqlx::query` (not `query!`) keeps the
//! project buildable without a live DB at compile time.

pub mod repo;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;
use std::path::Path;
use std::str::FromStr;

/// Opens (or creates) the SQLite database at `path`, applies migrations, and
/// returns a connection pool ready for use.
pub async fn init(path: &Path) -> Result<SqlitePool> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent dir for db at {}", path.display()))?;
        }
    }

    let url = format!("sqlite://{}", path.display());
    let opts = SqliteConnectOptions::from_str(&url)?
        .create_if_missing(true)
        // WAL mode is the right default for a read-heavy validation workload.
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await
        .with_context(|| format!("opening sqlite at {}", path.display()))?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("running migrations")?;

    tracing::info!(path = %path.display(), "database ready");
    Ok(pool)
}
