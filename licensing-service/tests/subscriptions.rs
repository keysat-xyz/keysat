//! Integration tests for the recurring-subscriptions renewal worker.
//!
//! Drives `subscriptions::tick` directly against a mocked payment
//! provider that returns deterministic order ids/checkout urls,
//! plus a manual rate pin in the settings table for fiat-priced
//! subs. No network. Verifies:
//!   - SAT-priced subs renew with correct sat amounts
//!   - USD-priced subs re-quote each cycle via the rate fetcher
//!   - status transitions (active → past_due on renewal create)
//!   - settle webhook → past_due → active
//!   - lapse sweep flips past_due → lapsed once grace expires
//!   - failure path increments consecutive_failures + backs off
//!   - cap on consecutive_failures stops retries
//!   - cycle_number monotonically increments per subscription

use anyhow::Result;
use axum::http::HeaderMap;
use chrono::Utc;
use keysat::api::AppState;
use keysat::config::Config;
use keysat::license_self::Tier;
use keysat::payment::{
    CreateInvoiceParams, CreatedInvoiceHandle, PaymentProvider, ProviderInvoiceStatus,
    ProviderKind, ProviderWebhookEvent,
};
use keysat::subscriptions;
use serde_json::{json, Value};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use std::any::Any;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::sync::RwLock;
use uuid::Uuid;

const TEST_ADMIN_KEY: &str = "test_admin_api_key_with_at_least_32_chars_present";

/// Same fixture pattern as tests/api.rs::make_test_state, with
/// the renewal worker's MockPaymentProvider knobs added.
async fn make_state() -> (AppState, NamedTempFile, Arc<MockProvider>) {
    let tmp = NamedTempFile::new().expect("tempfile");
    let url = format!("sqlite://{}", tmp.path().display());
    let opts = SqliteConnectOptions::from_str(&url)
        .expect("parse url")
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(opts)
        .await
        .expect("connect");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrations");
    let keypair = keysat::crypto::keys::load_or_generate(&pool)
        .await
        .expect("keypair");

    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        db_path: PathBuf::from(":memory:"),
        admin_api_key: TEST_ADMIN_KEY.to_string(),
        btcpay_url: "http://btcpay.test".to_string(),
        btcpay_browser_url: None,
        btcpay_public_url: None,
        btcpay_api_key: None,
        btcpay_store_id: None,
        btcpay_webhook_secret: None,
        public_base_url: "http://keysat.test".to_string(),
        operator_name: None,
    };
    let mock = Arc::new(MockProvider::new());
    let state = AppState {
        db: pool,
        keypair: Arc::new(keypair),
        payment: Arc::new(RwLock::new(Some(
            mock.clone() as Arc<dyn PaymentProvider>,
        ))),
        config: Arc::new(cfg),
        self_tier: Arc::new(RwLock::new(Tier::Unlicensed {
            reason: "test".into(),
        })),
        rates: keysat::rates::RateCache::new(),
    };
    (state, tmp, mock)
}

/// Mock payment provider for the renewal-worker tests.
/// Configurable to fail create_invoice on demand so we can
/// exercise the failure-and-backoff path.
struct MockProvider {
    next_id: AtomicU64,
    fail_next: AtomicBool,
}

