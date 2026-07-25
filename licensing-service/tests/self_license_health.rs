//! The self-license condition of `GET /v1/admin/health-summary`, end to end.
//!
//! # Why this is its own test binary
//!
//! Reaching any unhappy self-license verdict means putting a license key where
//! the daemon looks for one, and both places it looks — `SELF_LICENSE_PATH` and
//! the `KEYSAT_LICENSE` environment variable — are **process-global**. Every
//! health-summary test in `tests/api.rs` asserts the summary's overall status,
//! so setting that variable from inside that binary would flake its neighbours
//! at random: cargo runs the tests in one binary concurrently on several
//! threads, but it runs each `tests/*.rs` as a **separate process**. Isolation
//! by binary is therefore the whole mechanism, and for the same reason the
//! env-touching test below must stay **one** sequential test rather than
//! several. The other tests in this file never read the environment, so they
//! run alongside it safely.
//!
//! # Why it cannot be a unit test
//!
//! What is under test is the roll-up: that the self-license condition is one of
//! the statuses the overall `status` is the worst of. A pure test of
//! `classify` cannot see that array, and a summary whose self-license section
//! is rendered but left out of the roll-up would report `status: "ok"` with a
//! `critical` condition sitting inside it — green while the daemon is running
//! the free tier on a key it cannot verify.
//!
//! Only the `signature_invalid` row is reachable *through the endpoint*: the
//! other six unhappy rows need a key signed by the master private half, which
//! exists on one machine and is not in this repo. The decision itself is
//! covered against the pure `license_self::classify`, and everything between
//! the key and the decision — the row lookup, the clock, the tier — is covered
//! here against `license_self::observe_with`, which takes the key state instead
//! of reading it. Those tests touch no environment variable and run
//! concurrently with the one above quite happily.

use axum::http::{HeaderMap, HeaderValue};
use chrono::{DateTime, Utc};
use keysat::api::AppState;
use keysat::config::Config;
use keysat::db::repo;
use keysat::license_self::{
    observe_with, KeyState, SelfLicenseCode, SelfLicenseSeverity, Tier, SELF_LICENSE_PATH,
};
use serde_json::json;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::sync::RwLock;
use uuid::Uuid;

const ADMIN_KEY: &str = "test_admin_api_key_with_at_least_32_chars_present";

/// Minimum-viable `AppState`: this endpoint reads `db`, `self_tier`,
/// `provider_health` and the admin key, and nothing else.
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
    let keypair = keysat::crypto::keys::load_or_generate(&pool)
        .await
        .expect("load_or_generate keypair");

    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        db_path: PathBuf::from(":memory:"),
        admin_api_key: ADMIN_KEY.to_string(),
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

/// Call the handler directly. The route wiring and the `require_admin` gate are
/// already pinned by `tests/api.rs`; what this file needs is the body.
async fn summary(state: &AppState) -> serde_json::Value {
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {ADMIN_KEY}")).expect("header"),
    );
    let axum::Json(body) =
        keysat::api::health_summary::get(axum::extract::State(state.clone()), headers)
            .await
            .expect("health-summary should succeed");
    body
}

