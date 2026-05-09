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

    // Pinned to migration 0009 by its filename prefix, not by
    // "last in the list" — once 0010+ land they may not be
    // idempotent (additive ALTER TABLE statements aren't), but
    // 0009's whole point was being safely re-runnable.
    let nine = migration_files()
        .into_iter()
        .find(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .map_or(false, |s| s.starts_with("0009_"))
        })
        .expect("migration 0009 file must be present");
    let sql = std::fs::read_to_string(&nine).unwrap();
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

/// Regression for the v0.1.0:48 → :49 incident: the `_sqlx_migrations`
/// table records a checksum for each applied migration; on every
/// subsequent boot sqlx verifies the on-disk bytes still match.
/// Builds across versions can produce subtly different bytes
/// (trailing newlines, line-endings, build-host normalization) for
/// the same semantic SQL, which makes sqlx refuse to start with
/// "migration N was previously applied but has been modified" and
/// crashes the daemon.
///
/// `db::init` works around this by detecting the
/// `MigrateError::VersionMismatch` for migrations on the
/// `IDEMPOTENT_MIGRATIONS` allowlist (just `9` for now), clearing the
/// stale row, and retrying. This test simulates the exact scenario:
/// poison the recorded checksum for v9, run init, expect success.
#[tokio::test]
async fn db_init_self_heals_checksum_mismatch_on_idempotent_migrations() {
    let (pool, _tmp) = make_pool().await;

    // Step 1: apply all migrations cleanly to populate
    // _sqlx_migrations with current checksums.
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("first apply");

    // Step 2: poison the recorded checksum for v9. This simulates
    // the cross-build drift that triggered the production incident.
    let bogus_checksum: Vec<u8> = (0..48).map(|_| 0xEF).collect(); // Sha384 = 48 bytes
    let n = sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = 9")
        .bind(&bogus_checksum)
        .execute(&pool)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(n, 1, "_sqlx_migrations should have a row for v9");

    // Step 3: confirm sqlx::migrate! ALONE bails — proves the
    // poisoning works and that without self-heal the daemon would
    // crash here.
    let ungated = sqlx::migrate!("./migrations").run(&pool).await;
    assert!(
        matches!(
            ungated,
            Err(sqlx::migrate::MigrateError::VersionMismatch(9))
        ),
        "raw sqlx::migrate! should reject the poisoned row: got {ungated:?}"
    );

    // Step 4: drop the existing pool and call db::init on the same
    // file. The self-heal should clear v9's row, re-apply, succeed.
    let tmp_path = _tmp.path().to_path_buf();
    drop(pool);
    drop(_tmp);
    let healed = keysat::db::init(&tmp_path)
        .await
        .expect("db::init should self-heal the poisoned v9 row");

    // Sanity check: v9 is back in _sqlx_migrations with a fresh
    // (correct) checksum, and v10 is still there from the original
    // apply.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations WHERE version IN (9, 10)")
            .fetch_one(&healed)
            .await
            .unwrap();
    assert_eq!(count, 2, "both 9 and 10 should be recorded after self-heal");

    // The poisoned checksum was replaced with the real one.
    let new_checksum: Vec<u8> =
        sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 9")
            .fetch_one(&healed)
            .await
            .unwrap();
    assert_ne!(
        new_checksum, bogus_checksum,
        "self-heal must replace the poisoned checksum with the current one"
    );
}