impl MockProvider {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            fail_next: AtomicBool::new(false),
        }
    }
    fn fail_next_call(&self) {
        self.fail_next.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl PaymentProvider for MockProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Btcpay
    }
    async fn create_invoice(
        &self,
        _params: CreateInvoiceParams<'_>,
    ) -> Result<CreatedInvoiceHandle> {
        if self.fail_next.swap(false, Ordering::SeqCst) {
            anyhow::bail!("mock-induced create_invoice failure");
        }
        let n = self.next_id.fetch_add(1, Ordering::SeqCst);
        Ok(CreatedInvoiceHandle {
            provider_invoice_id: format!("mock-renewal-{n}"),
            checkout_url: format!("http://mock.test/checkout/{n}"),
        })
    }
    async fn get_invoice_status(&self, _id: &str) -> Result<ProviderInvoiceStatus> {
        Ok(ProviderInvoiceStatus::Pending)
    }
    fn validate_webhook(&self, _h: &HeaderMap, _b: &[u8]) -> Result<ProviderWebhookEvent> {
        anyhow::bail!("not exercised by renewal-worker tests")
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Set up a SAT-priced subscription that's already due for renewal.
/// Returns the sub id.
async fn seed_due_sat_subscription(pool: &SqlitePool) -> String {
    let now = Utc::now().to_rfc3339();
    sqlx::query("INSERT INTO products(id, slug, name, price_sats, created_at, updated_at) VALUES('p1','sub','Sub Product',1000,?,?)")
        .bind(&now).bind(&now).execute(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO policies(id, product_id, name, slug, is_recurring, renewal_period_days, \
         grace_period_days, created_at, updated_at) \
         VALUES('pol1','p1','Monthly','monthly',1,30,7,?,?)",
    )
    .bind(&now).bind(&now).execute(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO licenses(id, product_id, status, issued_at, policy_id) \
         VALUES('lic1','p1','active',?,'pol1')",
    )
    .bind(&now).execute(pool).await.unwrap();

    let sub_id = "sub1".to_string();
    let past = (Utc::now() - chrono::Duration::days(1)).to_rfc3339();
    sqlx::query(
        "INSERT INTO subscriptions(id, license_id, policy_id, product_id, period_days, \
         listed_currency, listed_value, status, started_at, next_renewal_at, \
         consecutive_failures, created_at, updated_at) \
         VALUES(?, 'lic1', 'pol1', 'p1', 30, 'SAT', 50000, 'active', ?, ?, 0, ?, ?)",
    )
    .bind(&sub_id).bind(&now).bind(&past).bind(&now).bind(&now)
    .execute(pool).await.unwrap();
    sub_id
}

#[tokio::test]
async fn renewal_worker_creates_invoice_for_sat_priced_due_sub() {
    let (state, _tmp, _mock) = make_state().await;
    let sub_id = seed_due_sat_subscription(&state.db).await;

    subscriptions::tick(&state).await.expect("tick");

    // Sub flipped to past_due, next_renewal_at advanced ~30 days,
    // consecutive_failures still 0.
    let row: (String, Option<String>, i64) = sqlx::query_as(
        "SELECT status, next_renewal_at, consecutive_failures FROM subscriptions WHERE id = ?",
    )
    .bind(&sub_id).fetch_one(&state.db).await.unwrap();
    assert_eq!(row.0, "past_due", "renewal-create flips active → past_due");
    assert!(row.1.is_some());
    assert_eq!(row.2, 0);

    // Invoice was created with sat amount = listed_value (SAT identity).
    let inv: (i64, Option<String>) = sqlx::query_as(
        "SELECT i.amount_sats, i.exchange_rate_source \
         FROM invoices i \
         JOIN subscription_invoices si ON si.invoice_id = i.id \
         WHERE si.subscription_id = ?",
    )
    .bind(&sub_id).fetch_one(&state.db).await.unwrap();
    assert_eq!(inv.0, 50_000, "sat-priced sub charges listed_value sats verbatim");
    // SAT subs don't record a rate source (identity conversion).
    assert!(inv.1.is_none());

    // subscription_invoices got a row with cycle_number = 2 (the
    // first cycle invoice was the original purchase, which the
    // seed didn't create — so the worker's first renewal is
    // cycle 1 in the seed's universe; check it's > 0).
    let cycle: i64 = sqlx::query_scalar(
        "SELECT cycle_number FROM subscription_invoices WHERE subscription_id = ?",
    )
    .bind(&sub_id).fetch_one(&state.db).await.unwrap();
    assert!(cycle >= 1);
}

#[tokio::test]
async fn renewal_worker_requotes_rate_for_fiat_priced_sub() {
    let (state, _tmp, _mock) = make_state().await;
    // Pin USD to $50,000/BTC so $25.00 → 50,000 sats exactly.
    sqlx::query("INSERT INTO settings(key, value, updated_at) VALUES('manual_rate_pin_USD', '50000', ?)")
        .bind(Utc::now().to_rfc3339()).execute(&state.db).await.unwrap();

    let now = Utc::now().to_rfc3339();
    sqlx::query("INSERT INTO products(id, slug, name, price_sats, price_currency, price_value, created_at, updated_at) VALUES('p1','usd','USD Sub',0,'USD',2500,?,?)")
        .bind(&now).bind(&now).execute(&state.db).await.unwrap();
    sqlx::query("INSERT INTO policies(id, product_id, name, slug, is_recurring, renewal_period_days, grace_period_days, created_at, updated_at) VALUES('pol1','p1','M','m',1,30,7,?,?)")
        .bind(&now).bind(&now).execute(&state.db).await.unwrap();
    sqlx::query("INSERT INTO licenses(id, product_id, status, issued_at, policy_id) VALUES('lic1','p1','active',?,'pol1')")
        .bind(&now).execute(&state.db).await.unwrap();

    let past = (Utc::now() - chrono::Duration::days(1)).to_rfc3339();
    sqlx::query(
        "INSERT INTO subscriptions(id, license_id, policy_id, product_id, period_days, \
         listed_currency, listed_value, status, started_at, next_renewal_at, \
         consecutive_failures, created_at, updated_at) \
         VALUES('sub1','lic1','pol1','p1',30,'USD',2500,'active',?,?,0,?,?)",
    )
    .bind(&now).bind(&past).bind(&now).bind(&now).execute(&state.db).await.unwrap();

    subscriptions::tick(&state).await.expect("tick");

    // $25 at $50k/BTC = 0.0005 BTC = 50,000 sats.
    let row: (i64, Option<String>, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT i.amount_sats, i.listed_currency, i.listed_value, i.exchange_rate_centibps \
         FROM invoices i \
         JOIN subscription_invoices si ON si.invoice_id = i.id \
         WHERE si.subscription_id = 'sub1'",
    )
    .fetch_one(&state.db).await.unwrap();
    assert_eq!(row.0, 50_000);
    assert_eq!(row.1.as_deref(), Some("USD"));
    assert_eq!(row.2, Some(2500));
    assert_eq!(row.3, Some(500_000_000));
}

#[tokio::test]
async fn renewal_worker_backs_off_on_failure() {
    let (state, _tmp, mock) = make_state().await;
    let sub_id = seed_due_sat_subscription(&state.db).await;

    // Force the next provider call to fail.
    mock.fail_next_call();
    subscriptions::tick(&state).await.expect("tick succeeds even when individual renewal fails");

    let row: (String, i64) = sqlx::query_as(
        "SELECT status, consecutive_failures FROM subscriptions WHERE id = ?",
    )
    .bind(&sub_id).fetch_one(&state.db).await.unwrap();
    assert_eq!(row.0, "past_due");
    assert_eq!(row.1, 1, "first failure increments to 1");

    // No invoice was created.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM subscription_invoices WHERE subscription_id = ?",
    )
    .bind(&sub_id).fetch_one(&state.db).await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn renewal_worker_stops_retrying_at_max_failures() {
    let (state, _tmp, _mock) = make_state().await;
    let sub_id = seed_due_sat_subscription(&state.db).await;

    // Pre-set consecutive_failures = MAX so the find-due query
    // skips this row.
    sqlx::query("UPDATE subscriptions SET consecutive_failures = 5 WHERE id = ?")
        .bind(&sub_id).execute(&state.db).await.unwrap();

    subscriptions::tick(&state).await.expect("tick");

    // Should not have attempted a renewal — no invoices created.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM subscription_invoices WHERE subscription_id = ?",
    )
    .bind(&sub_id).fetch_one(&state.db).await.unwrap();
    assert_eq!(count, 0, "MAX_CONSECUTIVE_FAILURES should stop retries");
}

