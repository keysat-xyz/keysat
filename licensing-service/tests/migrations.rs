//! Migration regression tests.
//!
//! Boots a real SQLite database (per test, on a tempfile) with the same
//! pool options the daemon uses in production (see `src/db/mod.rs`),
//! applies the SQL migrations from disk one at a time, and asserts schema
//! + data integrity at each step.
//!
//! The trigger for this file: migration `0009_discount_codes_set_price.sql`
//! shipped a bug that crashed daemon boot on any install with rows in
//! `discount_redemptions` (SQLite error 787, FOREIGN KEY constraint
//! failed, surfaced at COMMIT). None of the existing crypto/webhook unit
//! tests touched the database, so the bug went undetected. These tests
//! reproduce the original failure mode against a populated DB and catch
//! the same class of bug on any future migration.
//!
//! We deliberately bypass `sqlx::migrate!()` here. The macro applies all
//! migrations as a single batch and we need per-migration control so we
//! can seed fixtures *between* migrations — e.g. populate
//! `discount_redemptions` after 0004 lands and before 0009 runs.

use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use tempfile::NamedTempFile;

const MIGRATIONS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");

fn migration_files() -> Vec<PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(MIGRATIONS_DIR)
        .expect("read migrations dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("sql"))
        .collect();
    files.sort();
    files
}

/// Open a fresh pool against a throwaway tempfile, mirroring
/// `src/db/mod.rs::init` exactly. The `NamedTempFile` is returned alongside
/// the pool so the caller can keep it alive for the duration of the test
/// — when it drops, the OS reclaims the file.
async fn make_pool() -> (SqlitePool, NamedTempFile) {
    let tmp = NamedTempFile::new().expect("create tempfile");
    let url = format!("sqlite://{}", tmp.path().display());
    let opts = SqliteConnectOptions::from_str(&url)
        .expect("parse sqlite url")
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(opts)
        .await
        .expect("connect to sqlite");
    (pool, tmp)
}

/// Apply migrations in the half-open index range `[start, end)` by reading
/// the .sql files from disk. Each migration runs in its own transaction
/// (matches sqlx-migrate behaviour). Splitting on a range — instead of
/// always applying from 0 — matters because `ALTER TABLE ADD COLUMN`
/// statements in our migrations don't have `IF NOT EXISTS` guards, so
/// re-applying 0003 onto an already-migrated DB fails with "duplicate
/// column name".
async fn apply_range(pool: &SqlitePool, start: usize, end: usize) -> anyhow::Result<()> {
    let files = migration_files();
    assert!(
        end <= files.len() && start <= end,
        "invalid migration range {start}..{end} (have {} migrations)",
        files.len()
    );
    for path in &files[start..end] {
        let sql = std::fs::read_to_string(path)?;
        let mut tx = pool.begin().await?;
        sqlx::raw_sql(&sql)
            .execute(&mut *tx)
            .await
            .map_err(|e| anyhow::anyhow!("applying {}: {e}", path.display()))?;
        tx.commit().await?;
    }
    Ok(())
}

async fn apply_through(pool: &SqlitePool, n: usize) -> anyhow::Result<()> {
    apply_range(pool, 0, n).await
}

async fn apply_all(pool: &SqlitePool) -> anyhow::Result<()> {
    apply_through(pool, migration_files().len()).await
}

/// Run SQLite's built-in consistency checks. Both should return clean rows
/// on a healthy database; either failing is a hard error for our tests.
async fn assert_db_clean(pool: &SqlitePool) -> anyhow::Result<()> {
    let violations: Vec<(String, Option<i64>, String, i64)> =
        sqlx::query_as::<_, (String, Option<i64>, String, i64)>("PRAGMA foreign_key_check")
            .fetch_all(pool)
            .await?;
    anyhow::ensure!(
        violations.is_empty(),
        "foreign_key_check violations: {violations:?}"
    );
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(pool)
        .await?;
    anyhow::ensure!(integrity == "ok", "integrity_check failed: {integrity}");
    Ok(())
}

