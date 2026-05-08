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
