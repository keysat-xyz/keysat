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

    run_migrations_with_self_heal(&pool).await?;

    tracing::info!(path = %path.display(), "database ready");
    Ok(pool)
}

/// Migrations that have been certified safe to re-run from scratch. If
/// sqlx complains about a checksum mismatch on one of these (which can
/// happen when the file content shifts subtly between builds —
/// trailing whitespace, line endings, build-host normalization), the
/// daemon clears the row from `_sqlx_migrations` and retries instead
/// of crash-looping.
///
/// Add a migration's version to this list ONLY when:
///   - It's `CREATE TABLE IF NOT EXISTS` / `INSERT OR IGNORE` style
///     OR a deliberate drop-and-rebuild that produces identical state
///     regardless of starting point.
///   - It does NOT include `ALTER TABLE ADD COLUMN` (that errors on
///     re-apply — SQLite has no `ADD COLUMN IF NOT EXISTS`).
///   - You've tested it via `migration_NNNN_is_idempotent` in
///     `tests/migrations.rs`.
const IDEMPOTENT_MIGRATIONS: &[i64] = &[
    9, // see migrations/0009_discount_codes_set_price.sql — explicitly
       // designed as a stash-drop-rebuild-restore that yields the same
       // end state regardless of the starting state. Pinned by
       // migration_0009_is_idempotent in tests/migrations.rs.
];

/// Run migrations with auto-recovery for the
/// `MigrateError::VersionMismatch` case on idempotent migrations.
///
/// Why this exists: sqlx records a SHA-384 of each migration file's
/// bytes when it's first applied, then verifies the on-disk bytes
/// still match on every subsequent boot. The verification is too
/// strict for our use case — a rebuild-from-clean-source can produce
/// different bytes (trailing newlines, line endings, etc.) even when
/// the SQL semantics are unchanged. Without this self-heal, every
/// such drift requires the operator to SSH in and run
/// `DELETE FROM _sqlx_migrations WHERE version = N` by hand.
///
/// The auto-clear is gated on `IDEMPOTENT_MIGRATIONS` so we only
/// re-apply migrations we've explicitly certified as safe to re-run.
/// Anything else still propagates the error and crashes the daemon —
/// preventing accidental data corruption from re-running a destructive
/// migration.
async fn run_migrations_with_self_heal(pool: &SqlitePool) -> Result<()> {
    use sqlx::migrate::MigrateError;
    let migrator = sqlx::migrate!("./migrations");
    match migrator.run(pool).await {
        Ok(()) => Ok(()),
        Err(MigrateError::VersionMismatch(version))
            if IDEMPOTENT_MIGRATIONS.contains(&version) =>
        {
            tracing::warn!(
                migration = version,
                "migration {version} checksum mismatch on a known-idempotent migration; \
                 clearing _sqlx_migrations row and retrying. This usually means the \
                 migration file's bytes drifted subtly between builds (trailing \
                 whitespace, line endings) without a semantic change."
            );
            sqlx::query("DELETE FROM _sqlx_migrations WHERE version = ?")
                .bind(version)
                .execute(pool)
                .await
                .with_context(|| {
                    format!("clearing _sqlx_migrations row for self-heal of v{version}")
                })?;
            migrator
                .run(pool)
                .await
                .with_context(|| format!("retry of migrations after self-heal of v{version}"))?;
            tracing::info!(
                migration = version,
                "migration {version} re-applied successfully after checksum self-heal"
            );
            Ok(())
        }
        Err(e) => Err(e).context("running migrations"),
    }
}