/// A `critical` self-license drags the whole summary to `critical`, and an
/// unlicensed daemon leaves it alone.
///
/// One test, run in sequence, because the two halves disagree about a
/// process-global environment variable — see the module comment.
#[tokio::test]
async fn the_self_license_condition_participates_in_the_overall_status() {
    // This asserts about the absence of a file outside the repo. It is absent
    // on a dev machine and in CI; on the one box that does have a self-license
    // installed, skip rather than fail, because the failure would be the
    // environment and not the code.
    if std::path::Path::new(SELF_LICENSE_PATH).exists() {
        eprintln!("skipping: {SELF_LICENSE_PATH} exists on this host");
        return;
    }
    std::env::remove_var("KEYSAT_LICENSE");

    let (state, _tmp) = make_state().await;

    // --- No key: informational, and the summary stays green. ---
    let body = summary(&state).await;
    assert_eq!(body["conditions"]["self_license"]["code"], "unlicensed");
    assert_eq!(body["conditions"]["self_license"]["status"], "ok");
    assert_eq!(body["conditions"]["self_license"]["alerting"], false);
    assert_eq!(
        body["status"], "ok",
        "the free Creator tier is a legitimate configuration, not an incident"
    );

    // --- A key that does not verify: critical, and it carries the summary. ---
    //
    // Nothing about this string is a valid LIC1 key, which is the point: the
    // daemon has something claiming to be its license and cannot verify it, so
    // it is running the free tier while believing it is licensed.
    std::env::set_var("KEYSAT_LICENSE", "LIC1-not-a-real-key-at-all");
    let body = summary(&state).await;
    let sl = &body["conditions"]["self_license"];
    assert_eq!(sl["code"], "signature_invalid");
    assert_eq!(sl["status"], "critical");
    assert_eq!(sl["alerting"], true);
    assert!(
        sl["detail"].as_str().is_some_and(|d| !d.is_empty()),
        "the operator needs the underlying failure; got {:?}",
        sl["detail"]
    );
    assert!(
        sl["message"].as_str().is_some_and(|m| !m.is_empty()),
        "a critical condition must say something"
    );
    assert_eq!(
        body["status"], "critical",
        "the self-license condition must be one of the statuses the overall status is the \
         worst of — a summary that renders it but leaves it out of the roll-up reads green \
         while the daemon is unlicensed"
    );
    // The other two conditions are quiet, so `critical` can only have come
    // from this one.
    assert_eq!(body["conditions"]["webhook_dead_letters"]["status"], "ok");
    assert_eq!(body["conditions"]["payment_providers"]["status"], "ok");

    // --- Removing the key returns the summary to green. ---
    std::env::remove_var("KEYSAT_LICENSE");
    let body = summary(&state).await;
    assert_eq!(body["conditions"]["self_license"]["code"], "unlicensed");
    assert_eq!(body["status"], "ok");
}

// ---------------------------------------------------------------------
// Everything between the key and the verdict.
//
// `observe_with` takes the `KeyState` rather than reading it, which is the
// whole point: hand it a `Verified` key naming a row these tests inserted and
// the row lookup, the clock and the tier plumbing all run for real, against the
// real `get_license_by_id` and the real writers. With the signature check
// inlined in that function, none of it could be exercised without the master
// private key — and five mutations of it survived the entire suite.
// ---------------------------------------------------------------------

/// 2027-01-15T08:00:00Z. Fixed, because half of what is under test here is
/// whether the caller's clock reaches the verdict at all.
const NOW: i64 = 1_800_000_000;
const DAY: i64 = 24 * 60 * 60;

fn rfc3339(unix: i64) -> String {
    DateTime::from_timestamp(unix, 0)
        .expect("in-range timestamp")
        .to_rfc3339()
}

fn verified(license_id: Uuid, expires_at: i64) -> KeyState {
    KeyState::Verified {
        expires_at,
        license_id,
    }
}

fn licensed_tier() -> Tier {
    Tier::Licensed {
        license_id: Uuid::nil(),
        product_id: Uuid::nil(),
        expires_at: 0,
        entitlements: vec!["unlimited_products".into()],
    }
}

/// Insert a `licenses` row for this daemon's own license, through the same
/// `repo` writers production uses.
async fn seed_self_license_row(state: &AppState, license_id: &Uuid, expires_at: Option<&str>) {
    let product = repo::create_product(&state.db, "keysat", "Keysat", "", 0, &json!({}))
        .await
        .expect("create_product");
    repo::create_license(
        &state.db,
        &license_id.to_string(),
        &product.id,
        None,
        &Utc::now().to_rfc3339(),
        &json!({}),
        None,
        expires_at,
        0,
        1,
        &["unlimited_products".to_string()],
        false,
        None,
        None,
    )
    .await
    .expect("create_license");
}

/// The row is really read, and it is read by the key's `license_id`.
///
/// Revocation is invisible to the key — the signature stays valid forever — so
/// if this lookup does not happen, or happens with the wrong id, an operator
/// whose license was revoked upstream sees a green card. The reason the issuer
/// gave travels with it, because "revoked" alone does not tell anyone what to
/// do next.
#[tokio::test]
async fn a_revoked_row_reaches_the_verdict_through_the_real_lookup() {
    let (state, _tmp) = make_state().await;
    let license_id = Uuid::new_v4();
    seed_self_license_row(&state, &license_id, None).await;
    repo::revoke_license(&state.db, &license_id.to_string(), "chargeback")
        .await
        .expect("revoke_license");

    let verdict = observe_with(verified(license_id, 0), &state.db, &licensed_tier(), NOW)
        .await
        .expect("observe_with");

    assert_eq!(verdict.code, SelfLicenseCode::Revoked);
    assert_eq!(verdict.severity(), SelfLicenseSeverity::Critical);
    assert!(verdict.row_present);
    let detail = verdict.detail.expect("a revoked verdict carries its detail");
    assert!(
        detail.contains("chargeback"),
        "the issuer's reason is the actionable half; got {detail:?}"
    );

    // A key whose id names no row is the offline-grace case, and it must not
    // inherit the revocation of some other row.
    let verdict = observe_with(verified(Uuid::new_v4(), 0), &state.db, &licensed_tier(), NOW)
        .await
        .expect("observe_with");
    assert_eq!(verdict.code, SelfLicenseCode::Ok);
    assert!(!verdict.row_present);
}