/// Migration 0010 (multi-currency foundation): verifies that the
/// backfill correctly populates the new `price_currency` and
/// `price_value` columns against products that existed before the
/// migration. This is the contract the rest of the multi-currency
/// build assumes — every existing row must end up with
/// `price_currency = 'SAT'` and `price_value = price_sats`.
#[tokio::test]
async fn migration_0010_backfills_existing_products_to_sat() {
    let (pool, _tmp) = make_pool().await;
    apply_range(&pool, 0, 9)
        .await
        .expect("apply 0001..=0009 (everything before 0010)");

    // Seed three products with different sat amounts (including 0
    // for the free case) before 0010 runs.
    sqlx::query(
        "INSERT INTO products(id, slug, name, price_sats, created_at, updated_at) \
         VALUES('pa', 'a', 'Product A', 0, 't', 't'), \
               ('pb', 'b', 'Product B', 10000, 't', 't'), \
               ('pc', 'c', 'Product C', 250000, 't', 't')",
    )
    .execute(&pool)
    .await
    .expect("seed products");

    // Seed a policy with a price override so the policy backfill
    // (price_value_override = price_sats_override) is exercised.
    sqlx::query(
        "INSERT INTO policies(id, product_id, name, slug, price_sats_override, \
         created_at, updated_at) \
         VALUES('pol1', 'pb', 'Pro', 'pro', 50000, 't', 't')",
    )
    .execute(&pool)
    .await
    .expect("seed policy with override");

    // Apply 0010.
    apply_range(&pool, 9, 10)
        .await
        .expect("apply 0010_multi_currency");

    // After: every product has price_currency='SAT' and
    // price_value matches price_sats.
    let rows: Vec<(String, String, i64, i64)> = sqlx::query_as(
        "SELECT id, price_currency, price_value, price_sats \
         FROM products ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 3);
    for (id, currency, value, sats) in &rows {
        assert_eq!(currency, "SAT", "{id}: currency must default to SAT");
        assert_eq!(value, sats, "{id}: price_value must mirror price_sats");
    }

    // The policy override was backfilled.
    let pol: (Option<String>, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT price_currency_override, price_value_override, price_sats_override \
         FROM policies WHERE id = 'pol1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        pol.0.is_none(),
        "currency_override should stay NULL = 'inherit from product'"
    );
    assert_eq!(pol.1, Some(50000), "price_value_override backfilled");
    assert_eq!(pol.2, Some(50000), "original price_sats_override preserved");

    // The new currency index exists (uses CREATE INDEX IF NOT
    // EXISTS so this is implicit-correct, but assert the index is
    // there so a future schema rebuild can't silently lose it).
    let idx_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master \
         WHERE type='index' AND name='idx_products_currency'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(idx_count, 1, "currency index should exist after 0010");

    // FK + integrity invariants still hold.
    assert_db_clean(&pool).await.expect("db clean after 0010");
}

/// Migration 0011 (subscriptions schema): verifies that adding the
/// new policies columns + the subscriptions / subscription_invoices
/// tables doesn't break existing data, and that the new tables
/// accept rows via FK references back to licenses / policies /
/// invoices created under the prior schema.
#[tokio::test]
async fn migration_0011_adds_subscriptions_without_breaking_existing_data() {
    let (pool, _tmp) = make_pool().await;

    // Apply everything before 0011, populate realistic state.
    apply_range(&pool, 0, 10)
        .await
        .expect("apply 0001..=0010");
    seed_realistic_fixtures(&pool)
        .await
        .expect("seed pre-0011 fixtures");

    // Apply 0011.
    apply_range(&pool, 10, 11)
        .await
        .expect("apply 0011_subscriptions");

    // New policies columns exist with sensible defaults on existing rows.
    let (is_recurring, period, grace, trial): (i64, Option<i64>, i64, i64) = sqlx::query_as(
        "SELECT is_recurring, renewal_period_days, grace_period_days, trial_days \
         FROM policies WHERE id = 'pol1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(is_recurring, 0, "existing policies must default to non-recurring");
    assert_eq!(period, None, "renewal_period_days should be NULL on non-recurring rows");
    assert_eq!(grace, 7, "grace_period_days default should be 7");
    assert_eq!(trial, 0, "trial_days default should be 0");

    // The new tables exist and accept a subscription tied to the
    // existing fixture license.
    let now = "2026-05-08T12:00:00Z";
    sqlx::query(
        "INSERT INTO subscriptions(id, license_id, policy_id, product_id, period_days, \
         listed_currency, listed_value, status, started_at, next_renewal_at, \
         created_at, updated_at) \
         VALUES('sub1', 'lic1', 'pol1', 'p1', 30, 'USD', 2500, 'active', ?, ?, ?, ?)",
    )
    .bind(now)
    .bind("2026-06-08T12:00:00Z")
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert subscription with FKs into pre-0011 rows");

    sqlx::query(
        "INSERT INTO subscription_invoices(id, subscription_id, invoice_id, cycle_number, \
         cycle_start_at, cycle_end_at, created_at) \
         VALUES('si1', 'sub1', 'inv1', 1, ?, ?, ?)",
    )
    .bind(now)
    .bind("2026-06-08T12:00:00Z")
    .bind(now)
    .execute(&pool)
    .await
    .expect("subscription_invoices accepts rows");

    // Status CHECK constraint enforced.
    let bad = sqlx::query(
        "INSERT INTO subscriptions(id, license_id, policy_id, product_id, period_days, \
         listed_currency, listed_value, status, started_at, created_at, updated_at) \
         VALUES('sub2', 'lic1', 'pol1', 'p1', 30, 'USD', 2500, 'garbage', ?, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await;
    assert!(
        bad.is_err(),
        "unknown subscription status should be rejected by CHECK"
    );

    // The cycle_number UNIQUE constraint prevents accidental
    // double-billing for the same cycle.
    let dup = sqlx::query(
        "INSERT INTO subscription_invoices(id, subscription_id, invoice_id, cycle_number, \
         cycle_start_at, cycle_end_at, created_at) \
         VALUES('si2', 'sub1', 'inv1', 1, ?, ?, ?)",
    )
    .bind(now)
    .bind("2026-06-08T12:00:00Z")
    .bind(now)
    .execute(&pool)
    .await;
    assert!(
        dup.is_err(),
        "(subscription_id, cycle_number) must be UNIQUE — same cycle twice should fail"
    );

    // FK + integrity invariants.
    assert_db_clean(&pool).await.expect("db clean after 0011");
}

/// Migration 0013 (tier upgrades schema): verifies that adding the
/// new `policies.tier_rank` column + the `tier_changes` table
/// doesn't break existing data, and that the new table accepts rows
/// via FK references back to licenses / policies / invoices created
/// under the prior schema.
#[tokio::test]
async fn migration_0013_adds_tier_upgrades_without_breaking_existing_data() {
    let (pool, _tmp) = make_pool().await;

    // Apply everything before 0013, populate realistic state.
    let total = migration_files().len();
    assert!(total >= 13, "need 13+ migrations to test 0013 in context");
    apply_range(&pool, 0, 12)
        .await
        .expect("apply 0001..=0012");
    seed_realistic_fixtures(&pool)
        .await
        .expect("seed pre-0013 fixtures");

    // Apply 0013.
    apply_range(&pool, 12, 13)
        .await
        .expect("apply 0013_tier_upgrades");

    // The new column exists with NULL default on existing rows
    // (existing operators didn't opt into tier ladders yet).
    let rank: Option<i64> = sqlx::query_scalar(
        "SELECT tier_rank FROM policies WHERE id = 'pol1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        rank, None,
        "existing policies must default to NULL tier_rank (out of any ladder)"
    );

    // The new tier_changes table accepts a row referencing the
    // pre-existing fixture license + policy + invoice.
    let now = "2026-05-08T12:00:00Z";
    sqlx::query(
        "INSERT INTO tier_changes(id, license_id, from_policy_id, to_policy_id, \
         direction, listed_currency, proration_charge_value, invoice_id, \
         effective_at, actor, reason, created_at) \
         VALUES('tc1', 'lic1', 'pol1', 'pol1', 'upgrade', 'USD', 3333, \
                'inv1', ?, 'buyer', 'test upgrade', ?)",
    )
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .expect("tier_changes accepts row with FKs into pre-0013 fixture rows");

    // CHECK constraints enforced: bad direction value rejected.
    let bad_direction = sqlx::query(
        "INSERT INTO tier_changes(id, license_id, from_policy_id, to_policy_id, \
         direction, listed_currency, effective_at, actor, created_at) \
         VALUES('tc2', 'lic1', 'pol1', 'pol1', 'sideways', 'USD', ?, 'buyer', ?)",
    )
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await;
    assert!(
        bad_direction.is_err(),
        "tier_changes.direction must be 'upgrade' or 'downgrade'"
    );

    // CHECK enforced: bad actor value rejected.
    let bad_actor = sqlx::query(
        "INSERT INTO tier_changes(id, license_id, from_policy_id, to_policy_id, \
         direction, listed_currency, effective_at, actor, created_at) \
         VALUES('tc3', 'lic1', 'pol1', 'pol1', 'upgrade', 'USD', ?, 'system', ?)",
    )
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await;
    assert!(
        bad_actor.is_err(),
        "tier_changes.actor must be 'buyer' or 'admin'"
    );

    // CHECK enforced: negative proration value rejected (operator
    // typo or buggy quote logic should fail loudly, not silently
    // store a refund-shaped row in an upgrade-shaped table).
    let bad_proration = sqlx::query(
        "INSERT INTO tier_changes(id, license_id, from_policy_id, to_policy_id, \
         direction, listed_currency, proration_charge_value, effective_at, \
         actor, created_at) \
         VALUES('tc4', 'lic1', 'pol1', 'pol1', 'upgrade', 'USD', -100, ?, 'admin', ?)",
    )
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await;
    assert!(
        bad_proration.is_err(),
        "tier_changes.proration_charge_value must be >= 0"
    );

    // FK + integrity invariants overall.
    assert_db_clean(&pool).await.expect("db clean after 0013");
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
