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
        rates: keysat::rates::RateCache::new(),
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

/// Paid purchase end-to-end through the trait. v0.1.0:43 migrated
/// `purchase::start` off the legacy `state.btcpay_client()` compat
/// accessor onto the abstract `state.payment_provider()` trait
/// surface, which means a `MockPaymentProvider` can drive the path
/// without a real BTCPay roundtrip.
///
/// Verifies:
///   - the daemon delegates invoice creation to the provider
///   - the returned `provider_invoice_id` is stamped onto the local
///     invoice row's `btcpay_invoice_id` column
///   - the buyer-facing `checkout_url` is whatever the provider
///     returned (mock returns a deterministic stub URL; production
///     BtcpayProvider rewrites the host inside its impl)
///   - no license is issued at this stage (that's the webhook's job)
#[tokio::test]
async fn paid_purchase_creates_invoice_via_provider() {
    let (state, _tmp) = make_test_state_with_mock_provider().await;

    repo::create_product(
        &state.db,
        "paid-test",
        "Paid Test",
        "",
        10_000,
        &json!({}),
    )
    .await
    .expect("create_product");

    let req = build_request(
        "POST",
        "/v1/purchase",
        &[],
        Some(json!({"product": "paid-test"})),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "paid purchase should succeed against the mock provider"
    );

    let body = body_json(resp).await;
    assert_eq!(body["amount_sats"], 10_000);
    assert_eq!(body["btcpay_invoice_id"], "mock-inv-1");
    assert!(
        body["checkout_url"]
            .as_str()
            .map_or(false, |s| s.starts_with("http://mock-checkout.test/")),
        "checkout_url should pass through from the provider: {body:?}"
    );
    assert!(
        body["license_key"].is_null(),
        "no license should be issued before the settle webhook fires"
    );

    // Pending invoice row exists with the provider's id stamped on it.
    let invoice_status: String = sqlx::query_scalar(
        "SELECT status FROM invoices WHERE btcpay_invoice_id = 'mock-inv-1'",
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(invoice_status, "pending");

    // No license yet.
    let licenses: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM licenses")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(licenses, 0);
}

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

/// Tier caps: an Unlicensed (or Creator-tier) operator may create up
/// to `CREATOR_PRODUCT_CAP` products. The Nth+1 attempt returns 402
/// with `upgrade_url` populated so the admin SPA can render the
/// "Upgrade to Pro" CTA inline.
///
/// Then we swap the daemon's `self_tier` to a Licensed tier with the
/// `unlimited_products` entitlement (the same entitlement the master
/// Keysat issues to paying operators) and verify the same N+1 attempt
/// now succeeds. This is the dynamic-swap behavior that lets operators
/// activate a new license via the admin API and keep working without a
/// daemon restart.
#[tokio::test]
async fn tier_caps_block_at_creator_limit_and_unlock_after_upgrade() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);

    // Reach the cap. CREATOR_PRODUCT_CAP is 5; create exactly five.
    for i in 0..5 {
        let req = build_request(
            "POST",
            "/v1/admin/products",
            &[("authorization", &auth)],
            Some(json!({
                "slug": format!("p{i}"),
                "name": format!("Product {i}"),
                "price_sats": 1_000,
            })),
        );
        let resp = send(&state, req).await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "product {i} should succeed (under cap)"
        );
    }

    // Sixth product → 402 with upgrade_url.
    let req = build_request(
        "POST",
        "/v1/admin/products",
        &[("authorization", &auth)],
        Some(json!({
            "slug": "p-over-cap",
            "name": "Over The Cap",
            "price_sats": 1_000,
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::PAYMENT_REQUIRED,
        "6th product should be blocked by the Creator-tier cap"
    );
    let body = body_json(resp).await;
    assert!(
        body["upgrade_url"]
            .as_str()
            .map_or(false, |u| u.contains("/buy/keysat")),
        "402 response should carry an upgrade_url pointing at the master Keysat: {body:?}"
    );

    // DB should still reflect exactly 5 products — the 6th must not
    // have leaked through.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM products")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(count, 5);

    // Swap self_tier to a Licensed tier with `unlimited_products`.
    // Mirrors what `Activate Keysat license` does in the admin UI: the
    // operator pastes their Keysat-licenses-Keysat key, the daemon
    // verifies it against the master pubkey, and writes the parsed
    // entitlements into self_tier under a write lock — no restart.
    *state.self_tier.write().await = Tier::Licensed {
        license_id: Uuid::new_v4(),
        product_id: Uuid::new_v4(),
        expires_at: 0,
        entitlements: vec!["self_host".into(), "unlimited_products".into()],
    };

    // Now the same 6th product attempt succeeds.
    let req = build_request(
        "POST",
        "/v1/admin/products",
        &[("authorization", &auth)],
        Some(json!({
            "slug": "p-after-upgrade",
            "name": "Pro Tier Now",
            "price_sats": 1_000,
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "after the tier swap, the cap should no longer fire"
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM products")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(count, 6, "the previously-blocked product should now exist");
}

/// Webhook DLQ (dead-letter queue) — list + retry round trip.
///
/// The delivery worker retries failed deliveries with exponential
/// backoff up to 10 attempts, then sets `next_attempt_at = NULL` and
/// walks away. Pre-this-feature, those rows were invisible to the
/// operator. Now `GET /v1/admin/webhook-deliveries?status=failed`
/// surfaces them and `POST /v1/admin/webhook-deliveries/:id/retry`
/// puts them back in the queue.
///
/// We seed a "dead-lettered" row directly via SQL — the worker isn't
/// spawned in tests, so we don't need to drive 10 real failures to
/// reach the dead state. This tests the admin surface, not the
/// worker.
#[tokio::test]
async fn webhook_dlq_lists_failed_and_retry_requeues() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);
    let now = Utc::now().to_rfc3339();

    // Configure a webhook endpoint to own the deliveries.
    let endpoint_id = "ep1";
    sqlx::query(
        "INSERT INTO webhook_endpoints(id, url, secret, event_types, active, \
         description, created_at, updated_at) \
         VALUES(?, 'https://operator.example/keysat-hook', \
                '0123456789abcdef0123456789abcdef', '[\"*\"]', 1, '', ?, ?)",
    )
    .bind(endpoint_id)
    .bind(&now)
    .bind(&now)
    .execute(&state.db)
    .await
    .unwrap();

    // One delivery in each state: delivered (success), pending
    // (in-queue), and failed (DLQ — what we mostly care about).
    let mk = |id: &str, attempts: i64, next: Option<&str>, delivered: Option<&str>| {
        let id = id.to_string();
        let attempts = attempts;
        let next = next.map(|s| s.to_string());
        let delivered = delivered.map(|s| s.to_string());
        let pool = state.db.clone();
        let now = now.clone();
        async move {
            sqlx::query(
                "INSERT INTO webhook_deliveries(id, endpoint_id, event_type, \
                 payload_json, attempt_count, next_attempt_at, delivered_at, created_at) \
                 VALUES(?, ?, 'license.issued', '{}', ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(endpoint_id)
            .bind(attempts)
            .bind(next.as_deref())
            .bind(delivered.as_deref())
            .bind(&now)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    mk("d-delivered", 1, None, Some(&now)).await;
    mk("d-pending", 2, Some(&now), None).await;
    // The dead-lettered case: 10 attempts, next_attempt_at NULL, never delivered.
    mk("d-failed", 10, None, None).await;

    // List with status=failed should return ONLY the dead-lettered row.
    let req = build_request(
        "GET",
        "/v1/admin/webhook-deliveries?status=failed",
        &[("authorization", &auth)],
        None,
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let deliveries = body["deliveries"].as_array().expect("deliveries array");
    assert_eq!(
        deliveries.len(),
        1,
        "status=failed should return the one DLQ row, got {deliveries:?}"
    );
    assert_eq!(deliveries[0]["id"], "d-failed");
    assert_eq!(deliveries[0]["attempt_count"], 10);

    // Retry the dead-lettered delivery.
    let req = build_request(
        "POST",
        "/v1/admin/webhook-deliveries/d-failed/retry",
        &[("authorization", &auth)],
        None,
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "retry should succeed");
    let body = body_json(resp).await;
    assert_eq!(
        body["attempt_count"], 0,
        "retry should reset attempt_count to 0"
    );
    assert!(
        body["next_attempt_at"].is_string(),
        "retry should set next_attempt_at: {body:?}"
    );

    // After retry: status=failed should be empty (the row left the
    // DLQ); status=pending should now contain it.
    let req = build_request(
        "GET",
        "/v1/admin/webhook-deliveries?status=failed",
        &[("authorization", &auth)],
        None,
    );
    let resp = send(&state, req).await;
    let body = body_json(resp).await;
    assert_eq!(
        body["deliveries"].as_array().unwrap().len(),
        0,
        "after retry, the row should no longer be 'failed'"
    );

    let req = build_request(
        "GET",
        "/v1/admin/webhook-deliveries?status=pending",
        &[("authorization", &auth)],
        None,
    );
    let resp = send(&state, req).await;
    let body = body_json(resp).await;
    let pending = body["deliveries"].as_array().unwrap();
    assert!(
        pending.iter().any(|d| d["id"] == "d-failed"),
        "after retry, the previously-failed row should appear in 'pending'"
    );

    // Audit log captured the retry.
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE action = 'webhook_delivery.retry' AND target_id = 'd-failed'",
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(audit_count, 1, "retry must write an audit log entry");

    // Retry on a non-existent id is 404.
    let req = build_request(
        "POST",
        "/v1/admin/webhook-deliveries/never-existed/retry",
        &[("authorization", &auth)],
        None,
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Bad status filter is 400 (a typo'd query string shouldn't
    // silently succeed; that's a UI footgun).
    let req = build_request(
        "GET",
        "/v1/admin/webhook-deliveries?status=garbage",
        &[("authorization", &auth)],
        None,
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Buyer self-service recovery: re-derive a lost license key from
/// (invoice_id, buyer_email). The most-common buyer support ticket
/// turned into a self-service flow.
///
/// Verifies:
///   - matching pair → 200 with a license_key that validates
///   - wrong email → 404 with the generic error message (does not
///     leak whether the invoice id existed)
///   - missing invoice → 404
///   - unsettled invoice → 404 (no license to recover)
///   - audit log row written on success
#[tokio::test]
async fn recover_returns_license_key_for_matching_pair() {
    let (state, _tmp) = make_test_state().await;

    // Seed a product, a settled invoice, and an active license.
    let product = repo::create_product(
        &state.db,
        "rec-test",
        "Recover Test",
        "",
        5_000,
        &json!({}),
    )
    .await
    .expect("create_product");

    let invoice_id = Uuid::new_v4().to_string();
    repo::create_invoice(
        &state.db,
        &invoice_id,
        "btcpay-rec-1",
        &product.id,
        5_000,
        "http://x/",
        Some("Buyer@Example.COM"), // mixed case to verify lowercasing
        None,
        None,
    )
    .await
    .expect("create_invoice");
    sqlx::query("UPDATE invoices SET status = 'settled' WHERE id = ?")
        .bind(&invoice_id)
        .execute(&state.db)
        .await
        .unwrap();

    let license_id = Uuid::new_v4();
    let now = Utc::now().to_rfc3339();
    repo::create_license(
        &state.db,
        &license_id.to_string(),
        &product.id,
        Some(&invoice_id),
        &now,
        &json!({}),
        None,
        None,
        0,
        1,
        &[],
        false,
        Some("buyer@example.com"),
        None,
    )
    .await
    .expect("create_license");

    // Wrong email → 404 with generic error (does not reveal the
    // invoice id exists).
    let req = build_request(
        "POST",
        "/v1/recover",
        &[("content-type", "application/json")],
        Some(json!({
            "invoice_id": invoice_id,
            "email": "wrong@example.com",
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "wrong email should 404"
    );

    // Bogus invoice id → same generic 404.
    let req = build_request(
        "POST",
        "/v1/recover",
        &[("content-type", "application/json")],
        Some(json!({
            "invoice_id": Uuid::new_v4().to_string(),
            "email": "buyer@example.com",
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Matching pair (case-insensitive email) → 200 with a real
    // license key.
    let req = build_request(
        "POST",
        "/v1/recover",
        &[("content-type", "application/json")],
        Some(json!({
            "invoice_id": invoice_id,
            "email": "Buyer@Example.com", // different casing on purpose
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "matching pair should succeed"
    );
    let body = body_json(resp).await;
    let license_key = body["license_key"]
        .as_str()
        .expect("license_key should be present in response")
        .to_string();
    assert_eq!(body["license_id"], license_id.to_string());

    // The recovered key validates round-trip via /v1/validate.
    let req = build_request(
        "POST",
        "/v1/validate",
        &[("content-type", "application/json")],
        Some(json!({"key": license_key})),
    );
    let resp = send(&state, req).await;
    let validation = body_json(resp).await;
    assert_eq!(
        validation["ok"], true,
        "recovered key must validate cleanly: {validation:?}"
    );

    // Audit log captured the recovery.
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE action = 'license.recovered'",
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(audit_count, 1, "recovery must write an audit row");
}

/// USD-priced paid purchase records the listed currency, value, and
/// exchange rate on the invoice row. Uses a manual rate pin so the
/// test is network-free and the conversion is exactly verifiable.
#[tokio::test]
async fn paid_purchase_in_usd_records_listed_currency_and_rate() {
    let (state, _tmp) = make_test_state_with_mock_provider().await;

    // Pin USD at $50,000 / BTC. $49.00 (4900 cents) → 9800 sats:
    //   sats = 4900 * 1_000_000 / 50000 = 98000... wait
    //   4900 * 1_000_000 = 4_900_000_000
    //   4_900_000_000 / 50_000 = 98_000
    sqlx::query("INSERT INTO settings(key, value, updated_at) VALUES('manual_rate_pin_USD', '50000', ?)")
        .bind(Utc::now().to_rfc3339())
        .execute(&state.db)
        .await
        .unwrap();

    // USD-priced product via the typed admin endpoint.
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);
    let req = build_request(
        "POST",
        "/v1/admin/products",
        &[("authorization", &auth)],
        Some(json!({
            "slug": "usd-app",
            "name": "USD App",
            "price_currency": "USD",
            "price_value": 4900,
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Initiate purchase. Should call create_invoice with the rate
    // recorded.
    let req = build_request(
        "POST",
        "/v1/purchase",
        &[("content-type", "application/json")],
        Some(json!({"product": "usd-app"})),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["amount_sats"], 98_000,
        "$49.00 at $50k/BTC = 98,000 sats — got {body:?}"
    );

    // The invoice row carries the audit trail.
    let row: (Option<String>, Option<i64>, Option<i64>, Option<String>, i64) = sqlx::query_as(
        "SELECT listed_currency, listed_value, exchange_rate_centibps, \
         exchange_rate_source, amount_sats FROM invoices WHERE btcpay_invoice_id = 'mock-inv-1'"
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(row.0.as_deref(), Some("USD"));
    assert_eq!(row.1, Some(4900));
    assert_eq!(row.2, Some(500_000_000), "rate × 10000: 50000 × 10000");
    assert_eq!(row.3.as_deref(), Some("manual_pin"));
    assert_eq!(row.4, 98_000);
}

/// Active-provider preference round-trip. Pins the contract that
/// `Activate <provider>` flips both the in-memory provider AND the
/// persisted preference so the next daemon boot picks the same one.
///
/// Simulates the operator's lifecycle:
///   1. Configure both BTCPay and Zaprite (both rows in DB)
///   2. Activate Zaprite → preference flag = "zaprite"
///   3. Activate BTCPay → preference flag = "btcpay"
///   4. Disconnect BTCPay → preference flag cleared (because it
///      pointed at the wiped config)
///   5. Disconnect Zaprite while preference was already "btcpay"
///      → preference NOT cleared (stays at "btcpay" because it
///      was pointing at a different provider)
#[tokio::test]
async fn payment_provider_preference_round_trip() {
    use keysat::payment::{self, ProviderKind};

    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);

    // Pre-seed both configs as if the operator had run Connect on
    // each at some point. We bypass the actual Connect endpoints
    // because they call out to BTCPay / Zaprite to validate the
    // credentials, which we don't want to do in unit tests.
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO btcpay_config(id, base_url, api_key, store_id, webhook_id, \
         webhook_secret, connected_at) \
         VALUES(1, 'http://btcpay.test', 'btcpay-key', 'store-1', 'wh-1', \
         '0123456789abcdef', ?)",
    )
    .bind(&now)
    .execute(&state.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO zaprite_config(id, api_key, base_url, webhook_id, connected_at, updated_at) \
         VALUES(1, 'zaprite-key', 'https://api.zaprite.test', NULL, ?, ?)",
    )
    .bind(&now)
    .bind(&now)
    .execute(&state.db)
    .await
    .unwrap();

    // Step 1: no preference recorded yet.
    let pref = payment::read_active_provider_preference(&state.db).await;
    assert_eq!(pref, None);

    // Step 2: GET status surfaces both as configured, no active yet.
    let req = build_request(
        "GET",
        "/v1/admin/payment-provider/status",
        &[("authorization", &auth)],
        None,
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["btcpay_configured"], true);
    assert_eq!(body["zaprite_configured"], true);
    assert!(body["preferred"].is_null());

    // Step 3: Activate Zaprite. The endpoint reads the saved
    // zaprite_config to build the provider — the saved key
    // 'zaprite-key' won't talk to a real API but the activate
    // path doesn't ping; that's only on Connect.
    let req = build_request(
        "POST",
        "/v1/admin/payment-provider/activate",
        &[("authorization", &auth)],
        Some(json!({"provider": "zaprite"})),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "activate zaprite should succeed when zaprite_config is present"
    );
    let pref = payment::read_active_provider_preference(&state.db).await;
    assert_eq!(pref, Some(ProviderKind::Zaprite));

    // Step 4: Activate BTCPay. Preference flips.
    let req = build_request(
        "POST",
        "/v1/admin/payment-provider/activate",
        &[("authorization", &auth)],
        Some(json!({"provider": "btcpay"})),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let pref = payment::read_active_provider_preference(&state.db).await;
    assert_eq!(pref, Some(ProviderKind::Btcpay));

    // Step 5: Activate something that's not configured. Should 400.
    sqlx::query("DELETE FROM zaprite_config WHERE id = 1")
        .execute(&state.db)
        .await
        .unwrap();
    let req = build_request(
        "POST",
        "/v1/admin/payment-provider/activate",
        &[("authorization", &auth)],
        Some(json!({"provider": "zaprite"})),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "activating an unconfigured provider must 400 with 'run Connect first'"
    );

    // Step 6: Bad provider name → 400.
    let req = build_request(
        "POST",
        "/v1/admin/payment-provider/activate",
        &[("authorization", &auth)],
        Some(json!({"provider": "stripe"})),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Step 7: write_active_provider_preference invariant —
    // explicit setting survives a re-read (durability across the
    // simulated restart that the boot-time loader cares about).
    payment::write_active_provider_preference(&state.db, ProviderKind::Btcpay)
        .await
        .unwrap();
    let pref = payment::read_active_provider_preference(&state.db).await;
    assert_eq!(pref, Some(ProviderKind::Btcpay));
    payment::write_active_provider_preference(&state.db, ProviderKind::Zaprite)
        .await
        .unwrap();
    let pref = payment::read_active_provider_preference(&state.db).await;
    assert_eq!(pref, Some(ProviderKind::Zaprite));
}

/// Zaprite webhook authentication contract.
///
/// Zaprite doesn't sign webhooks (verified May 2026 — no HMAC,
/// no JWT, no header-based signature). The defense Keysat uses is
/// the externalUniqId round-trip: we set our local invoice UUID
/// as the order's externalUniqId at creation, and the webhook
/// handler trusts the body only insofar as we can match the
/// Zaprite order id back to a local invoice we created.
///
/// This test pins the validate_webhook impl's parsing contract:
///   - extracts the order id from `data.id` (Zaprite's payload shape)
///   - maps event types to ProviderWebhookEvent variants
///   - rejects payloads missing an order id
#[tokio::test]
async fn zaprite_webhook_event_parsing() {
    use keysat::payment::{
        zaprite::{ZapriteClient, ZapriteProvider},
        PaymentProvider, ProviderWebhookEvent,
    };

    // We don't talk to Zaprite for this test — just exercise the
    // pure-parsing branch of validate_webhook. Construct a client
    // with bogus credentials; never used here.
    let provider = ZapriteProvider::new(ZapriteClient::new(
        "https://api.zaprite.test",
        "test-key-not-used",
    ));
    let headers = axum::http::HeaderMap::new();

    // order.paid → InvoiceSettled
    let body = br#"{"event":"order.paid","data":{"id":"zap-order-1"}}"#;
    let event = provider.validate_webhook(&headers, body).expect("parse");
    match event {
        ProviderWebhookEvent::InvoiceSettled { provider_invoice_id } => {
            assert_eq!(provider_invoice_id, "zap-order-1");
        }
        other => panic!("expected InvoiceSettled, got {other:?}"),
    }

    // order.complete + order.overpaid → also Settled (operator gets paid)
    for kind in &["order.complete", "order.overpaid"] {
        let body = format!(r#"{{"event":"{kind}","data":{{"id":"x"}}}}"#);
        let event = provider
            .validate_webhook(&headers, body.as_bytes())
            .expect("parse");
        assert!(
            matches!(event, ProviderWebhookEvent::InvoiceSettled { .. }),
            "{kind} should map to Settled"
        );
    }

    // order.expired → InvoiceExpired
    let body = br#"{"event":"order.expired","data":{"id":"zap-order-2"}}"#;
    let event = provider.validate_webhook(&headers, body).expect("parse");
    assert!(matches!(
        event,
        ProviderWebhookEvent::InvoiceExpired { .. }
    ));

    // order.refunded → InvoiceRefunded
    let body = br#"{"event":"order.refunded","data":{"id":"zap-order-3"}}"#;
    let event = provider.validate_webhook(&headers, body).expect("parse");
    assert!(matches!(
        event,
        ProviderWebhookEvent::InvoiceRefunded { .. }
    ));

    // Unknown event type → Other (forward-compat for new event
    // kinds Zaprite ships in the future)
    let body = br#"{"event":"order.partially_refunded","data":{"id":"zap-order-4"}}"#;
    let event = provider.validate_webhook(&headers, body).expect("parse");
    match event {
        ProviderWebhookEvent::Other { kind, provider_invoice_id } => {
            assert_eq!(kind, "order.partially_refunded");
            assert_eq!(provider_invoice_id.as_deref(), Some("zap-order-4"));
        }
        other => panic!("expected Other, got {other:?}"),
    }

    // Missing order id → reject. An attacker can't trigger any
    // local state change without telling us which order to act on.
    let body = br#"{"event":"order.paid","data":{}}"#;
    let result = provider.validate_webhook(&headers, body);
    assert!(
        result.is_err(),
        "payload without order id must be rejected"
    );

    // Malformed JSON → reject.
    let body = b"not json at all";
    let result = provider.validate_webhook(&headers, body);
    assert!(result.is_err());
}

/// Zaprite provider self-identifies as `ProviderKind::Zaprite`.
/// Trivial but pins the kind() return for the call sites that
/// switch on provider identity (e.g., audit log strings).
#[tokio::test]
async fn zaprite_provider_kind() {
    use keysat::payment::{
        zaprite::{ZapriteClient, ZapriteProvider},
        PaymentProvider, ProviderKind,
    };
    let p = ZapriteProvider::new(ZapriteClient::new(
        "https://api.zaprite.test",
        "test-key",
    ));
    assert_eq!(p.kind(), ProviderKind::Zaprite);
    assert_eq!(p.kind().as_str(), "zaprite");
}

/// Rate fetcher: manual pin in settings table overrides the source
/// chain. Locks in the test-mode + maintenance-window contract that
/// other phases (invoice rate recording, buy-page rendering) rely on.
#[tokio::test]
async fn rate_cache_honors_manual_pin_from_settings() {
    let (state, _tmp) = make_test_state().await;

    // Pin USD at $65,000 / BTC. The fetcher MUST return this value
    // without hitting any external API.
    sqlx::query("INSERT INTO settings(key, value, updated_at) VALUES('manual_rate_pin_USD', '65000', ?)")
        .bind(Utc::now().to_rfc3339())
        .execute(&state.db)
        .await
        .unwrap();

    let rate = keysat::rates::get_rate(&state, "USD")
        .await
        .expect("manual pin should resolve without network");
    assert_eq!(rate.units_per_btc, 65000.0);
    assert_eq!(rate.source, "manual_pin");

    // Convert $49.00 (4900 cents) to sats. At $65k/BTC:
    // sats = 4900 * 1_000_000 / 65000 = 75,384.6 → 75,385.
    let conv = keysat::rates::convert_to_sats(&state, "USD", 4900)
        .await
        .expect("convert");
    assert_eq!(conv.sats, 75_385, "rounding tie-break: 75384.615 rounds to 75385");
    assert_eq!(
        conv.rate_centibps,
        Some(650_000_000),
        "rate stored as units×10000: 65000 × 10000"
    );

    // SAT-currency conversions are identity (no rate involved).
    let sat_conv = keysat::rates::convert_to_sats(&state, "SAT", 50_000)
        .await
        .unwrap();
    assert_eq!(sat_conv.sats, 50_000);
    assert!(sat_conv.rate_centibps.is_none());
}

/// Admin endpoint visibility: GET /v1/admin/rates returns whatever
/// is currently cached, including manual pins. Operators can verify
/// the daemon's current quote against external sources before
/// trusting fiat-priced invoice flows.
#[tokio::test]
async fn admin_rates_endpoint_reflects_manual_pin() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);

    sqlx::query("INSERT INTO settings(key, value, updated_at) VALUES('manual_rate_pin_USD', '60000', ?)")
        .bind(Utc::now().to_rfc3339())
        .execute(&state.db)
        .await
        .unwrap();

    // Trigger a rate read so the cache populates.
    let _ = keysat::rates::get_rate(&state, "USD").await.unwrap();

    let req = build_request(
        "GET",
        "/v1/admin/rates",
        &[("authorization", &auth)],
        None,
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let rates = body["rates"].as_array().expect("rates array");
    let usd = rates
        .iter()
        .find(|r| r["currency"] == "USD")
        .expect("USD entry should be present");
    assert_eq!(usd["units_per_btc"], 60_000.0);
    assert_eq!(usd["source"], "manual_pin");
}

/// Multi-currency product creation. The admin endpoint accepts both
/// the legacy SAT-only form (`price_sats: N`) and the new typed form
/// (`price_currency + price_value`). Verifies:
///   - legacy form still works, produces a SAT-currency row
///   - typed SAT form works, dual-writes price_sats correctly
///   - typed USD form works, leaves price_sats=0 (filled at invoice time)
///   - unknown currency code → 400
///   - inconsistent legacy + typed values → 400 (catches half-migrated clients)
///   - typed without value → 400; value without currency → 400
#[tokio::test]
async fn admin_create_product_accepts_legacy_and_typed_currency_forms() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);

    // Legacy SAT form.
    let req = build_request(
        "POST",
        "/v1/admin/products",
        &[("authorization", &auth)],
        Some(json!({"slug": "legacy", "name": "Legacy", "price_sats": 50000})),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["price_sats"], 50_000);
    assert_eq!(body["price_currency"], "SAT");
    assert_eq!(body["price_value"], 50_000);

    // Typed SAT form.
    let req = build_request(
        "POST",
        "/v1/admin/products",
        &[("authorization", &auth)],
        Some(json!({
            "slug": "typed-sat",
            "name": "Typed SAT",
            "price_currency": "SAT",
            "price_value": 75000,
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["price_sats"], 75_000);
    assert_eq!(body["price_currency"], "SAT");
    assert_eq!(body["price_value"], 75_000);

    // Typed USD form: $49.00 = 4900 cents. price_sats stays 0 until
    // the first invoice triggers a rate lookup.
    let req = build_request(
        "POST",
        "/v1/admin/products",
        &[("authorization", &auth)],
        Some(json!({
            "slug": "typed-usd",
            "name": "Typed USD",
            "price_currency": "USD",
            "price_value": 4900,
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["price_currency"], "USD");
    assert_eq!(body["price_value"], 4900);
    assert_eq!(
        body["price_sats"], 0,
        "USD products should have price_sats=0 until first invoice rate-converts them"
    );

    // Bad currency.
    let req = build_request(
        "POST",
        "/v1/admin/products",
        &[("authorization", &auth)],
        Some(json!({
            "slug": "bad-currency",
            "name": "Bad",
            "price_currency": "GBP",
            "price_value": 100,
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Inconsistent legacy + typed (catches half-migrated clients).
    let req = build_request(
        "POST",
        "/v1/admin/products",
        &[("authorization", &auth)],
        Some(json!({
            "slug": "inconsistent",
            "name": "Inconsistent",
            "price_sats": 50000,
            "price_currency": "USD",
            "price_value": 4900,
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "mismatched legacy + typed pricing should 400"
    );

    // Half-form: currency without value.
    let req = build_request(
        "POST",
        "/v1/admin/products",
        &[("authorization", &auth)],
        Some(json!({
            "slug": "half-1",
            "name": "Half 1",
            "price_currency": "USD",
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Half-form: value without currency.
    let req = build_request(
        "POST",
        "/v1/admin/products",
        &[("authorization", &auth)],
        Some(json!({
            "slug": "half-2",
            "name": "Half 2",
            "price_value": 4900,
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Community analytics: opt-in toggle + privacy contract.
///
/// Locks in two invariants:
///   - Default state is OFF; no install_uuid generated.
///   - Enabling generates a fresh install_uuid; the heartbeat
///     preview's counts are floored to the nearest 5 (anti-
///     fingerprinting); no operator-identifying fields are present.
///   - Bad collector URL → 400 (must start with http:// or https://).
#[tokio::test]
async fn community_analytics_opt_in_and_privacy_contract() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);

    // Default state: disabled, no install_uuid yet.
    let req = build_request(
        "GET",
        "/v1/admin/community-analytics",
        &[("authorization", &auth)],
        None,
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["enabled"], false, "must default to off");
    assert!(
        body["install_uuid"].is_null(),
        "no UUID should exist before opt-in"
    );
    assert!(
        body["collector_url"].is_null(),
        "no URL should exist before opt-in"
    );

    // Bad URL → 400.
    let req = build_request(
        "POST",
        "/v1/admin/community-analytics",
        &[("authorization", &auth)],
        Some(json!({"enabled": true, "collector_url": "ftp://wrong"})),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Enabling without a URL is allowed (armed but silent).
    let req = build_request(
        "POST",
        "/v1/admin/community-analytics",
        &[("authorization", &auth)],
        Some(json!({"enabled": true, "collector_url": null})),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Now an install_uuid exists.
    let req = build_request(
        "GET",
        "/v1/admin/community-analytics",
        &[("authorization", &auth)],
        None,
    );
    let resp = send(&state, req).await;
    let body = body_json(resp).await;
    assert_eq!(body["enabled"], true);
    let uuid = body["install_uuid"]
        .as_str()
        .expect("install_uuid should be present after opt-in");
    assert_eq!(uuid.len(), 36, "install_uuid should be a UUIDv4 string");

    // Privacy contract: the preview heartbeat MUST contain only
    // anonymized fields. Specifically, no operator_name, no
    // public_url, no store_id, no api keys, no buyer info.
    let preview = &body["preview_heartbeat"];
    let preview_str =
        serde_json::to_string(preview).expect("preview should serialize");
    for forbidden in &[
        "operator_name",
        "public_url",
        "store_id",
        "api_key",
        "buyer_email",
        "btcpay_url",
    ] {
        assert!(
            !preview_str.contains(forbidden),
            "preview heartbeat must not contain '{forbidden}': {preview_str}"
        );
    }
    // Counts must be floored to the nearest 5. Seed 23 active
    // licenses → counts.active_licenses must be 20.
    let product = repo::create_product(
        &state.db,
        "ana-prod",
        "Analytics Test",
        "",
        100,
        &json!({}),
    )
    .await
    .unwrap();
    for _ in 0..23 {
        let lid = Uuid::new_v4().to_string();
        repo::create_license(
            &state.db,
            &lid,
            &product.id,
            None,
            &Utc::now().to_rfc3339(),
            &json!({}),
            None,
            None,
            0,
            1,
            &[],
            false,
            None,
            None,
        )
        .await
        .unwrap();
    }
    let req = build_request(
        "GET",
        "/v1/admin/community-analytics",
        &[("authorization", &auth)],
        None,
    );
    let resp = send(&state, req).await;
    let body = body_json(resp).await;
    let preview = &body["preview_heartbeat"];
    assert_eq!(
        preview["counts"]["active_licenses"], 20,
        "23 licenses must floor to 20 (anti-fingerprinting): {preview:?}"
    );

    // Reset wipes the UUID.
    let req = build_request(
        "POST",
        "/v1/admin/community-analytics/reset",
        &[("authorization", &auth)],
        None,
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let req = build_request(
        "GET",
        "/v1/admin/community-analytics",
        &[("authorization", &auth)],
        None,
    );
    let body = body_json(send(&state, req).await).await;
    assert!(
        body["install_uuid"].is_null(),
        "install_uuid must be wiped after reset: {body:?}"
    );
}

// ---------------------------------------------------------------------
// Recurring-subscription policy admin (Phase 4 of recurring subs)
//
// The renewal worker (src/subscriptions.rs + tests/subscriptions.rs)
// has its own coverage. This block is about the ADMIN surface — can an
// operator create a recurring policy through the API, can they edit
// it, and does the public buy-page endpoint surface the right cadence
// fields for the front-end to render?
// ---------------------------------------------------------------------

/// Helper: swap `state.self_tier` to a Pro-equivalent licensed tier
/// (carries `unlimited_products`, `unlimited_policies`, AND
/// `recurring_billing`). Mirrors what `Activate Keysat license` does
/// in production.
async fn upgrade_to_pro(state: &AppState) {
    *state.self_tier.write().await = Tier::Licensed {
        license_id: Uuid::new_v4(),
        product_id: Uuid::new_v4(),
        expires_at: 0,
        entitlements: vec![
            "self_host".into(),
            "unlimited_products".into(),
            "unlimited_policies".into(),
            "unlimited_codes".into(),
            "recurring_billing".into(),
            "card_payments".into(),
        ],
    };
}

/// Operator on Creator tier (no `recurring_billing` entitlement)
/// cannot create a recurring policy. The 402 should mention upgrade.
#[tokio::test]
async fn recurring_policy_blocked_on_creator_tier() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);

    // Seed a product so `product_slug` lookup succeeds and the test
    // exercises the recurring-feature gate, not the not-found path.
    let _ = repo::create_product(
        &state.db,
        "rec-blocked",
        "Blocked",
        "",
        100_000,
        &json!({}),
    )
    .await
    .expect("create_product");

    let req = build_request(
        "POST",
        "/v1/admin/policies",
        &[("authorization", &auth)],
        Some(json!({
            "product_slug": "rec-blocked",
            "name": "Monthly",
            "slug": "monthly",
            "is_recurring": true,
            "renewal_period_days": 30
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::PAYMENT_REQUIRED,
        "Creator tier must see 402 for recurring=true; got {}",
        resp.status()
    );
    let body = body_json(resp).await;
    assert!(
        body["upgrade_url"].as_str().unwrap_or("").contains("buy/keysat"),
        "402 should carry an upgrade_url to the master Keysat: {body:?}"
    );
}

/// Operator on Pro tier can create a monthly subscription policy. The
/// stored row carries the recurring fields, the public list endpoint
/// echoes them, and the policies admin list shows is_recurring=true.
#[tokio::test]
async fn pro_tier_creates_monthly_recurring_policy() {
    let (state, _tmp) = make_test_state().await;
    upgrade_to_pro(&state).await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);

    let _ = repo::create_product(
        &state.db,
        "rec-product",
        "Recurring Product",
        "",
        25_000,
        &json!({}),
    )
    .await
    .expect("create_product");

    // Create a recurring monthly policy with a 14-day trial.
    let req = build_request(
        "POST",
        "/v1/admin/policies",
        &[("authorization", &auth)],
        Some(json!({
            "product_slug": "rec-product",
            "name": "Monthly",
            "slug": "monthly",
            "duration_seconds": 30 * 86_400,
            "max_machines": 1,
            "is_recurring": true,
            "renewal_period_days": 30,
            "grace_period_days": 7,
            "trial_days": 14
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "create with Pro tier should succeed; got {}",
        resp.status()
    );
    let body = body_json(resp).await;
    assert_eq!(body["is_recurring"], true);
    assert_eq!(body["renewal_period_days"], 30);
    assert_eq!(body["grace_period_days"], 7);
    assert_eq!(body["trial_days"], 14);

    // Public buy-page API surfaces the same shape so the JS price
    // renderer can reach for it.
    let req = build_request("GET", "/v1/products/rec-product/policies", &[], None);
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let policies = body["policies"].as_array().expect("policies array");
    let monthly = policies
        .iter()
        .find(|p| p["slug"] == "monthly")
        .expect("monthly policy in public list");
    assert_eq!(monthly["is_recurring"], true);
    assert_eq!(monthly["renewal_period_days"], 30);
    assert_eq!(monthly["trial_days"], 14);
}

/// Validation: recurring=true with renewal_period_days=0 must be rejected.
/// Catches a foot-gun where the operator forgets to fill in the cadence.
#[tokio::test]
async fn recurring_requires_positive_period() {
    let (state, _tmp) = make_test_state().await;
    upgrade_to_pro(&state).await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);

    let _ = repo::create_product(&state.db, "rec-bad", "Bad", "", 100, &json!({}))
        .await
        .expect("create_product");

    let req = build_request(
        "POST",
        "/v1/admin/policies",
        &[("authorization", &auth)],
        Some(json!({
            "product_slug": "rec-bad",
            "name": "Bad",
            "slug": "bad",
            "is_recurring": true,
            "renewal_period_days": 0
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "is_recurring=true with renewal_period_days=0 must be rejected"
    );
}

/// Edit-policy can flip a non-recurring policy to recurring on Pro tier
/// and a Pro-tier-gated operator gets a 402 trying the same.
#[tokio::test]
async fn edit_policy_to_recurring_respects_tier_gate() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);

    // Pre-create product + non-recurring policy directly via the repo,
    // so we don't need to go through Pro tier just for setup.
    let product = repo::create_product(
        &state.db,
        "rec-edit",
        "Edit Test",
        "",
        100_000,
        &json!({}),
    )
    .await
    .expect("create_product");
    let policy = repo::create_policy(
        &state.db,
        &product.id,
        "Default",
        "default",
        0,
        0,
        1,
        false,
        None,
        &[],
        &json!({}),
        None,
        0,
        None,
        repo::RecurringConfig::off(),
    )
    .await
    .expect("create_policy");

    // Creator-tier attempt to flip is_recurring=true → 402.
    let req = build_request(
        "PATCH",
        &format!("/v1/admin/policies/{}", policy.id),
        &[("authorization", &auth)],
        Some(json!({
            "is_recurring": true,
            "renewal_period_days": 30
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::PAYMENT_REQUIRED,
        "flipping a policy to recurring on Creator tier must 402"
    );

    // Upgrade and try again.
    upgrade_to_pro(&state).await;
    let req = build_request(
        "PATCH",
        &format!("/v1/admin/policies/{}", policy.id),
        &[("authorization", &auth)],
        Some(json!({
            "is_recurring": true,
            "renewal_period_days": 30,
            "trial_days": 7
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "Pro tier can flip a policy to recurring"
    );
    let body = body_json(resp).await;
    assert_eq!(body["is_recurring"], true);
    assert_eq!(body["renewal_period_days"], 30);
    assert_eq!(body["trial_days"], 7);

    // Idempotency: a second PATCH that LEAVES is_recurring true should
    // succeed and not re-fire the tier gate. Drop back to Creator and
    // PATCH a tangential field — must still work.
    *state.self_tier.write().await = Tier::Unlicensed {
        reason: "downgraded".into(),
    };
    let req = build_request(
        "PATCH",
        &format!("/v1/admin/policies/{}", policy.id),
        &[("authorization", &auth)],
        Some(json!({ "name": "Renamed Default" })),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "name-only patch on a recurring policy must not re-fire the tier gate"
    );
}
