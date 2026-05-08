//! API endpoint integration tests.
//!
//! Drives real HTTP requests through the daemon's `axum::Router` against
//! a real SQLite database (per-test tempfile, identical pool options to
//! `src/db/mod.rs::init`). Companion to `tests/migrations.rs`: that file
//! tested schema correctness; this one tests endpoint correctness.
//!
//! These tests bypass `main.rs`'s env-var bootstrap and skip background
//! workers (reconcile, webhook delivery, session reaper). They construct
//! `AppState` programmatically with deterministic values so the same
//! pool, signing key, and admin token are reachable from inside the test
//! body.

use anyhow::Result;
use axum::body::{to_bytes, Body};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::Response;
use chrono::Utc;
use keysat::api::{self, AppState};
use keysat::config::Config;
use keysat::crypto::{self, LicensePayload};
use keysat::db::repo;
use keysat::license_self::Tier;
use keysat::payment::{
    CreateInvoiceParams, CreatedInvoiceHandle, PaymentProvider, ProviderInvoiceStatus,
    ProviderKind, ProviderWebhookEvent,
};
use serde_json::{json, Value};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use std::any::Any;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::sync::RwLock;
use tower::ServiceExt;
use uuid::Uuid;

/// Deterministic admin token used by every test that exercises an admin
/// endpoint. ≥32 chars to satisfy `Config::from_env`'s validation rule
/// (we don't go through that path here, but matching the constraint
/// keeps fixtures realistic).
const TEST_ADMIN_KEY: &str = "test_admin_api_key_with_at_least_32_chars_present";

// ---------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------

/// Open a fresh pool against a throwaway tempfile, mirroring
/// `src/db/mod.rs::init` exactly. `NamedTempFile` is returned so the
/// caller keeps it alive for the test's lifetime — when it drops, the
/// OS reclaims the file.
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
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("run migrations");
    (pool, tmp)
}

/// Build a fully-populated `AppState` ready to serve requests. Skips
/// `main.rs`'s env-var bootstrap and never spawns background workers —
/// these tests only exercise the request/response handler chain.
///
/// - `payment` is `None`. Endpoints that require a payment provider
///   (e.g. `POST /v1/purchase`) will return 503; tests below don't drive
///   those paths.
/// - `self_tier = Tier::Unlicensed` inherits Creator-tier caps (5
///   products, 5 codes, etc.). Plenty for the small fixtures here.
async fn make_test_state() -> (AppState, NamedTempFile) {
    let (pool, tmp) = make_pool().await;
    let keypair = crypto::keys::load_or_generate(&pool)
        .await
        .expect("load_or_generate keypair");

    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        db_path: PathBuf::from(":memory:"),
        admin_api_key: TEST_ADMIN_KEY.to_string(),
        btcpay_url: "http://btcpay.test:23000".to_string(),
        btcpay_browser_url: None,
        btcpay_public_url: None,
        btcpay_api_key: None,
        btcpay_store_id: None,
        btcpay_webhook_secret: None,
        public_base_url: "http://keysat.test".to_string(),
        operator_name: Some("Test Operator".into()),
    };

    let state = AppState {
        db: pool,
        keypair: Arc::new(keypair),
        payment: Arc::new(RwLock::new(None)),
        config: Arc::new(cfg),
        self_tier: Arc::new(RwLock::new(Tier::Unlicensed {
            reason: "test fixture".into(),
        })),
    };
    (state, tmp)
}

/// Issue one request through the router. Clones state per call (cheap;
/// the DB pool, Arc'd config and keypair are all `Clone`) so multiple
/// requests in a single test share the same backend.
async fn send(state: &AppState, req: Request<Body>) -> Response {
    api::router(state.clone())
        .oneshot(req)
        .await
        .expect("router::oneshot")
}

async fn body_json(resp: Response) -> Value {
    let bytes = to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    serde_json::from_slice(&bytes).expect("response body should be JSON")
}

fn build_request(
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).expect("serialize JSON body"))
        }
        None => Body::empty(),
    };
    b.body(body).expect("build request")
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