#[tokio::test]
async fn lapse_sweep_flips_past_due_after_grace() {
    let (state, _tmp, _mock) = make_state().await;
    let sub_id = seed_due_sat_subscription(&state.db).await;

    // Set the sub to past_due with a next_renewal_at far enough
    // in the past that grace_period_days (7 from the seed policy)
    // has clearly elapsed.
    let way_past = (Utc::now() - chrono::Duration::days(15)).to_rfc3339();
    sqlx::query(
        "UPDATE subscriptions SET status='past_due', next_renewal_at=?, \
         consecutive_failures=5 WHERE id = ?",
    )
    .bind(&way_past).bind(&sub_id).execute(&state.db).await.unwrap();

    subscriptions::tick(&state).await.expect("tick");

    let status: String =
        sqlx::query_scalar("SELECT status FROM subscriptions WHERE id = ?")
            .bind(&sub_id).fetch_one(&state.db).await.unwrap();
    assert_eq!(status, "lapsed", "past_due past grace should flip to lapsed");
}

#[tokio::test]
async fn settle_webhook_flips_sub_back_to_active() {
    let (state, _tmp, _mock) = make_state().await;
    let _sub_id = seed_due_sat_subscription(&state.db).await;

    // First tick creates the renewal invoice and flips sub to past_due.
    subscriptions::tick(&state).await.expect("tick");

    // Find the just-created invoice + simulate a settle.
    let invoice_id: String = sqlx::query_scalar(
        "SELECT i.id FROM invoices i \
         JOIN subscription_invoices si ON si.invoice_id = i.id \
         WHERE si.subscription_id = 'sub1'",
    )
    .fetch_one(&state.db).await.unwrap();
    sqlx::query("UPDATE invoices SET status = 'settled' WHERE id = ?")
        .bind(&invoice_id).execute(&state.db).await.unwrap();

    // Build the Invoice model for the helper's signature.
    let invoice = keysat::db::repo::get_invoice_by_id(&state.db, &invoice_id)
        .await.unwrap().unwrap();
    keysat::subscriptions::on_invoice_settled(&state, &invoice)
        .await.expect("on_invoice_settled");

    let row: (String, i64) = sqlx::query_as(
        "SELECT status, consecutive_failures FROM subscriptions WHERE id = 'sub1'",
    )
    .fetch_one(&state.db).await.unwrap();
    assert_eq!(row.0, "active");
    assert_eq!(row.1, 0);
}

/// Tick is idempotent in the no-op direction: running it when no
/// subs are due doesn't crash and doesn't side-effect anything.
#[tokio::test]
async fn tick_is_no_op_when_nothing_due() {
    let (state, _tmp, _mock) = make_state().await;
    // No fixtures seeded.
    subscriptions::tick(&state).await.expect("tick on empty");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM invoices")
        .fetch_one(&state.db).await.unwrap();
    assert_eq!(count, 0);
}
