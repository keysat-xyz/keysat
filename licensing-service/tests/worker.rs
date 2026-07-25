//! Integration tests for the background workers.
//!
//! Companion to `tests/api.rs`'s DLQ test, which only exercised the
//! admin surface against an SQL fixture. This file drives the workers
//! themselves against a real HTTP server, watching behavior empirically
//! rather than trusting a SQL fixture:
//!
//! - `webhooks::tick` — the outbound delivery worker's
//!   retry-then-dead-letter ladder.
//! - `reconcile::tick` — the provider liveness probe, whose whole reason to
//!   exist is that it runs when nothing else does, so it can only be proven
//!   from outside, against an idle daemon.

use axum::{http::StatusCode, routing::any, Router};
use chrono::Utc;
use keysat::api::AppState;
use keysat::config::Config;
use keysat::db::repo;
use keysat::license_self::Tier;
use keysat::payment::health::{ProviderAuthHealth, PROBE_INTERVAL};
use keysat::{crypto, reconcile, webhooks};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
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
        sandbox_mode: false,
    };
    let state = AppState {
        db: pool,
        keypair: Arc::new(keypair),
        payment: Arc::new(RwLock::new(None)),
        provider_override: None,
        config: Arc::new(cfg),
        self_tier: Arc::new(RwLock::new(Tier::Unlicensed {
            reason: "test".into(),
        })),
        rates: keysat::rates::RateCache::new(),
        provider_health: Default::default(),
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

// =======================================================================
// The provider liveness probe (`reconcile::tick`).
//
// Every test below drives a REAL provider client — built by
// `payment::build_provider` from a real `payment_providers` row — against a
// local stub, and reads the shared health map back out. Nothing here
// hand-builds a health value or calls the tracker in place of the probe: the
// two things that can go wrong (the probe not running at all, and the probe
// running under the wrong label) are both invisible to a test that stops
// short of the client.
// =======================================================================

/// One request the stub saw, reduced to the two things a probe can get wrong
/// without any test noticing.
#[derive(Clone, Debug)]
struct SeenRequest {
    /// Path plus query, exactly as it arrived.
    uri: String,
    authorization: Option<String>,
}

/// Throwaway HTTP server on an ephemeral port answering every method and path
/// with one fixed status, **and recording what it was asked**. Same shape as
/// `spawn_500_receiver` above, the established local-stub pattern in this repo,
/// with the status parameterized because these tests need 401 / 403 / 200.
///
/// **The recording is load-bearing.** A stub that answers everything the same
/// way cannot tell a correct probe from a probe pointed at the wrong URL or
/// sending no credential, and both of those fail *silently* in opposite
/// directions: a wrong path 404s, and a 404 is inert in the alert rule, so the
/// probe would run forever, learn nothing and look healthy — total silent
/// failure of the thing this step exists for. A missing `Authorization` header
/// 401s, so a perfectly good key would alert `auth_dead` after three probes.
async fn spawn_status_receiver(status: StatusCode) -> (String, Arc<Mutex<Vec<SeenRequest>>>) {
    let seen: Arc<Mutex<Vec<SeenRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let app = Router::new().fallback(move |req: axum::extract::Request| {
        let recorder = recorder.clone();
        async move {
            recorder.lock().expect("not poisoned").push(SeenRequest {
                uri: req
                    .uri()
                    .path_and_query()
                    .map(|pq| pq.as_str().to_string())
                    .unwrap_or_default(),
                authorization: req
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string),
            });
            status
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}"), seen)
}

fn requests(seen: &Arc<Mutex<Vec<SeenRequest>>>) -> Vec<SeenRequest> {
    seen.lock().expect("not poisoned").clone()
}

/// Seed one `payment_providers` row on the default merchant profile, pointing
/// at `base_url`. Returns the row id — the key its health entry lives under.
async fn seed_provider(state: &AppState, kind: &str, base_url: &str) -> String {
    let profile = repo::get_default_merchant_profile(&state.db)
        .await
        .expect("query default profile")
        .expect("migration 0020 auto-creates a default merchant profile");
    let id = format!("prov-{kind}");
    repo::create_payment_provider(
        &state.db,
        &id,
        &profile.id,
        kind,
        "Probe test provider",
        "test-api-key",
        base_url,
        None,
        Some("deadbeef"),
        // BTCPay needs a store id to build at all; Zaprite ignores it.
        Some("store-1"),
        &Utc::now().to_rfc3339(),
    )
    .await
    .expect("seed payment provider");
    id
}

fn health_of(state: &AppState, id: &str) -> ProviderAuthHealth {
    state
        .provider_health
        .read()
        .expect("not poisoned")
        .get(id)
        .cloned()
        .unwrap_or_default()
}

/// Put a running `auth_dead` (2 failures) and an already-**alerting**
/// `cannot_sell` (3 failures, oldest well past the 10-minute arm) into the
/// tracker, plus a standing permissions observation.
///
/// Driven through `record_failure`, the rule's own entry point, and with a
/// sell label — so what the probe does to these is measured against a state a
/// real revoked-then-scope-broken key would produce, not a struct literal.
///
/// Both streaks are seeded on purpose: against an empty one, "the probe left
/// it alone" and "the probe reset it" are indistinguishable.
fn seed_running_streaks(state: &AppState, id: &str, sell_label: &str, now: SystemTime) {
    let ago = |secs: u64| now - Duration::from_secs(secs);
    let mut map = state.provider_health.write().expect("not poisoned");
    let entry = map.entry(id.to_string()).or_default();
    entry.record_failure(sell_label, 401, ago(1_400));
    entry.record_failure(sell_label, 401, ago(1_300));
    entry.record_failure(sell_label, 403, ago(1_200));
    entry.record_failure(sell_label, 403, ago(1_100));
    entry.record_failure(sell_label, 403, ago(1_000));
    entry.probe_403_since = Some(ago(1_500));

    assert_eq!(entry.auth_dead.consecutive, 2);
    assert_eq!(entry.cannot_sell.consecutive, 3);
    assert!(
        entry.cannot_sell.is_alerting(now),
        "the seeded cannot_sell must already be alerting, or the assertions \
         about the probe not silencing it prove nothing"
    );
}

/// **The whole point of the step.** `reconcile::tick` early-returns the moment
/// `list_pending_invoices` comes back empty, and `subscriptions::tick` does the
/// same — so between sales the daemon makes no provider calls at all and a
/// revoked key stays invisible until a buyer reaches checkout, which is exactly
/// the moment the alert exists to pre-empt.
///
/// Asserted from the outside on an idle database rather than by reading the
/// call site: moving the probe below that early return would restore the blind
/// spot silently, and this is the test that would notice.
#[tokio::test]
async fn the_probe_fires_on_an_idle_daemon_with_no_pending_invoices() {
    let (state, _tmp) = make_state().await;
    let (base, seen) = spawn_status_receiver(StatusCode::UNAUTHORIZED).await;
    let id = seed_provider(&state, "btcpay", &base).await;

    // Idle: nothing for the reconcile sweep to do, so everything past the
    // early return is dead code on this tick.
    assert!(
        repo::list_pending_invoices(&state.db, 72)
            .await
            .expect("list pending")
            .is_empty(),
        "this test is only meaningful against an idle daemon"
    );

    reconcile::tick(&state).await.expect("tick");

    let h = health_of(&state, &id);
    assert_eq!(
        h.auth_dead.consecutive, 1,
        "the probe must reach the provider even with nothing pending"
    );
    assert_eq!(h.last_status, Some(401));
    assert!(h.last_probe_at.is_some());

    // **What the probe actually asked, not merely that it asked something.**
    // Both halves fail silently if they drift: a wrong path 404s and a 404 is
    // inert, so the probe would look healthy while learning nothing; a missing
    // credential 401s, so a good key would alert after three probes.
    let reqs = requests(&seen);
    assert_eq!(reqs.len(), 1, "one probe, one request");
    assert_eq!(
        reqs[0].uri, "/api/v1/stores/store-1",
        "BTCPay probe must hit GET /api/v1/stores/{{storeId}}"
    );
    assert_eq!(
        reqs[0].authorization.as_deref(),
        Some("token test-api-key"),
        "BTCPay authenticates with `token <key>`; an unauthenticated probe \
         401s and alerts on a healthy key"
    );
}

/// The same for Zaprite, because the two clients reach the sink by different
/// routes: BTCPay through a brand-new `&self` method, Zaprite through a
/// relabeled wrapper over `ping`'s HTTP call.
#[tokio::test]
async fn the_zaprite_probe_also_fires_on_an_idle_daemon() {
    let (state, _tmp) = make_state().await;
    let (base, seen) = spawn_status_receiver(StatusCode::UNAUTHORIZED).await;
    let id = seed_provider(&state, "zaprite", &base).await;

    reconcile::tick(&state).await.expect("tick");

    assert_eq!(health_of(&state, &id).auth_dead.consecutive, 1);

    let reqs = requests(&seen);
    assert_eq!(reqs.len(), 1, "one probe, one request");
    assert_eq!(
        reqs[0].uri, "/v1/orders?limit=1",
        "the Zaprite probe must issue `ping`'s authenticated read-only call"
    );
    assert_eq!(
        reqs[0].authorization.as_deref(),
        Some("Bearer test-api-key"),
        "Zaprite authenticates with a bearer token"
    );
}

/// A probe **401** counts toward `auth_dead` like any other 401 — the probe
/// exists precisely to find a revoked key before a buyer does.
///
/// Against a **running** streak, so this says "increments" and not merely
/// "sets"; and with `cannot_sell` running too, so it also says the 401 did not
/// leak into the other streak or reset it.
#[tokio::test]
async fn a_probe_401_extends_auth_dead_and_leaves_cannot_sell_alone() {
    let (state, _tmp) = make_state().await;
    let (base, _seen) = spawn_status_receiver(StatusCode::UNAUTHORIZED).await;
    let id = seed_provider(&state, "btcpay", &base).await;
    let now = SystemTime::now();
    seed_running_streaks(&state, &id, "btcpay.create_invoice", now);
    let before = health_of(&state, &id);

    reconcile::tick(&state).await.expect("tick");

    let h = health_of(&state, &id);
    assert_eq!(h.auth_dead.consecutive, 3, "the probe's 401 is the third");
    assert_eq!(
        h.auth_dead.first_failure_at, before.auth_dead.first_failure_at,
        "extending a run must not restamp when it began"
    );
    assert!(h.auth_dead.is_alerting(now), "three, spanning past the window");
    assert_eq!(h.cannot_sell, before.cannot_sell);
    assert_eq!(h.last_status, Some(401));
}

/// **The label, proven end to end — the counting direction.**
///
/// The probe deliberately touches a broader permission than a checkout does
/// (BTCPay's `canviewstoresettings` versus `cancreateinvoice`), so a key that
/// sells perfectly can 403 here forever. Its 403 must therefore be a
/// non-alerting permissions observation and must not touch either streak.
///
/// This is what fails if the probe is ever wired to a counting label — the
/// Zaprite half is a one-word edit away, `ping()` instead of `probe_auth()`,
/// and the compiler cannot see the difference because both labels are
/// `&'static str`.
#[tokio::test]
async fn a_probe_403_does_not_raise_cannot_sell_and_disturbs_neither_streak() {
    for (kind, sell_label) in [
        ("btcpay", "btcpay.create_invoice"),
        ("zaprite", "zaprite.create_order"),
    ] {
        let (state, _tmp) = make_state().await;
        let (base, _seen) = spawn_status_receiver(StatusCode::FORBIDDEN).await;
        let id = seed_provider(&state, kind, &base).await;
        let now = SystemTime::now();
        seed_running_streaks(&state, &id, sell_label, now);
        let before = health_of(&state, &id);

        reconcile::tick(&state).await.expect("tick");

        let h = health_of(&state, &id);
        // FIRST, because every other assertion here is "nothing moved" — and
        // `seed_running_streaks` already leaves `last_status = Some(403)`, so
        // a tick that never probed at all would satisfy all of them. This is
        // the assertion that separates "the probe correctly did nothing" from
        // "the probe never ran": it is the only test in this file that moving
        // `probe_providers` below the early return did NOT kill.
        assert!(
            h.last_probe_at.is_some(),
            "{kind}: the probe must actually have run"
        );
        assert_eq!(
            h.cannot_sell, before.cannot_sell,
            "{kind}: a probe 403 must neither count nor reset the sell streak"
        );
        assert_eq!(
            h.auth_dead, before.auth_dead,
            "{kind}: a 403 is a permissions statement, not an authentication one"
        );
        assert_eq!(
            h.probe_403_since, before.probe_403_since,
            "{kind}: probe_403_since holds the FIRST such timestamp"
        );
        assert_eq!(h.last_status, Some(403), "{kind}: it is still observed");
    }
}

/// **The label, proven end to end — the clearing direction.**
///
/// A probe **success** proves the credential authenticates, so it clears
/// `auth_dead` and the permissions observation. It proves nothing whatever
/// about `cancreateinvoice`, so an already-alerting `cannot_sell` must still be
/// alerting afterwards.
///
/// This is the half the counter split exists for: with a 15-minute probe
/// cadence against a 10-minute alert window, a probe success that cleared
/// `cannot_sell` would mean a key that authenticates but cannot sell alerts
/// only if three checkouts 403 inside the ten minutes after a probe — on a
/// quiet instance, possibly never.
#[tokio::test]
async fn a_probe_success_clears_auth_dead_but_leaves_cannot_sell_alerting() {
    for (kind, sell_label) in [
        ("btcpay", "btcpay.create_invoice"),
        ("zaprite", "zaprite.create_order"),
    ] {
        let (state, _tmp) = make_state().await;
        let (base, _seen) = spawn_status_receiver(StatusCode::OK).await;
        let id = seed_provider(&state, kind, &base).await;
        let now = SystemTime::now();
        seed_running_streaks(&state, &id, sell_label, now);
        let before = health_of(&state, &id);

        reconcile::tick(&state).await.expect("tick");

        let h = health_of(&state, &id);
        assert_eq!(
            h.auth_dead,
            Default::default(),
            "{kind}: a 2xx on any label proves the key still authenticates"
        );
        assert_eq!(
            h.cannot_sell, before.cannot_sell,
            "{kind}: the probe created no invoice, so it clears nothing here"
        );
        assert!(
            h.cannot_sell.is_alerting(now),
            "{kind}: and the alert must still be standing"
        );
        assert_eq!(
            h.probe_403_since, None,
            "{kind}: a probe success is the only evidence the broader \
             permission came back"
        );
        assert!(h.last_success_at.is_some(), "{kind}");
        assert_eq!(h.last_status, None, "{kind}");
    }
}

/// The throttle, across ticks that arrive as fast as the reconcile loop does.
///
/// Driven on the injected clock rather than by sleeping: `tokio`'s `full`
/// feature excludes `test-util`, so `time::pause` is unavailable and fifteen
/// real minutes is not a test.
#[tokio::test]
async fn the_probe_throttle_holds_across_rapid_ticks() {
    let (state, _tmp) = make_state().await;
    let (base, _seen) = spawn_status_receiver(StatusCode::UNAUTHORIZED).await;
    let id = seed_provider(&state, "btcpay", &base).await;
    let t0 = SystemTime::now();

    reconcile::tick_at(&state, t0).await.expect("tick");
    assert_eq!(health_of(&state, &id).auth_dead.consecutive, 1);

    // The reconcile loop ticks every 60 seconds; none of these may probe.
    for secs in [1, 60, 120, 600, PROBE_INTERVAL.as_secs() - 1] {
        reconcile::tick_at(&state, t0 + Duration::from_secs(secs))
            .await
            .expect("tick");
        assert_eq!(
            health_of(&state, &id).auth_dead.consecutive,
            1,
            "a second probe fired {secs}s into a {}s window",
            PROBE_INTERVAL.as_secs()
        );
    }

    reconcile::tick_at(&state, t0 + PROBE_INTERVAL)
        .await
        .expect("tick");
    assert_eq!(
        health_of(&state, &id).auth_dead.consecutive,
        2,
        "the window has elapsed, so the probe must run again"
    );
    // ...and the new window is measured from the probe that just ran.
    reconcile::tick_at(&state, t0 + PROBE_INTERVAL + Duration::from_secs(60))
        .await
        .expect("tick");
    assert_eq!(health_of(&state, &id).auth_dead.consecutive, 2);
}

/// Two connected providers are throttled independently and both get probed.
/// A single shared window would leave whichever one lost the race unprobed
/// for as long as the other kept claiming it.
#[tokio::test]
async fn every_connected_provider_is_probed() {
    let (state, _tmp) = make_state().await;
    let (base, _seen) = spawn_status_receiver(StatusCode::UNAUTHORIZED).await;
    let btcpay = seed_provider(&state, "btcpay", &base).await;
    let zaprite = seed_provider(&state, "zaprite", &base).await;

    reconcile::tick(&state).await.expect("tick");

    assert_eq!(health_of(&state, &btcpay).auth_dead.consecutive, 1);
    assert_eq!(health_of(&state, &zaprite).auth_dead.consecutive, 1);
}

/// A provider that cannot be reached at all records nothing — a transport
/// failure never yields an HTTP status, and an unreachable provider is not
/// evidence about its API key in either direction. The tick must still
/// succeed, and the slot must still be spent, so an unreachable provider is
/// retried on the throttled cadence and not on every 60-second tick.
#[tokio::test]
async fn an_unreachable_provider_neither_fails_the_tick_nor_records() {
    let (state, _tmp) = make_state().await;
    // Nothing listens on port 1 — binding it needs root.
    let id = seed_provider(&state, "btcpay", "http://127.0.0.1:1").await;

    reconcile::tick(&state).await.expect("tick must not fail");

    let h = health_of(&state, &id);
    assert_eq!(h.auth_dead, Default::default());
    assert_eq!(h.last_status, None);
    assert_eq!(h.last_success_at, None);
    assert!(h.last_probe_at.is_some(), "the attempt still spends the slot");
}

// -----------------------------------------------------------------
// Error containment. The probe runs ahead of the invoice sweep, so every
// way of failing to ask its question has to be swallowed rather than
// propagated — otherwise a health check takes down money reconciliation.
// Both arms below had no test until a reviewer replaced each body with a
// `panic!` and the suite stayed green.
// -----------------------------------------------------------------

/// The provider enumeration itself failing must not fail the tick. It is not
/// evidence about anyone's API key, and reconciliation does not depend on it.
#[tokio::test]
async fn a_provider_list_failure_does_not_fail_the_tick() {
    let (state, _tmp) = make_state().await;
    let (base, seen) = spawn_status_receiver(StatusCode::UNAUTHORIZED).await;
    seed_provider(&state, "btcpay", &base).await;

    // Break the enumeration under the probe's feet.
    sqlx::query("DROP TABLE payment_providers")
        .execute(&state.db)
        .await
        .expect("drop payment_providers");

    reconcile::tick(&state)
        .await
        .expect("a failed provider list must not fail the tick");

    assert!(
        requests(&seen).is_empty(),
        "nothing could be enumerated, so nothing may be probed"
    );
    assert!(
        state.provider_health.read().expect("not poisoned").is_empty(),
        "and no slot may be claimed for a provider that was never listed"
    );
}

/// One unbuildable row must not cost the others their probe.
///
/// The row here carries a `kind` neither `ProviderKind::parse` nor
/// `payment::build_provider` recognizes — the shape a future migration, or a
/// hand-edited database, could produce. It is seeded with the **earliest**
/// `connected_at` so the probe loop reaches it first: a `return` where the code
/// says `continue` would then skip every healthy provider behind it.
///
/// The obvious alternative — a BTCPay row with a NULL `store_id`, which
/// `build_provider` explicitly refuses — does **not** work, and finding out why
/// is worth recording: `db/repo.rs::row_to_payment_provider` reads that column
/// with `try_get::<String>(..).ok()`, and sqlx's SQLite decoder turns a NULL
/// `TEXT` into `Some("")` rather than `None`. So that guard cannot fire for any
/// row actually read from the database; BTCPay is built with an empty store id
/// and issues requests against `/api/v1/stores/`. Pre-existing, unrelated to
/// the probe, and outside this change — but do not write a test that assumes
/// otherwise, as this one first did.
#[tokio::test]
async fn a_provider_that_cannot_be_built_does_not_stop_the_others() {
    let (state, _tmp) = make_state().await;
    let (base, seen) = spawn_status_receiver(StatusCode::UNAUTHORIZED).await;
    let profile = repo::get_default_merchant_profile(&state.db)
        .await
        .expect("query default profile")
        .expect("default profile");

    // `payment_providers` has `CHECK (kind IN ('btcpay','zaprite'))`, so the
    // unrecognized kind goes in behind the pragma that suspends CHECKs — on a
    // single pinned connection, since the pragma is per-connection and the pool
    // holds more than one.
    let mut conn = state.db.acquire().await.expect("acquire");
    sqlx::query("PRAGMA ignore_check_constraints = ON")
        .execute(&mut *conn)
        .await
        .expect("pragma on");
    sqlx::query(
        "INSERT INTO payment_providers(id, merchant_profile_id, kind, label, \
         api_key, base_url, webhook_id, webhook_secret, store_id, \
         connected_at, updated_at) \
         VALUES('prov-broken', ?, 'paypal', 'Broken', 'k', ?, NULL, NULL, NULL, \
         '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')",
    )
    .bind(&profile.id)
    .bind(&base)
    .execute(&mut *conn)
    .await
    .expect("insert unbuildable row");
    sqlx::query("PRAGMA ignore_check_constraints = OFF")
        .execute(&mut *conn)
        .await
        .expect("pragma off");
    drop(conn);

    let good = seed_provider(&state, "zaprite", &base).await;

    reconcile::tick(&state)
        .await
        .expect("an unbuildable provider must not fail the tick");

    // The broken row spent its slot — the attempt is what the throttle counts,
    // so it is retried on the interval and not on every 60-second tick — and
    // recorded nothing, because no HTTP call was ever issued for it.
    let b = health_of(&state, "prov-broken");
    assert!(b.last_probe_at.is_some(), "the attempt still spends the slot");
    assert_eq!(b.last_status, None);
    assert_eq!(b.auth_dead, Default::default());
    assert_eq!(b.cannot_sell, Default::default());

    // ...and the provider behind it was still reached.
    assert_eq!(
        health_of(&state, &good).auth_dead.consecutive,
        1,
        "a broken row earlier in the list must not skip the ones after it"
    );
    let reqs = requests(&seen);
    assert_eq!(reqs.len(), 1, "exactly one provider was buildable");
    assert_eq!(reqs[0].uri, "/v1/orders?limit=1");
}