/// Smoke test for the framework. If this passes, we know the
/// state-construction + router-dispatch + response-parsing pipeline
/// works; tests below can focus on real assertions.
#[tokio::test]
async fn health_endpoint_returns_200() {
    let (state, _tmp) = make_test_state().await;
    let req = build_request("GET", "/healthz", &[], None);
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// Admin endpoints reject calls that lack a valid admin token. The
/// distinction between 401 (no/malformed header) and 403 (header present
/// but token doesn't match) matters — the SPA renders different UI for
/// each ("you're not logged in" vs "you don't have permission").
#[tokio::test]
async fn admin_endpoint_rejects_missing_or_wrong_auth() {
    let (state, _tmp) = make_test_state().await;
    let body = json!({"slug": "x", "name": "X", "price_sats": 100});

    // No Authorization header → 401 unauthorized.
    let req = build_request("POST", "/v1/admin/products", &[], Some(body.clone()));
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "missing auth header should be 401"
    );

    // Wrong token → 403 forbidden. (The constant-time compare in
    // require_admin returns Forbidden, not Unauthorized, when a token
    // is present but doesn't match.)
    let req = build_request(
        "POST",
        "/v1/admin/products",
        &[(
            "authorization",
            "Bearer wrong_token_xxxxxxxxxxxxxxxxxxxxxxxx",
        )],
        Some(body),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "wrong token should be 403"
    );
}