/// Insert one row into every table that participates in a foreign-key
/// chain. Schema state assumed: post-0008 — i.e. after tiered pricing
/// adds `policies.public` and `invoices.policy_id`, but before 0009
/// rebuilds discount_codes. Each row deliberately uses values that are
/// otherwise indistinguishable from real production data.
///
/// Skips standalone tables that aren't part of the FK web that bit us
/// (server_keys, btcpay_*, settings, sessions, rate_buckets, audit_log,
/// validation_log).
async fn seed_realistic_fixtures(pool: &SqlitePool) -> anyhow::Result<()> {
    let now = "2026-05-08T00:00:00Z";

    sqlx::query(
        "INSERT INTO products(id, slug, name, price_sats, created_at, updated_at) \
         VALUES(?, ?, ?, ?, ?, ?)",
    )
    .bind("p1")
    .bind("test-product")
    .bind("Test Product")
    .bind(10_000_i64)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO policies(id, product_id, name, slug, created_at, updated_at) \
         VALUES(?, ?, ?, ?, ?, ?)",
    )
    .bind("pol1")
    .bind("p1")
    .bind("Standard")
    .bind("standard")
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO invoices(id, btcpay_invoice_id, product_id, status, amount_sats, \
         checkout_url, created_at, updated_at, policy_id) \
         VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("inv1")
    .bind("btcpay-inv-1")
    .bind("p1")
    .bind("settled")
    .bind(10_000_i64)
    .bind("https://btcpay.example/i/1")
    .bind(now)
    .bind(now)
    .bind("pol1")
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO licenses(id, product_id, invoice_id, status, issued_at, policy_id) \
         VALUES(?, ?, ?, ?, ?, ?)",
    )
    .bind("lic1")
    .bind("p1")
    .bind("inv1")
    .bind("active")
    .bind(now)
    .bind("pol1")
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO machines(id, license_id, fingerprint, fingerprint_hash, activated_at) \
         VALUES(?, ?, ?, ?, ?)",
    )
    .bind("mac1")
    .bind("lic1")
    .bind("raw-fingerprint")
    .bind("sha256hex")
    .bind(now)
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO discount_codes(id, code, kind, amount, created_at, updated_at) \
         VALUES(?, ?, ?, ?, ?, ?)",
    )
    .bind("dc1")
    .bind("LAUNCH50")
    .bind("percent")
    .bind(5_000_i64)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO discount_redemptions(id, code_id, invoice_id, license_id, status, \
         discount_applied_sats, base_price_sats, final_price_sats, created_at, updated_at) \
         VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("red1")
    .bind("dc1")
    .bind("inv1")
    .bind("lic1")
    .bind("redeemed")
    .bind(5_000_i64)
    .bind(10_000_i64)
    .bind(5_000_i64)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO webhook_endpoints(id, url, secret, created_at, updated_at) \
         VALUES(?, ?, ?, ?, ?)",
    )
    .bind("wh1")
    .bind("https://example.com/hook")
    .bind("0123456789abcdef")
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO webhook_deliveries(id, endpoint_id, event_type, payload_json, created_at) \
         VALUES(?, ?, ?, ?, ?)",
    )
    .bind("del1")
    .bind("wh1")
    .bind("license.issued")
    .bind("{}")
    .bind(now)
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO tip_attempts(id, license_id, policy_id, recipient, amount_sats, pct_bps, \
         status, created_at) \
         VALUES(?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("tip1")
    .bind("lic1")
    .bind("pol1")
    .bind("tips@example.com")
    .bind(50_i64)
    .bind(50_i64)
    .bind("sent")
    .bind(now)
    .execute(pool)
    .await?;

    Ok(())
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

/// Baseline. Every migration applies cleanly to an empty database. Until
/// today, this was the only scenario the migration set was ever vetted
/// against — and it's still missing as a real test.
#[tokio::test]
async fn all_migrations_apply_to_empty_db() {
    let (pool, _tmp) = make_pool().await;
    apply_all(&pool).await.expect("migrations apply to empty db");
    assert_db_clean(&pool).await.expect("post-migration db is clean");
}

/// The regression. With realistic data populated through migration 0008,
/// migration 0009 must:
///   - apply without crashing (no SQLite error 787),
///   - preserve the existing discount_redemptions row,
///   - preserve the existing discount_codes row,
///   - accept the new `set_price` kind on a fresh insert,
///   - still reject invalid kinds (CHECK constraint intact).
///
/// Reverting `0009_discount_codes_set_price.sql` to the buggy first
/// revision makes this test fail at the `apply_through(&pool, 9)` step
/// with "FOREIGN KEY constraint failed" — same error operators saw in
/// the StartOS service logs.
#[tokio::test]
async fn migration_0009_survives_existing_redemptions() {
    let (pool, _tmp) = make_pool().await;

    apply_range(&pool, 0, 8)
        .await
        .expect("apply migrations 0001 through 0008");
    seed_realistic_fixtures(&pool)
        .await
        .expect("seed pre-0009 fixtures");

    apply_range(&pool, 8, 9)
        .await
        .expect("0009 must survive a populated discount_redemptions");

    assert_db_clean(&pool).await.expect("db clean after 0009");

    let red_count: i64 = sqlx::query_scalar("SELECT count(*) FROM discount_redemptions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(red_count, 1, "discount_redemptions row preserved");

    let code_count: i64 = sqlx::query_scalar("SELECT count(*) FROM discount_codes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(code_count, 1, "discount_codes row preserved");

    sqlx::query(
        "INSERT INTO discount_codes(id, code, kind, amount, created_at, updated_at) \
         VALUES('dc2', 'PRICE25', 'set_price', 2500, '2026-05-08', '2026-05-08')",
    )
    .execute(&pool)
    .await
    .expect("the new set_price kind must now be accepted by CHECK");

    let bad = sqlx::query(
        "INSERT INTO discount_codes(id, code, kind, amount, created_at, updated_at) \
         VALUES('dc3', 'BAD', 'definitely_not_real', 0, '2026-05-08', '2026-05-08')",
    )
    .execute(&pool)
    .await;
    assert!(
        bad.is_err(),
        "garbage kinds must still be rejected by the CHECK constraint"
    );
}

/// Idempotency. The original 0009 incident left some operators with a
/// checksum-mismatch path: clear the row from `_sqlx_migrations`, let the
/// fixed 0009 re-apply. That re-apply path needs to be safe — running
/// 0009 twice against the same DB must not corrupt or duplicate data.
#[tokio::test]
async fn migration_0009_is_idempotent() {
    let (pool, _tmp) = make_pool().await;
    apply_all(&pool).await.expect("first full apply");
    seed_realistic_fixtures(&pool)
        .await
        .expect("seed after full apply");

    let red_before: i64 = sqlx::query_scalar("SELECT count(*) FROM discount_redemptions")
        .fetch_one(&pool)
        .await
        .unwrap();
    let code_before: i64 = sqlx::query_scalar("SELECT count(*) FROM discount_codes")
        .fetch_one(&pool)
        .await
        .unwrap();

    let last = migration_files().into_iter().last().unwrap();
    let sql = std::fs::read_to_string(&last).unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::raw_sql(&sql)
        .execute(&mut *tx)
        .await
        .expect("0009 re-apply on already-migrated db");
    tx.commit().await.unwrap();

    let red_after: i64 = sqlx::query_scalar("SELECT count(*) FROM discount_redemptions")
        .fetch_one(&pool)
        .await
        .unwrap();
    let code_after: i64 = sqlx::query_scalar("SELECT count(*) FROM discount_codes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(red_before, red_after, "redemptions unchanged on re-apply");
    assert_eq!(code_before, code_after, "discount_codes unchanged on re-apply");

    assert_db_clean(&pool).await.expect("db clean after re-apply");
}

/// Future-proofing. Always seeds fixtures one migration before the end,
/// then applies the final migration. As new migrations land (0010,
/// 0011, …), they get vetted against populated data automatically; no
/// new test needs to be written. If a future migration introduces the
/// same DROP+inbound-FK pattern that bit 0009, this test catches it.
#[tokio::test]
async fn last_migration_preserves_foreign_keys_with_data() {
    let (pool, _tmp) = make_pool().await;
    let total = migration_files().len();
    assert!(total >= 2, "need at least 2 migrations for this test");

    apply_range(&pool, 0, total - 1)
        .await
        .expect("apply all but the last migration");
    seed_realistic_fixtures(&pool)
        .await
        .expect("seed before the final migration");
    apply_range(&pool, total - 1, total)
        .await
        .expect("final migration applies cleanly with data present");

    assert_db_clean(&pool)
        .await
        .expect("db clean after final migration");
}