/// A row-shortened expiry is read out of the database and beats the key.
///
/// The key here is perpetual, so every part of this verdict comes from the row:
/// the lookup, the RFC-3339 parse, and the `min()`. `refresh_self_tier_from_db`
/// never looks at that column, so this endpoint is the only thing in the daemon
/// that would notice.
#[tokio::test]
async fn a_row_shortened_expiry_reaches_the_verdict_from_the_database() {
    let (state, _tmp) = make_state().await;
    let license_id = Uuid::new_v4();
    seed_self_license_row(&state, &license_id, Some(&rfc3339(NOW - 2 * DAY))).await;

    let verdict = observe_with(verified(license_id, 0), &state.db, &licensed_tier(), NOW)
        .await
        .expect("observe_with");

    assert_eq!(verdict.code, SelfLicenseCode::Expired);
    assert_eq!(verdict.effective_expiry, Some(NOW - 2 * DAY));
    assert!(verdict.row_present);
}

/// A database failure is an error, never a healthy verdict.
///
/// "No row" is reported as `ok` — documented offline grace — so a lookup that
/// swallowed its error would render a broken database as a green card. The rest
/// of the endpoint 500s on a dead database; this must not be the one condition
/// that shrugs.
#[tokio::test]
async fn a_database_failure_is_not_reported_as_a_healthy_daemon() {
    let (state, _tmp) = make_state().await;
    let license_id = Uuid::new_v4();
    seed_self_license_row(&state, &license_id, None).await;

    sqlx::query("DROP TABLE licenses")
        .execute(&state.db)
        .await
        .expect("drop licenses");

    let result = observe_with(verified(license_id, 0), &state.db, &licensed_tier(), NOW).await;
    assert!(
        result.is_err(),
        "a failed lookup must propagate, not read as an absent row; got {:?}",
        result.map(|v| v.code)
    );
}

/// The running tier reaches the verdict.
///
/// `stale_tier` is the only verdict that consults it, and it is the one that
/// says "you are paying for Keysat and this daemon is not using it". A wiring
/// slip that read a constant instead would silence it permanently.
#[tokio::test]
async fn the_running_tier_reaches_the_verdict() {
    let (state, _tmp) = make_state().await;
    let key = verified(Uuid::new_v4(), 0);

    let verdict = observe_with(
        key.clone(),
        &state.db,
        &Tier::Unlicensed {
            reason: "booted before the key was installed".into(),
        },
        NOW,
    )
    .await
    .expect("observe_with");
    assert_eq!(verdict.code, SelfLicenseCode::StaleTier);
    assert_eq!(verdict.severity(), SelfLicenseSeverity::Warn);

    let verdict = observe_with(key, &state.db, &licensed_tier(), NOW)
        .await
        .expect("observe_with");
    assert_eq!(verdict.code, SelfLicenseCode::Ok);
}

/// The caller's clock reaches the verdict.
///
/// Both expiry verdicts are a comparison against it, so a wiring slip that
/// passed a constant would leave a perpetually-healthy card on a daemon whose
/// license lapsed months ago.
#[tokio::test]
async fn the_clock_reaches_the_verdict() {
    let (state, _tmp) = make_state().await;
    let key = verified(Uuid::new_v4(), NOW + 3 * DAY);

    let at = |now| {
        let key = key.clone();
        let db = state.db.clone();
        async move {
            observe_with(key, &db, &licensed_tier(), now)
                .await
                .expect("observe_with")
                .code
        }
    };

    assert_eq!(at(NOW - 100 * DAY).await, SelfLicenseCode::Ok);
    assert_eq!(at(NOW).await, SelfLicenseCode::ExpiringSoon);
    assert_eq!(at(NOW + 4 * DAY).await, SelfLicenseCode::Expired);
}