/// The full happy path for an admin write: auth → handler → DB insert
/// → audit log → response. If a refactor ever breaks one of those
/// links, this fails loudly.
#[tokio::test]
async fn admin_creates_product_with_correct_token() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);

    let req = build_request(
        "POST",
        "/v1/admin/products",
        &[("authorization", &auth)],
        Some(json!({
            "slug": "test-product",
            "name": "Test Product",
            "description": "for tests",
            "price_sats": 10_000
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "expected 200; got {}",
        resp.status()
    );

    let body = body_json(resp).await;
    assert_eq!(body["slug"], "test-product");
    assert_eq!(body["name"], "Test Product");
    assert_eq!(body["price_sats"], 10_000);
    let id = body["id"]
        .as_str()
        .expect("response body should contain product id")
        .to_string();

    // Row landed in DB.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM products WHERE id = ?")
        .bind(&id)
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(count, 1, "exactly one product row should exist");

    // Audit row was written for the create.
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE action = 'product.create' AND target_id = ?",
    )
    .bind(&id)
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(audit_count, 1, "audit log should record one create");
}

/// `/v1/validate` always returns HTTP 200 (per the documented contract);
/// failures are surfaced via `ok: false` + a machine-readable `reason`.
/// Bogus input returns `bad_format` — the parser couldn't even decode
/// the base32 envelope. This exercises the rate-limit pre-check and
/// the early parse-fail path.
#[tokio::test]
async fn validate_rejects_unsigned_garbage() {
    let (state, _tmp) = make_test_state().await;
    let req = build_request(
        "POST",
        "/v1/validate",
        &[],
        Some(json!({"key": "not-a-real-license"})),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["ok"], false);
    assert_eq!(body["reason"], "bad_format");
}

/// End-to-end license validation:
///   - seed a product
///   - issue a license tied to it
///   - sign a matching `LicensePayload` with the daemon's actual key
///   - encode to the base32 wire format
///   - POST /v1/validate
///   - assert `ok: true` plus the populated metadata fields
///
/// This is the most complex of the first round — it ties together DB
/// writes, the crypto module, and the validate handler. If anything in
/// any of those layers regresses, this fails.
#[tokio::test]
async fn validate_accepts_well_formed_license() {
    let (state, _tmp) = make_test_state().await;

    // Seed a product directly via the repo (skip the admin endpoint —
    // this test is about /v1/validate, not product creation).
    let product = repo::create_product(
        &state.db,
        "validate-test",
        "Validate Test",
        "",
        100,
        &json!({}),
    )
    .await
    .expect("create_product");

    // Issue a license tied to that product. Perpetual, single-machine,
    // no entitlements — the simplest valid license shape.
    let license_id = Uuid::new_v4();
    let issued_at = Utc::now();
    repo::create_license(
        &state.db,
        &license_id.to_string(),
        &product.id,
        None,                      // invoice_id (manual issuance — no invoice)
        &issued_at.to_rfc3339(),
        &json!({}),                // metadata
        None,                      // policy_id
        None,                      // expires_at — perpetual
        0,                         // grace_seconds
        1,                         // max_machines
        &[],                       // entitlements
        false,                     // is_trial
        None,                      // buyer_email
        None,                      // nostr_npub
    )
    .await
    .expect("create_license");

    // Build the matching signed payload. Must use the same product_id
    // and license_id as the DB row, because validate() looks the row up
    // by license_id and verifies product_id matches.
    let product_uuid = Uuid::parse_str(&product.id).expect("product id is a uuid");
    let payload = LicensePayload {
        version: 2,
        flags: 0,
        product_id: product_uuid,
        license_id,
        issued_at: issued_at.timestamp(),
        expires_at: 0,
        fingerprint_hash: [0; 32],
        entitlements: vec![],
    };
    let signature = crypto::sign_payload(&state.keypair.signing, &payload);
    let key_string = crypto::encode_key(&payload, &signature);

    let req = build_request(
        "POST",
        "/v1/validate",
        &[],
        Some(json!({"key": key_string})),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert_eq!(
        body["ok"], true,
        "validation rejected a known-good license: {body:?}"
    );
    assert_eq!(body["license_id"], license_id.to_string());
    assert_eq!(body["product_id"], product.id);
    assert_eq!(body["status"], "active");
}

// ---------------------------------------------------------------------
// MockPaymentProvider — exercises the purchase + webhook code paths
// without talking to a real BTCPay. Reports kind=Btcpay so the daemon's
// BTCPay-specific compat accessors keep working; produces deterministic
// invoice ids so tests can assert on them; bypasses HMAC verification
// in `validate_webhook` and instead parses the test-supplied JSON body.
// ---------------------------------------------------------------------

struct MockPaymentProvider {
    next_invoice_id: AtomicU64,
}

impl MockPaymentProvider {
    fn new() -> Self {
        Self {
            next_invoice_id: AtomicU64::new(1),
        }
    }
}

#[async_trait::async_trait]
impl PaymentProvider for MockPaymentProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Btcpay
    }

    async fn create_invoice(
        &self,
        _params: CreateInvoiceParams<'_>,
    ) -> Result<CreatedInvoiceHandle> {
        let n = self.next_invoice_id.fetch_add(1, Ordering::SeqCst);
        Ok(CreatedInvoiceHandle {
            provider_invoice_id: format!("mock-inv-{n}"),
            checkout_url: format!("http://mock-checkout.test/i/{n}"),
        })
    }

    async fn get_invoice_status(
        &self,
        _provider_invoice_id: &str,
    ) -> Result<ProviderInvoiceStatus> {
        // Reconcile loop isn't exercised by these tests; return a sane
        // default in case it gets called transitively.
        Ok(ProviderInvoiceStatus::Settled)
    }

    /// Test-friendly webhook validator. Production providers would
    /// HMAC-verify the body; we instead parse the body as JSON of
    /// shape `{"kind": "settled"|"expired"|"invalid"|"refunded"|<other>,
    /// "provider_invoice_id": "..."}`. Tests construct their own
    /// payloads with no signature ceremony.
    fn validate_webhook(
        &self,
        _headers: &HeaderMap,
        body: &[u8],
    ) -> Result<ProviderWebhookEvent> {
        let v: Value = serde_json::from_slice(body)?;
        let kind = v["kind"].as_str().unwrap_or("");
        let id = v["provider_invoice_id"].as_str().unwrap_or("").to_string();
        Ok(match kind {
            "settled" => ProviderWebhookEvent::InvoiceSettled {
                provider_invoice_id: id,
            },
            "expired" => ProviderWebhookEvent::InvoiceExpired {
                provider_invoice_id: id,
            },
            "invalid" => ProviderWebhookEvent::InvoiceInvalid {
                provider_invoice_id: id,
            },
            other => ProviderWebhookEvent::Other {
                kind: other.to_string(),
                provider_invoice_id: Some(id).filter(|s| !s.is_empty()),
            },
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Build a state with a MockPaymentProvider already installed. Mirror of
/// `make_test_state` for tests that drive the purchase / webhook paths.
async fn make_test_state_with_mock_provider() -> (AppState, NamedTempFile) {
    let (state, tmp) = make_test_state().await;
    state
        .set_payment_provider(Arc::new(MockPaymentProvider::new()))
        .await;
    (state, tmp)
}

// ---------------------------------------------------------------------
// Purchase + webhook tests
// ---------------------------------------------------------------------

/// The free-tier shortcut: when post-discount, post-policy-override
/// price is 0 sats, the daemon synthesizes a settled invoice locally,
/// issues a license inline, and returns the signed key in the response.
/// No payment provider involved — `payment` stays `None`. This test
/// verifies that fast path end-to-end.
#[tokio::test]
async fn free_purchase_issues_license_inline() {
    let (state, _tmp) = make_test_state().await;
    let now = Utc::now().to_rfc3339();

    // Seed a product (price > 0) plus a "free" policy that overrides
    // the price to 0 sats. This is the common shape: paid product with
    // an optional free tier on the buy page.
    let product = repo::create_product(
        &state.db,
        "free-test",
        "Free Test",
        "",
        10_000,
        &json!({}),
    )
    .await
    .expect("create_product");

    sqlx::query(
        "INSERT INTO policies(id, product_id, name, slug, price_sats_override, \
         max_machines, public, created_at, updated_at) \
         VALUES('pol-free', ?, 'Free', 'free', 0, 1, 1, ?, ?)",
    )
    .bind(&product.id)
    .bind(&now)
    .bind(&now)
    .execute(&state.db)
    .await
    .expect("insert free policy");

    let req = build_request(
        "POST",
        "/v1/purchase",
        &[],
        Some(json!({
            "product": "free-test",
            "policy_slug": "free"
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert_eq!(
        body["amount_sats"], 0,
        "free policy should produce zero-sat invoice"
    );
    assert!(
        body["license_key"].is_string(),
        "free purchase should return license inline: {body:?}"
    );
    assert_eq!(body["checkout_url"], "");

    // License row exists in DB.
    let licenses: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM licenses")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(licenses, 1, "exactly one license should be issued");

    // The inline license_key validates round-trip via /v1/validate.
    let key = body["license_key"].as_str().unwrap().to_string();
    let req = build_request("POST", "/v1/validate", &[], Some(json!({"key": key})));
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let validation = body_json(resp).await;
    assert_eq!(
        validation["ok"], true,
        "the inlined license_key must validate cleanly: {validation:?}"
    );
}

// Note on the missing paid-purchase test:
//
// `purchase::start` still uses the legacy compat accessor
// `state.btcpay_client()`, which downcasts the active provider
// specifically to the concrete `BtcpayProvider` type rather than
// going through the `PaymentProvider` trait. A `MockPaymentProvider`
// can't satisfy that downcast — it'd need to BE a `BtcpayProvider`,
// which requires a working HTTP client.
//
// The fix is a small refactor of `purchase::start` to use
// `state.payment_provider().await?.create_invoice(...)` instead of
// the compat path. That's already on the v0.3 backlog (see
// `src/payment/mod.rs` "Why a trait" doc comment). Once it lands, a
// `paid_purchase_creates_invoice_via_provider` test slots right in.
// For now we test the webhook handler — which IS already on the
// trait surface — directly against a fixture invoice.

/// The settle webhook: provider POSTs an InvoiceSettled event, daemon
/// flips the invoice status and issues a license. Re-POSTing the same
/// webhook (which providers DO retry, sometimes aggressively) must not
/// duplicate the license — idempotency is critical because a flaky
/// network or provider retries can deliver the same event multiple
/// times. This is the production-correctness invariant we most need to
/// hold.
#[tokio::test]
async fn webhook_settles_invoice_and_issues_license_idempotently() {
    let (state, _tmp) = make_test_state_with_mock_provider().await;

    // Seed a product + a pending invoice directly via the repo (the
    // HTTP purchase endpoint still uses BTCPay-specific compat code —
    // see the comment block above). The webhook handler itself is on
    // the abstract `PaymentProvider` trait, which the mock satisfies,
    // so we can drive it through the router.
    let product = repo::create_product(
        &state.db,
        "webhook-test",
        "Webhook Test",
        "",
        5_000,
        &json!({}),
    )
    .await
    .expect("create_product");

    let internal_invoice_id = Uuid::new_v4().to_string();
    let provider_invoice_id = "mock-inv-fixture".to_string();
    repo::create_invoice(
        &state.db,
        &internal_invoice_id,
        &provider_invoice_id,
        &product.id,
        5_000,
        "http://mock-checkout.test/i/1",
        None, // buyer_email
        None, // buyer_note
        None, // policy_id
    )
    .await
    .expect("create_invoice");

    // First webhook delivery: daemon flips invoice → settled, issues
    // license.
    let webhook_body = json!({
        "kind": "settled",
        "provider_invoice_id": provider_invoice_id,
    });
    let req = build_request(
        "POST",
        "/v1/btcpay/webhook",
        &[("content-type", "application/json")],
        Some(webhook_body.clone()),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "settle webhook should ack 200"
    );

    // Verify state changes.
    let status_after_first: String = sqlx::query_scalar(
        "SELECT status FROM invoices WHERE btcpay_invoice_id = ?",
    )
    .bind(&provider_invoice_id)
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(status_after_first, "settled");

    let licenses_after_first: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM licenses")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(
        licenses_after_first, 1,
        "first settle webhook should issue exactly one license"
    );

    // Re-deliver the same webhook. Daemon must NOT issue a second
    // license — provider retries are routine and a duplicated license
    // means duplicated revenue or duplicated revocation surface area.
    let req = build_request(
        "POST",
        "/v1/btcpay/webhook",
        &[("content-type", "application/json")],
        Some(webhook_body),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "redelivered webhook should also ack 200"
    );

    let licenses_after_second: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM licenses")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(
        licenses_after_second, 1,
        "redelivered settle webhook MUST NOT duplicate the license"
    );
}
