//! Integration tests for the outbound-webhook delivery worker.
//!
//! Companion to `tests/api.rs`'s DLQ test, which only exercised the
//! admin surface against an SQL fixture. This file drives the worker
//! itself (`webhooks::tick`) against a real HTTP receiver, watching
//! the retry-then-dead-letter behavior empirically rather than
//! trusting the SQL fixture.

use axum::{http::StatusCode, routing::any, Router};
use chrono::Utc;
use keysat::api::AppState;
use keysat::config::Config;
use keysat::license_self::Tier;
use keysat::{crypto, webhooks};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::net::TcpListener;
use tokio::sync::RwLock;

/// Minimum-viable AppState for a worker test. The worker only touches
/// `state.db` for queue queries — nothing else matters here.
async fn make_state() -> (AppState, NamedTempFile) {
    let tmp = NamedTempFile::new().expect("tempfile");
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
        .expect("connect sqlite");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("apply migrations");
    let keypair = crypto::keys::load_or_generate(&pool)
        .await
        .expect("load_or_generate keypair");

    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        db_path: PathBuf::from(":memory:"),
        admin_api_key: "x".repeat(32),
        btcpay_url: "http://btcpay.test".to_string(),
        btcpay_browser_url: None,
        btcpay_public_url: None,
        btcpay_api_key: None,
        btcpay_store_id: None,
        btcpay_webhook_secret: None,
        public_base_url: "http://keysat.test".to_string(),
        operator_name: None,
    };
    let state = AppState {
        db: pool,
        keypair: Arc::new(keypair),
        payment: Arc::new(RwLock::new(None)),
        config: Arc::new(cfg),
        self_tier: Arc::new(RwLock::new(Tier::Unlicensed {
            reason: "test".into(),
        })),
        rates: keysat::rates::RateCache::new(),
    };
    (state, tmp)
}

/// Spawn a tiny axum server on a random port that returns 500 for every
/// request. Returns the URL the webhook endpoint should be configured
/// with. Server runs for the lifetime of the test process; tokio
/// reclaims it on test completion.
async fn spawn_500_receiver() -> String {
    let app = Router::new().route(
        "/",
        any(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    format!("http://{addr}/")
}

/// Insert a webhook endpoint + a single ready-to-deliver row.
async fn seed_endpoint_and_delivery(
    pool: &SqlitePool,
    url: &str,
    initial_attempts: i64,
) -> String {
    let now = Utc::now().to_rfc3339();
    let endpoint_id = "ep-test";
    sqlx::query(
        "INSERT INTO webhook_endpoints(id, url, secret, event_types, active, \
         description, created_at, updated_at) \
         VALUES(?, ?, '0123456789abcdef0123456789abcdef', '[\"*\"]', 1, '', ?, ?)",
    )
    .bind(endpoint_id)
    .bind(url)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();

    let delivery_id = "del-test";
    sqlx::query(
        "INSERT INTO webhook_deliveries(id, endpoint_id, event_type, \
         payload_json, attempt_count, next_attempt_at, created_at) \
         VALUES(?, ?, 'license.issued', '{\"data\":\"x\"}', ?, ?, ?)",
    )
    .bind(delivery_id)
    .bind(endpoint_id)
    .bind(initial_attempts)
    .bind(&now) // due now
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();

    delivery_id.to_string()
}

/// First-attempt failure: worker POSTs, receiver 500s, worker marks
/// the row as a failure, schedules a retry. Verifies attempt_count
/// went 0→1, next_attempt_at is in the future, last_status_code is
/// the 500, last_error is populated.
#[tokio::test]
async fn worker_marks_failure_and_schedules_retry_on_500() {
    let (state, _tmp) = make_state().await;
    let url = spawn_500_receiver().await;
    let delivery_id = seed_endpoint_and_delivery(&state.db, &url, 0).await;

    webhooks::tick(&state).await.expect("tick");

    let row: (i64, Option<String>, Option<i64>, Option<String>, Option<String>) =
        sqlx::query_as(
            "SELECT attempt_count, next_attempt_at, last_status_code, \
             last_error, delivered_at FROM webhook_deliveries WHERE id = ?",
        )
        .bind(&delivery_id)
        .fetch_one(&state.db)
        .await
        .unwrap();

    assert_eq!(row.0, 1, "attempt_count should be 1 after one failed tick");
    assert!(
        row.1.is_some(),
        "next_attempt_at should be scheduled for retry"
    );
    assert_eq!(
        row.2,
        Some(500),
        "last_status_code should record the receiver's 500"
    );
    assert!(
        row.3.as_deref().unwrap_or("").contains("non-2xx"),
        "last_error should describe the failure: {:?}",
        row.3
    );
    assert!(row.4.is_none(), "delivered_at must remain NULL on failure");
}

/// Crossing the dead-letter boundary: with attempt_count already at 9,
/// one more failed tick takes it to 10, and the worker MUST NOT
/// schedule another retry — it sets next_attempt_at = NULL. This is
/// the row that the new admin DLQ surface (`?status=failed`) picks up.
#[tokio::test]
async fn worker_dead_letters_after_max_attempts() {
    let (state, _tmp) = make_state().await;
    let url = spawn_500_receiver().await;
    let delivery_id = seed_endpoint_and_delivery(&state.db, &url, 9).await;

    webhooks::tick(&state).await.expect("tick");

    let row: (i64, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT attempt_count, next_attempt_at, delivered_at \
         FROM webhook_deliveries WHERE id = ?",
    )
    .bind(&delivery_id)
    .fetch_one(&state.db)
    .await
    .unwrap();

    assert_eq!(row.0, 10, "attempt_count should reach the cap");
    assert!(
        row.1.is_none(),
        "next_attempt_at MUST be NULL — this is the DLQ signal: {:?}",
        row.1
    );
    assert!(row.2.is_none(), "delivered_at must remain NULL");

    // Confirm the dead-lettered row also shows up in the admin DLQ
    // filter — the SQL predicate the admin endpoint uses
    // (delivered_at IS NULL AND next_attempt_at IS NULL AND
    // attempt_count > 0) must match this row.
    let dlq_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM webhook_deliveries \
         WHERE delivered_at IS NULL AND next_attempt_at IS NULL AND attempt_count > 0",
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(
        dlq_count, 1,
        "the dead-lettered row must satisfy the admin DLQ predicate"
    );
}

/// 2xx response → success. The worker stamps `delivered_at` with the
/// current time, leaves `next_attempt_at` NULL, and records the status
/// code. This is the happy path — implicitly tested already via
/// production usage but pinned here for completeness alongside the
/// failure cases above.
#[tokio::test]
async fn worker_marks_success_on_2xx() {
    let (state, _tmp) = make_state().await;

    // Receiver that always returns 200.
    let app = Router::new().route("/", any(|| async { StatusCode::OK }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    let url = format!("http://{addr}/");

    let delivery_id = seed_endpoint_and_delivery(&state.db, &url, 0).await;

    webhooks::tick(&state).await.expect("tick");

    let row: (i64, Option<String>, Option<i64>, Option<String>) = sqlx::query_as(
        "SELECT attempt_count, next_attempt_at, last_status_code, delivered_at \
         FROM webhook_deliveries WHERE id = ?",
    )
    .bind(&delivery_id)
    .fetch_one(&state.db)
    .await
    .unwrap();

    assert_eq!(row.0, 1);
    assert!(row.1.is_none(), "next_attempt_at should be NULL on success");
    assert_eq!(row.2, Some(200));
    assert!(row.3.is_some(), "delivered_at should be stamped on success");
}
