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
    CreateInvoiceParams, CreatedInvoiceHandle, Money, PaymentProvider, ProviderInvoiceSnapshot,
    ProviderInvoiceStatus, ProviderKind, ProviderWebhookEvent,
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
    make_test_state_inner(false).await
}

/// Same fixture but with the daemon sandbox flag ON — for the
/// agent-payment-connect outer gate (a scoped `payment_providers:write` key may
/// only start a connect on a sandbox daemon).
async fn make_test_state_sandbox() -> (AppState, NamedTempFile) {
    make_test_state_inner(true).await
}

async fn make_test_state_inner(sandbox_mode: bool) -> (AppState, NamedTempFile) {
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
        sandbox_mode,
    };

    let state = AppState {
        db: pool,
        keypair: Arc::new(keypair),
        payment: Arc::new(RwLock::new(None)),
        provider_override: None,
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

/// How the mock answers the handler's settle-confirmation re-fetch
/// (`get_invoice_status`).
#[derive(Clone, Copy)]
enum StatusReport {
    /// Report this authoritative status.
    Reports(ProviderInvoiceStatus),
    /// Simulate the provider's status API being unreachable (network error).
    Unavailable,
}

struct MockPaymentProvider {
    next_invoice_id: AtomicU64,
    status_report: StatusReport,
    /// Amount `get_invoice_status` reports the invoice is denominated for.
    /// `None` (the default) = "no opinion", which disables the advisory
    /// settle-amount tripwire; `Some` lets a test drive an amount mismatch.
    settled_amount: Option<Money>,
}

impl MockPaymentProvider {
    /// Happy path: the provider confirms the invoice is settled.
    fn new() -> Self {
        Self {
            next_invoice_id: AtomicU64::new(1),
            status_report: StatusReport::Reports(ProviderInvoiceStatus::Settled),
            settled_amount: None,
        }
    }

    /// Authoritative status does NOT confirm payment, so a `settled` webhook
    /// body is a forgery the handler must refuse.
    fn new_unconfirmed() -> Self {
        Self {
            next_invoice_id: AtomicU64::new(1),
            status_report: StatusReport::Reports(ProviderInvoiceStatus::Pending),
            settled_amount: None,
        }
    }

    /// The provider's status API is unreachable, so the handler can't confirm
    /// a settle and must ack-without-issuing (deferring to the reconciler).
    fn new_status_unavailable() -> Self {
        Self {
            next_invoice_id: AtomicU64::new(1),
            status_report: StatusReport::Unavailable,
            settled_amount: None,
        }
    }

    /// Confirms `Settled` but reports a specific denominated amount, so a test
    /// can exercise the advisory settle-amount tripwire (mismatch → still
    /// issues, but audits).
    fn new_settled_with_amount(amount: Money) -> Self {
        Self {
            next_invoice_id: AtomicU64::new(1),
            status_report: StatusReport::Reports(ProviderInvoiceStatus::Settled),
            settled_amount: Some(amount),
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
    ) -> Result<ProviderInvoiceSnapshot> {
        // The webhook handler re-fetches this to confirm a settle claim
        // before issuing. Configurable per-mock so a test can simulate the
        // provider disagreeing with a forged "settled" body, or being down.
        match self.status_report {
            StatusReport::Reports(s) => Ok(ProviderInvoiceSnapshot {
                status: s,
                amount: self.settled_amount.clone(),
            }),
            StatusReport::Unavailable => {
                anyhow::bail!("mock: provider status API unavailable")
            }
        }
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
    install_mock_provider(MockPaymentProvider::new()).await
}

/// Install a specific `MockPaymentProvider` on a fresh test state, wiring it
/// into both the legacy singleton and the merchant-profile resolver (see the
/// two-seams note below). Lets tests vary the mock's behavior — e.g. an
/// unconfirmed-status mock to exercise the settle-confirmation guard.
async fn install_mock_provider(mock_impl: MockPaymentProvider) -> (AppState, NamedTempFile) {
    let (mut state, tmp) = make_test_state().await;
    let mock: Arc<dyn PaymentProvider> = Arc::new(mock_impl);
    // Two seams, two code paths:
    //  - The legacy singleton (`set_payment_provider`) backs the back-compat
    //    `/v1/{kind}/webhook` route via `state.payment_provider()`.
    //  - The `provider_override` field backs the merchant-profile resolver
    //    (`resolve_provider_for_profile_rail` / `payment_provider_by_id`) that
    //    the real `/v1/purchase` path uses. Both point at the same mock so a
    //    test can drive purchase → settle end-to-end.
    state.set_payment_provider(mock.clone()).await;
    state.provider_override = Some(mock);
    // The resolver still reads profile/rail/row from the DB before swapping in
    // the override, so a real provider row must exist on the default profile —
    // otherwise the purchase path 400s with "no payment providers connected".
    // build_provider is never called for it (the override short-circuits), so
    // the BTCPay credentials here are inert placeholders.
    let default_profile = repo::get_default_merchant_profile(&state.db)
        .await
        .expect("query default profile")
        .expect("migration 0020 auto-creates a default merchant profile");
    repo::create_payment_provider(
        &state.db,
        "test-provider-1",
        &default_profile.id,
        "btcpay",
        "Test BTCPay",
        "inert-test-key",
        "http://btcpay.test",
        None,
        Some("deadbeef"),
        Some("store-test"),
        &Utc::now().to_rfc3339(),
    )
    .await
    .expect("seed test payment provider");
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
    let status = resp.status();
    let body = body_json(resp).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "paid purchase should succeed against the mock provider; body={body:?}"
    );

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

/// Anti-forgery (P0): a `settled` webhook whose provider API does NOT
/// confirm payment must not settle the invoice or issue a license. This is
/// the defense for signature-less providers (Zaprite) — a forged settle
/// POST with a known order id would otherwise mint a free license. The
/// handler re-fetches `get_invoice_status`; the unconfirmed mock reports
/// `Pending`, so the claim is refused: 200 ack (so the provider stops
/// retrying) but no state change and no license.
#[tokio::test]
async fn forged_settle_webhook_without_provider_confirmation_is_refused() {
    let (state, _tmp) =
        install_mock_provider(MockPaymentProvider::new_unconfirmed()).await;

    let product = repo::create_product(
        &state.db,
        "forgery-test",
        "Forgery Test",
        "",
        7_000,
        &json!({}),
    )
    .await
    .expect("create_product");

    let internal_invoice_id = Uuid::new_v4().to_string();
    let provider_invoice_id = "mock-inv-forged".to_string();
    repo::create_invoice(
        &state.db,
        &internal_invoice_id,
        &provider_invoice_id,
        &product.id,
        7_000,
        "http://mock-checkout.test/i/forged",
        None, // buyer_email
        None, // buyer_note
        None, // policy_id
        None, // payment_provider_id
    )
    .await
    .expect("create_invoice");

    let req = build_request(
        "POST",
        "/v1/btcpay/webhook",
        &[("content-type", "application/json")],
        Some(json!({ "kind": "settled", "provider_invoice_id": provider_invoice_id })),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "handler should ack the forged webhook so the provider stops retrying"
    );

    let status_after: String =
        sqlx::query_scalar("SELECT status FROM invoices WHERE btcpay_invoice_id = ?")
            .bind(&provider_invoice_id)
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(
        status_after, "pending",
        "forged settle must NOT flip the invoice to settled"
    );

    let licenses: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM licenses")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(licenses, 0, "forged settle must NOT issue a license");
}

/// When the provider's status API is unreachable, a settle webhook must be
/// acked (200, so the provider doesn't retry-storm) WITHOUT issuing — the
/// reconcile loop re-confirms and issues later. Pins the fail-open-on-ack /
/// fail-closed-on-issuance behavior so a future refactor can't turn this
/// into a 5xx retry storm or, worse, issue on an unconfirmable settle.
#[tokio::test]
async fn settle_webhook_acks_without_issuing_when_provider_unreachable() {
    let (state, _tmp) =
        install_mock_provider(MockPaymentProvider::new_status_unavailable()).await;

    let product = repo::create_product(
        &state.db,
        "unreachable-test",
        "Unreachable Test",
        "",
        6_000,
        &json!({}),
    )
    .await
    .expect("create_product");

    let internal_invoice_id = Uuid::new_v4().to_string();
    let provider_invoice_id = "mock-inv-unreachable".to_string();
    repo::create_invoice(
        &state.db,
        &internal_invoice_id,
        &provider_invoice_id,
        &product.id,
        6_000,
        "http://mock-checkout.test/i/unreachable",
        None, // buyer_email
        None, // buyer_note
        None, // policy_id
        None, // payment_provider_id
    )
    .await
    .expect("create_invoice");

    let req = build_request(
        "POST",
        "/v1/btcpay/webhook",
        &[("content-type", "application/json")],
        Some(json!({ "kind": "settled", "provider_invoice_id": provider_invoice_id })),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "unconfirmable settle must ack 200, not 5xx (a non-2xx triggers retry storms)"
    );

    let status_after: String =
        sqlx::query_scalar("SELECT status FROM invoices WHERE btcpay_invoice_id = ?")
            .bind(&provider_invoice_id)
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(
        status_after, "pending",
        "unconfirmable settle must NOT flip the invoice to settled"
    );

    let licenses: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM licenses")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(
        licenses, 0,
        "unconfirmable settle must NOT issue a license (reconciler handles it later)"
    );
}

/// Advisory settle-amount tripwire (P1): when the provider confirms `Settled`
/// but reports a different amount than we charged, the handler STILL issues
/// the license — the amount check is advisory, NOT a gate — and records an
/// `invoice.amount_mismatch` audit row so the drift is observable. This pins
/// the deliberate non-blocking behavior: a hard gate would false-reject
/// operators running a BTCPay payment tolerance. See docs/guides/payments.md.
#[tokio::test]
async fn settled_amount_mismatch_issues_license_but_audits() {
    let (state, _tmp) =
        install_mock_provider(MockPaymentProvider::new_settled_with_amount(Money::sats(1))).await;

    let product = repo::create_product(
        &state.db,
        "amount-mismatch-test",
        "Amount Mismatch Test",
        "",
        7_000,
        &json!({}),
    )
    .await
    .expect("create_product");

    let internal_invoice_id = Uuid::new_v4().to_string();
    let provider_invoice_id = "mock-inv-mismatch".to_string();
    repo::create_invoice(
        &state.db,
        &internal_invoice_id,
        &provider_invoice_id,
        &product.id,
        7_000,
        "http://mock-checkout.test/i/mismatch",
        None, // buyer_email
        None, // buyer_note
        None, // policy_id
        None, // payment_provider_id
    )
    .await
    .expect("create_invoice");

    let req = build_request(
        "POST",
        "/v1/btcpay/webhook",
        &[("content-type", "application/json")],
        Some(json!({ "kind": "settled", "provider_invoice_id": provider_invoice_id })),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // The settle is confirmed (status Settled), so issuance proceeds despite
    // the amount mismatch — the tripwire is advisory.
    let status_after: String =
        sqlx::query_scalar("SELECT status FROM invoices WHERE btcpay_invoice_id = ?")
            .bind(&provider_invoice_id)
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(status_after, "settled");

    let licenses: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM licenses")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(licenses, 1, "advisory amount mismatch must NOT block issuance");

    // ...but the drift is recorded for the operator to investigate.
    let mismatches: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE action = 'invoice.amount_mismatch'",
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(
        mismatches, 1,
        "amount/currency drift must be recorded in the audit log"
    );
}

/// Fiat-denominated settles have no clean SAT comparison basis, so the advisory
/// tripwire SKIPS them — issues, no audit row. This is the case of a USD
/// subscription renewal, where the provider charges in the listed fiat currency
/// (not sats) and `amount_sats` is not the charged amount. Regression guard for
/// the false-positive a naive SAT comparison would emit on every fiat renewal.
#[tokio::test]
async fn settled_non_sat_settle_skips_amount_tripwire() {
    let (state, _tmp) = install_mock_provider(MockPaymentProvider::new_settled_with_amount(
        Money {
            currency: "USD".to_string(),
            amount: 999,
        },
    ))
    .await;

    let product =
        repo::create_product(&state.db, "non-sat-test", "Non-SAT Test", "", 7_000, &json!({}))
            .await
            .expect("create_product");
    let internal_invoice_id = Uuid::new_v4().to_string();
    let provider_invoice_id = "mock-inv-nonsat".to_string();
    repo::create_invoice(
        &state.db,
        &internal_invoice_id,
        &provider_invoice_id,
        &product.id,
        7_000,
        "http://mock-checkout.test/i/nonsat",
        None,
        None,
        None,
        None,
    )
    .await
    .expect("create_invoice");

    let req = build_request(
        "POST",
        "/v1/btcpay/webhook",
        &[("content-type", "application/json")],
        Some(json!({ "kind": "settled", "provider_invoice_id": provider_invoice_id })),
    );
    assert_eq!(send(&state, req).await.status(), StatusCode::OK);

    let licenses: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM licenses")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(licenses, 1, "non-SAT settle must still issue the license");
    let mismatches: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE action = 'invoice.amount_mismatch'",
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(
        mismatches, 0,
        "non-SAT settle has no SAT comparison basis — skip, do NOT audit as a mismatch"
    );
}

/// When the provider reports no parseable amount (`None`), the tripwire has no
/// opinion and is skipped: the license issues and no `invoice.amount_mismatch`
/// row is written. Pins the "None = skip, not mismatch" contract.
#[tokio::test]
async fn settled_without_provider_amount_skips_tripwire() {
    // make_test_state_with_mock_provider uses MockPaymentProvider::new() —
    // confirms Settled but reports no amount (settled_amount = None).
    let (state, _tmp) = make_test_state_with_mock_provider().await;

    let product =
        repo::create_product(&state.db, "none-amt-test", "None Amt", "", 5_000, &json!({}))
            .await
            .expect("create_product");
    let internal_invoice_id = Uuid::new_v4().to_string();
    let provider_invoice_id = "mock-inv-noneamt".to_string();
    repo::create_invoice(
        &state.db,
        &internal_invoice_id,
        &provider_invoice_id,
        &product.id,
        5_000,
        "http://mock-checkout.test/i/noneamt",
        None,
        None,
        None,
        None,
    )
    .await
    .expect("create_invoice");

    let req = build_request(
        "POST",
        "/v1/btcpay/webhook",
        &[("content-type", "application/json")],
        Some(json!({ "kind": "settled", "provider_invoice_id": provider_invoice_id })),
    );
    assert_eq!(send(&state, req).await.status(), StatusCode::OK);

    let licenses: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM licenses")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(licenses, 1);
    let mismatches: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE action = 'invoice.amount_mismatch'",
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(mismatches, 0, "no provider amount → tripwire skipped, no audit row");
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
        None, // payment_provider_id
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
        None, // payment_provider_id
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
        None,
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

// ---------------------------------------------------------------------
// Subscription cancellation (Phase 6)
//
// Admin cancel: full trust, just needs the bearer token + the sub id.
// Buyer cancel: auth via license key in the body. The cancelled state
// is terminal — license stays valid through end-of-cycle, renewal
// worker stops creating new invoices, webhook fires.
// ---------------------------------------------------------------------

/// Helper: seed a license + active subscription tied to it, plus a
/// product + recurring policy. Returns (license_id, sub_id, key_string)
/// where `key_string` is the signed license key the buyer would have
/// in hand (used by the self-service cancel test).
async fn seed_subscription(state: &AppState) -> (String, String, String) {
    let product = repo::create_product(
        &state.db,
        "sub-cancel-prod",
        "Cancel Test",
        "",
        25_000,
        &json!({}),
    )
    .await
    .expect("create_product");
    let policy = repo::create_policy(
        &state.db,
        &product.id,
        "Monthly",
        "monthly",
        30 * 86_400,
        0,
        1,
        false,
        None,
        &[],
        &json!({}),
        None,
        0,
        None,
        repo::RecurringConfig {
            is_recurring: true,
            renewal_period_days: 30,
            grace_period_days: 7,
            trial_days: 0,
        },
        None,
    )
    .await
    .expect("create_policy");

    let license_id = Uuid::new_v4();
    let issued_at = Utc::now();
    repo::create_license(
        &state.db,
        &license_id.to_string(),
        &product.id,
        None,
        &issued_at.to_rfc3339(),
        &json!({}),
        Some(&policy.id),
        None,
        0,
        1,
        &[],
        false,
        None,
        None,
    )
    .await
    .expect("create_license");

    // Seed a placeholder cycle-1 invoice so the FK on subscription_invoices
    // is satisfiable — the invoice details don't matter for the cancel
    // tests, only that a row exists.
    let invoice_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO invoices(id, btcpay_invoice_id, product_id, amount_sats, \
         checkout_url, status, created_at, updated_at, listed_currency, \
         listed_value, policy_id) \
         VALUES(?, ?, ?, 0, ?, 'pending', ?, ?, 'SAT', 0, ?)",
    )
    .bind(&invoice_id)
    .bind(&format!("test-inv-{}", &invoice_id[..8]))
    .bind(&product.id)
    .bind("http://test.invalid/inv")
    .bind(issued_at.to_rfc3339())
    .bind(issued_at.to_rfc3339())
    .bind(&policy.id)
    .execute(&state.db)
    .await
    .expect("seed invoice");

    let sub = keysat::subscriptions::create_subscription(
        &state.db,
        &license_id.to_string(),
        &policy.id,
        &product.id,
        30,
        "SAT",
        25_000,
        &invoice_id,
        None, // merchant_profile_id
        None, // payment_provider_id
    )
    .await
    .expect("create_subscription");

    // Build a real signed key the buyer-cancel endpoint can verify.
    let product_uuid = Uuid::parse_str(&product.id).expect("product id is uuid");
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

    (license_id.to_string(), sub.id, key_string)
}

#[tokio::test]
async fn admin_cancel_subscription_happy_path() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);
    let (_license_id, sub_id, _key) = seed_subscription(&state).await;

    // Cancel.
    let req = build_request(
        "POST",
        &format!("/v1/admin/subscriptions/{}/cancel", sub_id),
        &[("authorization", &auth)],
        Some(json!({"reason": "customer requested"})),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["ok"], true);
    assert_eq!(body["status"], "cancelled");

    // DB row reflects the new state + cancelled_at is stamped.
    let (status, cancelled_at): (String, Option<String>) = sqlx::query_as(
        "SELECT status, cancelled_at FROM subscriptions WHERE id = ?",
    )
    .bind(&sub_id)
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(status, "cancelled");
    assert!(cancelled_at.is_some(), "cancelled_at must be stamped");

    // Audit row exists.
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE action = 'subscription.cancel' \
         AND target_id = ?",
    )
    .bind(&sub_id)
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(n, 1, "exactly one audit row for the cancel");

    // Idempotency: cancelling a cancelled sub returns ok with the prior state.
    let req = build_request(
        "POST",
        &format!("/v1/admin/subscriptions/{}/cancel", sub_id),
        &[("authorization", &auth)],
        Some(json!({})),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["already"], "cancelled");
}

#[tokio::test]
async fn admin_cancel_unknown_subscription_404s() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);

    let req = build_request(
        "POST",
        "/v1/admin/subscriptions/no-such-sub/cancel",
        &[("authorization", &auth)],
        Some(json!({})),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn buyer_cancel_subscription_via_license_key() {
    let (state, _tmp) = make_test_state().await;
    let (_license_id, sub_id, key_string) = seed_subscription(&state).await;

    // Buyer self-cancels by POSTing the signed key. No admin auth.
    let req = build_request(
        "POST",
        "/v1/subscriptions/cancel",
        &[],
        Some(json!({
            "license_key": key_string,
            "reason": "no longer needed"
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "buyer cancel should succeed with a valid key"
    );
    let body = body_json(resp).await;
    assert_eq!(body["status"], "cancelled");
    assert_eq!(body["subscription_id"], sub_id);

    // Audit row carries actor=buyer.
    let actor: Option<String> = sqlx::query_scalar(
        "SELECT actor_kind FROM audit_log WHERE target_id = ? \
         AND action = 'subscription.cancel'",
    )
    .bind(&sub_id)
    .fetch_optional(&state.db)
    .await
    .unwrap();
    assert_eq!(
        actor.as_deref(),
        Some("buyer_license_key"),
        "audit must record the buyer-key actor kind"
    );
}

// ---------------------------------------------------------------------
// Tier upgrade endpoints (Phase 3 of TIER_UPGRADES_DESIGN)
// ---------------------------------------------------------------------

/// Seed a USD perpetual product with Standard (rank 1) + Pro (rank 2)
/// policies, plus a license under Standard with a real signed key the
/// buyer would hold. Returns (license_id, key_string, standard_id, pro_id).
async fn seed_perpetual_ladder_with_key(state: &AppState) -> (String, String, String, String) {
    let product = repo::create_product(
        &state.db,
        "upgrade-test",
        "Upgrade Test",
        "",
        2500,
        &json!({}),
    )
    .await
    .expect("create_product");
    sqlx::query("UPDATE products SET price_currency='USD', price_value=2500 WHERE id = ?")
        .bind(&product.id)
        .execute(&state.db)
        .await
        .unwrap();
    let standard = repo::create_policy(
        &state.db,
        &product.id,
        "Standard",
        "standard",
        0,
        0,
        1,
        false,
        Some(2500),
        &["core".into()],
        &json!({}),
        None,
        0,
        None,
        repo::RecurringConfig::off(),
        Some(1),
    )
    .await
    .expect("create standard");
    let pro = repo::create_policy(
        &state.db,
        &product.id,
        "Pro",
        "pro",
        0,
        0,
        3,
        false,
        Some(7500),
        &["core".into(), "ai_summaries".into()],
        &json!({}),
        None,
        0,
        None,
        repo::RecurringConfig::off(),
        Some(2),
    )
    .await
    .expect("create pro");

    let license_id = Uuid::new_v4();
    let issued_at = Utc::now();
    repo::create_license(
        &state.db,
        &license_id.to_string(),
        &product.id,
        None,
        &issued_at.to_rfc3339(),
        &json!({}),
        Some(&standard.id),
        None,
        0,
        1,
        &["core".to_string()],
        false,
        None,
        None,
    )
    .await
    .expect("create_license");

    let product_uuid = Uuid::parse_str(&product.id).expect("product id is uuid");
    let payload = LicensePayload {
        version: 2,
        flags: 0,
        product_id: product_uuid,
        license_id,
        issued_at: issued_at.timestamp(),
        expires_at: 0,
        fingerprint_hash: [0; 32],
        entitlements: vec!["core".into()],
    };
    let signature = crypto::sign_payload(&state.keypair.signing, &payload);
    let key_string = crypto::encode_key(&payload, &signature);

    (license_id.to_string(), key_string, standard.id, pro.id)
}

/// `/v1/upgrade-quote` returns the prorated charge for a valid
/// license + target combo.
#[tokio::test]
async fn upgrade_quote_returns_perpetual_difference() {
    let (state, _tmp) = make_test_state().await;
    let (_lic, key, _std, _pro) = seed_perpetual_ladder_with_key(&state).await;

    let req = build_request(
        "POST",
        "/v1/upgrade-quote",
        &[],
        Some(json!({
            "license_key": key,
            "target_policy_slug": "pro"
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["direction"], "upgrade");
    assert_eq!(body["listed_currency"], "USD");
    // Pro $75 - Standard $25 = $50 = 5000 cents.
    assert_eq!(body["proration_charge_value"], 5000);
    assert_eq!(body["effective_at"], "immediate");
}

#[tokio::test]
async fn upgrade_quote_rejects_garbage_key() {
    let (state, _tmp) = make_test_state().await;
    let req = build_request(
        "POST",
        "/v1/upgrade-quote",
        &[],
        Some(json!({
            "license_key": "not-a-real-key",
            "target_policy_slug": "pro"
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn upgrade_quote_rejects_unknown_target_policy() {
    let (state, _tmp) = make_test_state().await;
    let (_lic, key, _, _) = seed_perpetual_ladder_with_key(&state).await;
    let req = build_request(
        "POST",
        "/v1/upgrade-quote",
        &[],
        Some(json!({
            "license_key": key,
            "target_policy_slug": "no-such-policy"
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// `/v1/upgrade` against a paid path: creates a real provider invoice
/// (mock), persists a tier_changes row, returns checkout URL.
#[tokio::test]
async fn upgrade_start_creates_invoice_and_tier_change_row() {
    let (state, _tmp) = make_test_state_with_mock_provider().await;
    // Pin a USD/BTC rate so the rates fetcher doesn't try the network
    // when we hit the upgrade path.
    sqlx::query(
        "INSERT INTO settings(key, value, updated_at) \
         VALUES('manual_rate_pin_USD', '50000', ?)",
    )
    .bind(Utc::now().to_rfc3339())
    .execute(&state.db)
    .await
    .unwrap();

    let (license_id, key, _std, pro_id) = seed_perpetual_ladder_with_key(&state).await;

    let req = build_request(
        "POST",
        "/v1/upgrade",
        &[],
        Some(json!({
            "license_key": key,
            "target_policy_slug": "pro"
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "upgrade start should succeed; got {}",
        resp.status()
    );
    let body = body_json(resp).await;
    let invoice_id = body["invoice_id"].as_str().expect("invoice_id").to_string();
    assert!(body["checkout_url"].as_str().unwrap().contains("mock-checkout"));
    assert_eq!(body["proration_charge_value"], 5000); // 5000 cents
    assert!(body["amount_sats"].as_i64().unwrap() > 0,
        "fiat conversion should produce a non-zero sat charge");

    // tier_changes row exists with this invoice_id.
    let tc = keysat::upgrades::get_tier_change_by_invoice(&state.db, &invoice_id)
        .await
        .unwrap()
        .expect("tier_change row");
    assert_eq!(tc.license_id, license_id);
    assert_eq!(tc.to_policy_id, pro_id);
    assert_eq!(tc.actor, "buyer");
    assert_eq!(tc.direction, "upgrade");
    assert_eq!(tc.invoice_id.as_deref(), Some(invoice_id.as_str()));

    // License is NOT yet on Pro — that happens on settle (next test).
    let license_now = repo::get_license_by_id(&state.db, &license_id)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        license_now.policy_id.as_deref(),
        Some(pro_id.as_str()),
        "license should NOT change tier until invoice settles"
    );
}

/// Webhook settle on a tier-change invoice applies the change instead
/// of issuing a new license.
#[tokio::test]
async fn webhook_settle_on_tier_change_applies_instead_of_issuing() {
    let (state, _tmp) = make_test_state_with_mock_provider().await;
    sqlx::query(
        "INSERT INTO settings(key, value, updated_at) \
         VALUES('manual_rate_pin_USD', '50000', ?)",
    )
    .bind(Utc::now().to_rfc3339())
    .execute(&state.db)
    .await
    .unwrap();

    let (license_id, key, _std, pro_id) = seed_perpetual_ladder_with_key(&state).await;

    // Start the upgrade, capture the provider invoice id.
    let req = build_request(
        "POST",
        "/v1/upgrade",
        &[],
        Some(json!({
            "license_key": key,
            "target_policy_slug": "pro"
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let invoice_id = body["invoice_id"].as_str().unwrap().to_string();
    let provider_invoice_id = body["provider_invoice_id"].as_str().unwrap().to_string();

    // Fire a "settled" webhook on that invoice. The MockPaymentProvider's
    // validate_webhook reads the body as JSON.
    let req = build_request(
        "POST",
        "/v1/btcpay/webhook",
        &[],
        Some(json!({
            "kind": "settled",
            "provider_invoice_id": provider_invoice_id
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "webhook should ack 200 on tier-change settle"
    );

    // The license is now on Pro. No NEW license was issued (count
    // for this product still 1).
    let license_after = repo::get_license_by_id(&state.db, &license_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        license_after.policy_id.as_deref(),
        Some(pro_id.as_str()),
        "settle webhook should have applied the tier change"
    );
    assert!(
        license_after.entitlements.contains(&"ai_summaries".to_string()),
        "Pro entitlements should now be on the license: {:?}",
        license_after.entitlements
    );

    let n_licenses: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM licenses WHERE product_id = ?",
    )
    .bind(&license_after.product_id)
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(
        n_licenses, 1,
        "tier-change must NOT issue a new license; count must stay at 1"
    );

    // Re-delivering the same webhook is idempotent.
    let req = build_request(
        "POST",
        "/v1/btcpay/webhook",
        &[],
        Some(json!({
            "kind": "settled",
            "provider_invoice_id": provider_invoice_id
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "re-delivery must ack 200");
    let n_licenses_after: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM licenses WHERE product_id = ?",
    )
    .bind(&license_after.product_id)
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(n_licenses_after, 1, "re-delivery must not duplicate licenses");

    // Suppress unused-var warning: invoice_id is used implicitly via
    // the tier_changes lookup but kept named for readability.
    let _ = invoice_id;
}

/// Admin can force-change a license to any policy under the same
/// product. skip_payment=true applies immediately with no invoice.
#[tokio::test]
async fn admin_change_tier_skip_payment_applies_immediately() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);
    let (license_id, _key, _std, pro_id) = seed_perpetual_ladder_with_key(&state).await;

    let req = build_request(
        "POST",
        &format!("/v1/admin/licenses/{license_id}/change-tier"),
        &[("authorization", &auth)],
        Some(json!({
            "to_policy_slug": "pro",
            "skip_payment": true,
            "reason": "comp upgrade per support ticket #1234"
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["applied"], true);
    assert_eq!(body["skip_payment"], true);
    let tc_id = body["tier_change_id"].as_str().unwrap().to_string();

    let license_after = repo::get_license_by_id(&state.db, &license_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        license_after.policy_id.as_deref(),
        Some(pro_id.as_str()),
        "skip_payment=true should apply on the spot"
    );

    let tc = keysat::upgrades::get_tier_change(&state.db, &tc_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tc.actor, "admin");
    assert_eq!(tc.proration_charge_value, 0);
    assert_eq!(tc.invoice_id, None, "comp upgrade has no invoice");
    assert_eq!(
        tc.reason.as_deref(),
        Some("comp upgrade per support ticket #1234")
    );
}

/// Admin can force a perpetual downgrade. Buyer endpoint rejects
/// these (refund decision per design doc).
#[tokio::test]
async fn admin_change_tier_allows_perpetual_downgrade() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);
    let (license_id, _key, std_id, pro_id) = seed_perpetual_ladder_with_key(&state).await;
    sqlx::query("UPDATE licenses SET policy_id = ? WHERE id = ?")
        .bind(&pro_id)
        .bind(&license_id)
        .execute(&state.db)
        .await
        .unwrap();
    let req = build_request(
        "POST",
        &format!("/v1/admin/licenses/{license_id}/change-tier"),
        &[("authorization", &auth)],
        Some(json!({
            "to_policy_slug": "standard",
            "skip_payment": true,
            "reason": "honoring partial refund"
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let license_after = repo::get_license_by_id(&state.db, &license_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(license_after.policy_id.as_deref(), Some(std_id.as_str()));
}

#[tokio::test]
async fn admin_change_tier_rejects_zero_charge_paid_path() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);
    let (license_id, _key, std_id, _pro) = seed_perpetual_ladder_with_key(&state).await;
    let std_policy = repo::get_policy_by_id(&state.db, &std_id).await.unwrap().unwrap();
    let _sideways = repo::create_policy(
        &state.db,
        &std_policy.product_id,
        "Standard Plus",
        "standard-plus",
        0,
        0,
        1,
        false,
        Some(2500),
        &["core".into()],
        &json!({}),
        None,
        0,
        None,
        repo::RecurringConfig::off(),
        Some(1),
    )
    .await
    .unwrap();
    let req = build_request(
        "POST",
        &format!("/v1/admin/licenses/{license_id}/change-tier"),
        &[("authorization", &auth)],
        Some(json!({
            "to_policy_slug": "standard-plus",
            "skip_payment": false
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert!(
        body["message"].as_str().unwrap_or("").contains("skip_payment"),
        "error should hint at the skip_payment toggle: {body:?}"
    );
}

#[tokio::test]
async fn admin_change_tier_requires_admin_token() {
    let (state, _tmp) = make_test_state().await;
    let (license_id, _key, _std, _pro) = seed_perpetual_ladder_with_key(&state).await;
    let req = build_request(
        "POST",
        &format!("/v1/admin/licenses/{license_id}/change-tier"),
        &[],
        Some(json!({"to_policy_slug": "pro", "skip_payment": true})),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Buyer-initiated downgrade is rejected from this endpoint in v0.2.x
/// (Phase 4 admin endpoint covers downgrades).
#[tokio::test]
async fn upgrade_endpoint_rejects_buyer_downgrade() {
    let (state, _tmp) = make_test_state().await;
    let (lic, _key, std_id, pro_id) = seed_perpetual_ladder_with_key(&state).await;

    // Move the license to Pro by direct SQL so we can attempt a
    // downgrade back to Standard. (Real flow: admin would have done
    // this; we don't have an admin-change-tier endpoint until Phase 4.)
    sqlx::query("UPDATE licenses SET policy_id = ? WHERE id = ?")
        .bind(&pro_id)
        .bind(&lic)
        .execute(&state.db)
        .await
        .unwrap();

    // Re-sign a key for the now-Pro license. We can reuse the same
    // license_id + product_id — the entitlements in the payload are
    // not checked by the upgrade endpoint (it goes by license_id).
    let license = repo::get_license_by_id(&state.db, &lic).await.unwrap().unwrap();
    let product_uuid = Uuid::parse_str(&license.product_id).unwrap();
    let payload = LicensePayload {
        version: 2,
        flags: 0,
        product_id: product_uuid,
        license_id: Uuid::parse_str(&lic).unwrap(),
        issued_at: Utc::now().timestamp(),
        expires_at: 0,
        fingerprint_hash: [0; 32],
        entitlements: vec![],
    };
    let signature = crypto::sign_payload(&state.keypair.signing, &payload);
    let key_string = crypto::encode_key(&payload, &signature);

    let req = build_request(
        "POST",
        "/v1/upgrade",
        &[],
        Some(json!({
            "license_key": key_string,
            "target_policy_slug": "standard"
        })),
    );
    let resp = send(&state, req).await;
    // The quote function intercepts perpetual downgrades with a 400
    // "admin-only" before the endpoint's blanket-Forbidden check
    // fires. Either status is "this is not a buyer path"; the
    // message-level distinction matters more than the code.
    let status = resp.status();
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::FORBIDDEN,
        "buyer-initiated downgrade must be 400 or 403; got {status}"
    );
    if status == StatusCode::BAD_REQUEST {
        let body = body_json(resp).await;
        assert!(
            body["message"].as_str().unwrap_or("").contains("admin-only"),
            "400 should explain that downgrades are admin-only: {body:?}"
        );
    }

    let _ = std_id;
}

#[tokio::test]
async fn buyer_cancel_rejects_garbage_key() {
    let (state, _tmp) = make_test_state().await;
    let _ = seed_subscription(&state).await;

    let req = build_request(
        "POST",
        "/v1/subscriptions/cancel",
        &[],
        Some(json!({"license_key": "not-a-real-key"})),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "garbage key must be 401, not 404 — don't leak which subs exist"
    );
}

// ---------------------------------------------------------------------
// 0.2.0:12 — Scoped API keys + OpenAPI spec + Zaprite gate
// ---------------------------------------------------------------------

/// `GET /v1/openapi.json` — public, no auth. Returns a parseable spec
/// with the agent-relevant subset of endpoints documented.
#[tokio::test]
async fn openapi_spec_serves_valid_json() {
    let (state, _tmp) = make_test_state().await;
    let req = build_request("GET", "/v1/openapi.json", &[], None);
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["openapi"], "3.1.0");
    assert!(v["paths"].as_object().expect("paths is object").len() > 5);
    // Spot-check that the agent-relevant endpoints are present.
    assert!(v.pointer("/paths/~1v1~1admin~1api-keys").is_some());
    assert!(v.pointer("/paths/~1v1~1admin~1licenses").is_some());
    assert!(v.pointer("/paths/~1v1~1validate").is_some());
}

/// `POST /v1/admin/api-keys` — master admin creates a scoped key, the
/// raw token comes back once, and the role is recorded. Subsequent
/// `GET /v1/admin/api-keys` lists it without the token.
#[tokio::test]
async fn scoped_api_key_create_list_revoke_round_trip() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);

    // Create with a recognized role.
    let req = build_request(
        "POST",
        "/v1/admin/api-keys",
        &[("authorization", &auth)],
        Some(json!({"label": "Smoke test bot", "role": "license-issuer"})),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let token = body["token"].as_str().expect("token returned");
    assert!(token.starts_with("ks_"), "scoped token must use ks_ prefix");
    let key_id = body["id"].as_str().expect("id returned").to_string();
    assert_eq!(body["role"], "license-issuer");

    // List sees the new key but never the raw token.
    let req = build_request("GET", "/v1/admin/api-keys", &[("authorization", &auth)], None);
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    let keys = list["api_keys"].as_array().expect("api_keys array");
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["label"], "Smoke test bot");
    assert!(keys[0].get("token").is_none(), "list must not return raw tokens");

    // Revoke. Idempotent on second call.
    let path = format!("/v1/admin/api-keys/{}", key_id);
    let req = build_request("DELETE", &path, &[("authorization", &auth)], None);
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let req = build_request("DELETE", &path, &[("authorization", &auth)], None);
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["already_revoked"], true);
}

/// Create endpoint rejects unknown role with 400.
#[tokio::test]
async fn scoped_api_key_create_rejects_unknown_role() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);
    let req = build_request(
        "POST",
        "/v1/admin/api-keys",
        &[("authorization", &auth)],
        Some(json!({"label": "bad role", "role": "god-mode"})),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// `POST /v1/admin/api-keys` requires master admin, NOT a scoped
/// full-admin key — generating other API keys is a self-elevation path
/// that scoped keys are deliberately denied.
#[tokio::test]
async fn scoped_api_key_management_rejects_scoped_full_admin() {
    let (state, _tmp) = make_test_state().await;
    let master = format!("Bearer {}", TEST_ADMIN_KEY);

    // Master creates a full-admin scoped key.
    let req = build_request(
        "POST",
        "/v1/admin/api-keys",
        &[("authorization", &master)],
        Some(json!({"label": "Tries to elevate", "role": "full-admin"})),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let scoped_token = body["token"].as_str().expect("token").to_string();
    let scoped_auth = format!("Bearer {}", scoped_token);

    // Scoped full-admin tries to create another key. Should 403 — the
    // /v1/admin/api-keys handler calls require_admin, not require_scope.
    let req = build_request(
        "POST",
        "/v1/admin/api-keys",
        &[("authorization", &scoped_auth)],
        Some(json!({"label": "Pwn", "role": "read-only"})),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "scoped keys (even full-admin) must NOT manage other keys"
    );
}

/// Mint a scoped API key of `role` via the master-authed create endpoint and
/// return its raw bearer token. Exercises the real issue path the same way an
/// operator would.
async fn mint_scoped_key(state: &AppState, role: &str) -> String {
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);
    let req = build_request(
        "POST",
        "/v1/admin/api-keys",
        &[("authorization", &auth)],
        Some(json!({ "label": format!("{role} key"), "role": role })),
    );
    let resp = send(state, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "minting a {role} key should succeed");
    body_json(resp)
        .await
        .get("token")
        .and_then(|t| t.as_str())
        .expect("create returns the raw token once")
        .to_string()
}

/// Read-only scoped keys can hit read endpoints but are 403 on writes, and are
/// still denied the endpoints we deliberately keep master-only (db-info).
#[tokio::test]
async fn scoped_read_only_key_reads_but_cannot_write() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", mint_scoped_key(&state, "read-only").await);

    // Read endpoint — allowed (every role grants `:read`). Use a param-free
    // getter so the only gate exercised is the scope check (GET
    // /v1/admin/licenses requires a product_id query param that 400s at the
    // extractor before auth even runs).
    let req = build_request(
        "GET",
        "/v1/admin/settings/operator-name",
        &[("authorization", &auth)],
        None,
    );
    assert_eq!(send(&state, req).await.status(), StatusCode::OK);

    // db-info stays master-only even for reads.
    let req = build_request("GET", "/v1/admin/db-info", &[("authorization", &auth)], None);
    assert_eq!(
        send(&state, req).await.status(),
        StatusCode::FORBIDDEN,
        "db-info is master-only; a read-only scoped key must be denied"
    );

    // Write endpoint — denied (products:write is full-admin only).
    let req = build_request(
        "POST",
        "/v1/admin/products",
        &[("authorization", &auth)],
        Some(json!({ "slug": "ro-denied", "name": "Nope", "price_sats": 1000 })),
    );
    assert_eq!(send(&state, req).await.status(), StatusCode::FORBIDDEN);
}

/// License-issuer scoped keys can issue licenses (licenses:write) but cannot
/// manage the catalog (products:write is full-admin only).
#[tokio::test]
async fn scoped_license_issuer_key_issues_but_cannot_manage_catalog() {
    let (state, _tmp) = make_test_state().await;
    let master = format!("Bearer {}", TEST_ADMIN_KEY);

    // Master seeds a product to issue against.
    let req = build_request(
        "POST",
        "/v1/admin/products",
        &[("authorization", &master)],
        Some(json!({ "slug": "issuer-prod", "name": "Issuer Prod", "price_sats": 1000 })),
    );
    assert_eq!(send(&state, req).await.status(), StatusCode::OK);

    let auth = format!("Bearer {}", mint_scoped_key(&state, "license-issuer").await);

    // Issue a license — allowed.
    let req = build_request(
        "POST",
        "/v1/admin/licenses",
        &[("authorization", &auth)],
        Some(json!({ "product_slug": "issuer-prod" })),
    );
    assert_eq!(
        send(&state, req).await.status(),
        StatusCode::OK,
        "license-issuer must be able to issue licenses"
    );

    // Create a product — denied.
    let req = build_request(
        "POST",
        "/v1/admin/products",
        &[("authorization", &auth)],
        Some(json!({ "slug": "issuer-cant", "name": "Nope", "price_sats": 1000 })),
    );
    assert_eq!(
        send(&state, req).await.status(),
        StatusCode::FORBIDDEN,
        "license-issuer must NOT manage the catalog"
    );
}

/// Support scoped keys are granted subscription/machine writes but not catalog
/// writes. The cancel of a nonexistent subscription is expected to fail
/// downstream (not found) — what matters is that authorization PASSED (not
/// 401/403), which isolates the scope grant from the business logic.
#[tokio::test]
async fn scoped_support_key_allowed_support_writes_not_catalog() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", mint_scoped_key(&state, "support").await);

    // subscriptions:write — auth passes; missing sub yields a non-403/401 status.
    let req = build_request(
        "POST",
        "/v1/admin/subscriptions/does-not-exist/cancel",
        &[("authorization", &auth)],
        None,
    );
    let status = send(&state, req).await.status();
    assert_ne!(status, StatusCode::FORBIDDEN, "support is granted subscriptions:write");
    assert_ne!(status, StatusCode::UNAUTHORIZED);

    // Catalog write — denied.
    let req = build_request(
        "POST",
        "/v1/admin/products",
        &[("authorization", &auth)],
        Some(json!({ "slug": "sup-cant", "name": "Nope", "price_sats": 1000 })),
    );
    assert_eq!(send(&state, req).await.status(), StatusCode::FORBIDDEN);
}

/// Full-admin scoped keys CAN manage the catalog (products:write). The
/// master-only denial (minting other keys, etc.) is covered by
/// `scoped_api_key_management_rejects_scoped_full_admin`.
#[tokio::test]
async fn scoped_full_admin_key_manages_catalog() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", mint_scoped_key(&state, "full-admin").await);

    let req = build_request(
        "POST",
        "/v1/admin/products",
        &[("authorization", &auth)],
        Some(json!({ "slug": "fa-prod", "name": "FA Prod", "price_sats": 1000 })),
    );
    assert_eq!(
        send(&state, req).await.status(),
        StatusCode::OK,
        "full-admin must be able to manage the catalog"
    );
}

/// Merchant-onboard scoped keys can run the full self-serve onboarding chain
/// with their OWN credential — create a product, define a policy/tier, and
/// issue a license against it (products:write + policies:write +
/// licenses:write) — WITHOUT the master key. They must still be denied every
/// master-only gate (db-info, minting other keys) and the support writes they
/// don't need (subscriptions:write), which keeps the role least-privilege and
/// non-escalating.
#[tokio::test]
async fn scoped_merchant_onboard_key_onboards_but_not_master() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", mint_scoped_key(&state, "merchant-onboard").await);

    // 1. Create a product — allowed (products:write). Note: the key itself
    //    creates it, not the master — that's the whole point of the role.
    let req = build_request(
        "POST",
        "/v1/admin/products",
        &[("authorization", &auth)],
        Some(json!({ "slug": "onboard-prod", "name": "Onboard Prod", "price_sats": 1000 })),
    );
    assert_eq!(
        send(&state, req).await.status(),
        StatusCode::OK,
        "merchant-onboard must be able to create products"
    );

    // 2. Define a policy/tier on it — allowed (policies:write). Non-recurring
    //    so the Creator-tier recurring gate (402) doesn't fire.
    let req = build_request(
        "POST",
        "/v1/admin/policies",
        &[("authorization", &auth)],
        Some(json!({
            "product_slug": "onboard-prod",
            "name": "Standard",
            "slug": "standard",
            "duration_seconds": 0,
            "max_machines": 1
        })),
    );
    assert_eq!(
        send(&state, req).await.status(),
        StatusCode::OK,
        "merchant-onboard must be able to define policies"
    );

    // 3. Issue a license against it — allowed (licenses:write).
    let req = build_request(
        "POST",
        "/v1/admin/licenses",
        &[("authorization", &auth)],
        Some(json!({ "product_slug": "onboard-prod", "policy_slug": "standard" })),
    );
    assert_eq!(
        send(&state, req).await.status(),
        StatusCode::OK,
        "merchant-onboard must be able to issue licenses"
    );

    // 4. Master-only gates stay denied — no escalation path.
    let req = build_request("GET", "/v1/admin/db-info", &[("authorization", &auth)], None);
    assert_eq!(
        send(&state, req).await.status(),
        StatusCode::FORBIDDEN,
        "db-info is master-only; merchant-onboard must be denied"
    );
    let req = build_request(
        "POST",
        "/v1/admin/api-keys",
        &[("authorization", &auth)],
        Some(json!({ "label": "tries to elevate", "role": "full-admin" })),
    );
    assert_eq!(
        send(&state, req).await.status(),
        StatusCode::FORBIDDEN,
        "merchant-onboard must NOT mint other keys (self-elevation guard)"
    );

    // 5. Support writes it doesn't need stay denied — least-privilege boundary
    //    on the other side (this is what separates it from the support role).
    let req = build_request(
        "POST",
        "/v1/admin/subscriptions/does-not-exist/cancel",
        &[("authorization", &auth)],
        None,
    );
    assert_eq!(
        send(&state, req).await.status(),
        StatusCode::FORBIDDEN,
        "merchant-onboard must NOT have subscriptions:write"
    );
}

/// À-la-carte `payment_providers:write` can be granted on a key via the `scopes`
/// field (it's in no role), and round-trips through create + list. This is the
/// per-key grant mechanism the agent-payment-connect gate (slices 3+) builds on.
#[tokio::test]
async fn scoped_key_extra_scopes_round_trip() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);

    // Create a key with the à-la-carte scope on top of a read-only role.
    let req = build_request(
        "POST",
        "/v1/admin/api-keys",
        &[("authorization", &auth)],
        Some(json!({
            "label": "Sandbox connect bot",
            "role": "merchant-onboard",
            "scopes": ["payment_providers:write"]
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let scopes = body["scopes"].as_array().expect("scopes echoed on create");
    assert!(
        scopes.iter().any(|s| s == "payment_providers:write"),
        "create echoes the granted à-la-carte scope; got {scopes:?}"
    );
    let key_id = body["id"].as_str().unwrap().to_string();

    // List shows the same scope on the key entry.
    let req = build_request("GET", "/v1/admin/api-keys", &[("authorization", &auth)], None);
    let body = body_json(send(&state, req).await).await;
    let entry = body["api_keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == key_id.as_str())
        .expect("created key appears in list");
    let scopes = entry["scopes"].as_array().expect("list entry carries scopes");
    assert!(
        scopes.iter().any(|s| s == "payment_providers:write"),
        "list echoes the granted à-la-carte scope; got {scopes:?}"
    );
}

/// Create rejects any scope that isn't in the à-la-carte allowlist — a typo'd
/// or arbitrary scope string is a 400, never silently granted or dropped.
#[tokio::test]
async fn scoped_key_create_rejects_ungrantable_scope() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);
    let req = build_request(
        "POST",
        "/v1/admin/api-keys",
        &[("authorization", &auth)],
        Some(json!({
            "label": "Overreach",
            "role": "read-only",
            "scopes": ["billing:nuke"]
        })),
    );
    assert_eq!(send(&state, req).await.status(), StatusCode::BAD_REQUEST);
}

/// Mint a scoped key of `role` plus à-la-carte `scopes`, returning its token.
async fn mint_scoped_key_with_scopes(state: &AppState, role: &str, scopes: &[&str]) -> String {
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);
    let req = build_request(
        "POST",
        "/v1/admin/api-keys",
        &[("authorization", &auth)],
        Some(json!({ "label": format!("{role}+scopes key"), "role": role, "scopes": scopes })),
    );
    let resp = send(state, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "minting {role}+{scopes:?} should succeed");
    body_json(resp)
        .await
        .get("token")
        .and_then(|t| t.as_str())
        .expect("create returns the raw token once")
        .to_string()
}

// ----- agent-payment-connect gate (slices 3-4) -----
// `plans/agent-payment-connect-scope.md`: a scoped `payment_providers:write`
// key may START a BTCPay connect ONLY on a sandbox daemon (outer gate); the
// non-mainnet inner gate is enforced at callback time (covered live in
// tests/btcpay_network_live.rs). These cover the HTTP-level outer gate.

/// OUTER gate, production: a scoped `payment_providers:write` key is 403 on a
/// non-sandbox daemon — even though it holds the scope. Proves §5.1 (a scoped
/// key cannot repoint settlement on a live box, regtest or otherwise).
#[tokio::test]
async fn payment_connect_outer_gate_denies_scoped_on_production() {
    let (state, _tmp) = make_test_state().await; // sandbox_mode = false
    let token =
        mint_scoped_key_with_scopes(&state, "merchant-onboard", &["payment_providers:write"]).await;
    let auth = format!("Bearer {token}");
    let req = build_request("POST", "/v1/admin/btcpay/connect", &[("authorization", &auth)], None);
    assert_eq!(
        send(&state, req).await.status(),
        StatusCode::FORBIDDEN,
        "scoped payment_providers:write key must be 403 on a non-sandbox daemon"
    );
}

/// OUTER gate, sandbox: the same key passes on a sandbox daemon, and the
/// connect is recorded as a SCOPED initiator so the callback applies the
/// non-mainnet network gate.
#[tokio::test]
async fn payment_connect_outer_gate_allows_scoped_on_sandbox() {
    let (state, _tmp) = make_test_state_sandbox().await; // sandbox_mode = true
    let token =
        mint_scoped_key_with_scopes(&state, "merchant-onboard", &["payment_providers:write"]).await;
    let auth = format!("Bearer {token}");
    let req = build_request("POST", "/v1/admin/btcpay/connect", &[("authorization", &auth)], None);
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "scoped key passes the outer gate on sandbox");
    let body = body_json(resp).await;
    assert!(
        body["authorize_url"].as_str().unwrap_or("").contains("/api-keys/authorize"),
        "returns a BTCPay authorize URL; got {body:?}"
    );
    let (scoped, actor_hash): (i64, Option<String>) = sqlx::query_as(
        "SELECT scoped_initiator, initiator_actor_hash FROM btcpay_authorize_state WHERE state_token = ?",
    )
    .bind(body["state"].as_str().expect("state token echoed"))
    .fetch_one(&state.db)
    .await
    .expect("authorize_state row persisted");
    assert_eq!(scoped, 1, "the callback must see this as a scoped initiator");
    assert!(
        actor_hash.is_some(),
        "the scoped initiator's actor hash must be recorded for the callback's audit row"
    );
}

/// A scoped key WITHOUT `payment_providers:write` is 403 even on a sandbox
/// daemon — the scope is in no role (not even full-admin), so merchant-onboard
/// can't reach connect. Proves the gate isn't widened by role.
#[tokio::test]
async fn payment_connect_denies_scoped_without_the_scope() {
    let (state, _tmp) = make_test_state_sandbox().await; // sandbox ON, so only the missing scope can deny
    let auth = format!("Bearer {}", mint_scoped_key(&state, "merchant-onboard").await);
    let req = build_request("POST", "/v1/admin/btcpay/connect", &[("authorization", &auth)], None);
    assert_eq!(
        send(&state, req).await.status(),
        StatusCode::FORBIDDEN,
        "merchant-onboard without payment_providers:write must be 403 (no role widening)"
    );
}

/// The master key may start a connect on ANY daemon (bypasses the sandbox
/// gate). Recorded as a master (non-scoped) initiator → callback applies no
/// network restriction.
#[tokio::test]
async fn payment_connect_allows_master_on_production() {
    let (state, _tmp) = make_test_state().await; // sandbox OFF
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);
    let req = build_request("POST", "/v1/admin/btcpay/connect", &[("authorization", &auth)], None);
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "master may connect on any daemon");
    let body = body_json(resp).await;
    let scoped: i64 = sqlx::query_scalar(
        "SELECT scoped_initiator FROM btcpay_authorize_state WHERE state_token = ?",
    )
    .bind(body["state"].as_str().unwrap())
    .fetch_one(&state.db)
    .await
    .expect("authorize_state row persisted");
    assert_eq!(scoped, 0, "master connect is not a scoped initiator");
}

/// The initiator + actor hash round-trip through `btcpay_authorize_state`
/// (migration 0025): recorded at start, recovered at callback, single-use.
#[tokio::test]
async fn authorize_state_carries_scoped_initiator() {
    let (state, _tmp) = make_test_state().await;
    let profile = repo::get_default_merchant_profile(&state.db)
        .await
        .expect("query default profile")
        .expect("a default profile exists post-migration");

    keysat::btcpay::config::record_authorize_state(
        &state.db,
        "tok_scoped",
        Some(&profile.id),
        true,
        Some("deadbeef"),
    )
    .await
    .expect("record scoped");
    let s = keysat::btcpay::config::consume_authorize_state(&state.db, "tok_scoped")
        .await
        .expect("consume scoped");
    assert!(s.scoped_initiator, "scoped_initiator must round-trip");
    assert_eq!(s.initiator_actor_hash.as_deref(), Some("deadbeef"));
    assert_eq!(s.merchant_profile_id.as_deref(), Some(profile.id.as_str()));
    // Single-use: a replay of the same token fails.
    assert!(
        keysat::btcpay::config::consume_authorize_state(&state.db, "tok_scoped")
            .await
            .is_err(),
        "consumed token must not replay"
    );

    // Master initiator: defaults (not scoped, no hash).
    keysat::btcpay::config::record_authorize_state(
        &state.db,
        "tok_master",
        Some(&profile.id),
        false,
        None,
    )
    .await
    .expect("record master");
    let m = keysat::btcpay::config::consume_authorize_state(&state.db, "tok_master")
        .await
        .expect("consume master");
    assert!(!m.scoped_initiator);
    assert_eq!(m.initiator_actor_hash, None);
}

/// The GET BTCPay callback must surface a failed/denied connect as a non-2xx
/// status, not a 200 with an HTML error body (the POST callback already does via
/// `?`). An unknown state token fails closed at consume time -> 401. This guards
/// the regression where the deny path (e.g. a scoped key targeting a mainnet
/// store) would otherwise return 200 with no programmatic error signal.
#[tokio::test]
async fn btcpay_callback_get_propagates_error_status() {
    let (state, _tmp) = make_test_state_sandbox().await;
    let req = build_request(
        "GET",
        "/v1/btcpay/authorize/callback?state=bogus-token&apiKey=whatever",
        &[],
        None,
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "a GET callback with an invalid state token must return 401, not a 200 error page"
    );
}

/// Zaprite Connect refuses on Creator-tier (no `zaprite_payments`
/// entitlement) with 402. Switching the daemon's self-tier to a
/// Pro-flavored Licensed tier lets the Connect-precheck pass (it then
/// fails downstream on the unreachable test host, but the tier gate is
/// behind us).
#[tokio::test]
async fn zaprite_connect_gated_by_pro_entitlement() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);

    // Creator tier (default for test fixture) — Connect should 402.
    let req = build_request(
        "POST",
        "/v1/admin/zaprite/connect",
        &[("authorization", &auth)],
        Some(json!({"api_key": "fake-zaprite-key"})),
    );
    let resp = send(&state, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::PAYMENT_REQUIRED,
        "Zaprite Connect must 402 without zaprite_payments entitlement"
    );
    let body = body_json(resp).await;
    assert_eq!(body["error"], "tier_cap");
    assert!(body["upgrade_url"].as_str().expect("upgrade_url").contains("/buy/keysat"));
}

/// CORS — the public read-only endpoints answer cross-origin requests
/// from any browser origin so docs.keysat.xyz can fetch live pricing
/// from licensing.keysat.xyz without proxying. `allow_credentials` is
/// intentionally OFF: pages can read public responses but cannot ride
/// a logged-in admin session cookie to hit /v1/admin/*.
#[tokio::test]
async fn cors_allows_cross_origin_on_public_endpoints() {
    let (state, _tmp) = make_test_state().await;
    let req = build_request(
        "GET",
        "/v1/openapi.json",
        &[("origin", "https://docs.keysat.xyz")],
        None,
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let acao = resp
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(acao, "*", "public endpoints should set ACAO: *");
    // Credentials must NOT be allowed — combining `*` origin with
    // credentials is rejected by browsers, and disabling it means a
    // hostile cross-origin page can't ride a session cookie.
    let acac = resp.headers().get("access-control-allow-credentials");
    assert!(acac.is_none(), "credentials must not be allowed");
}

/// CORS preflight (OPTIONS) is handled by the CorsLayer directly and
/// never reaches the session-bridge or any handler. This is the path
/// browsers take before issuing an actual cross-origin POST.
#[tokio::test]
async fn cors_preflight_returns_2xx_without_auth() {
    let (state, _tmp) = make_test_state().await;
    let req = build_request(
        "OPTIONS",
        "/v1/admin/products",
        &[
            ("origin", "https://example.com"),
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "authorization,content-type"),
        ],
        None,
    );
    let resp = send(&state, req).await;
    // CorsLayer answers preflight with 200 (or 204). No auth required.
    assert!(
        resp.status().is_success() || resp.status() == StatusCode::NO_CONTENT,
        "preflight should be 2xx, got {}",
        resp.status()
    );
    let acao = resp
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(acao, "*");
}

/// Regression: `entitlements_catalog_json` was missing from every
/// product SELECT for ~a release, so admin UI edits appeared to drop
/// on the floor — the column was being written correctly but never
/// read back. This test creates a product, sets a catalog, reads it
/// back through the same code path the admin UI hits.
#[tokio::test]
async fn product_entitlements_catalog_round_trips_through_list_endpoint() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);

    // Create a product
    let req = build_request(
        "POST",
        "/v1/admin/products",
        &[("authorization", &auth)],
        Some(json!({
            "slug": "catalog-rt",
            "name": "Catalog round-trip",
            "description": "",
            "price_currency": "SAT",
            "price_value": 1000,
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "product create");
    let created = body_json(resp).await;
    let product_id = created["id"].as_str().expect("id").to_string();

    // PATCH the catalog
    let req = build_request(
        "PATCH",
        &format!("/v1/admin/products/{}", product_id),
        &[("authorization", &auth)],
        Some(json!({
            "entitlements_catalog": [
                {"slug": "self_host", "name": "Self-host on Start9", "description": "Run on your own hardware."},
                {"slug": "unlimited_products", "name": "Unlimited products", "description": "No 5-product cap."}
            ]
        })),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "product patch with catalog");

    // Now read it back via /v1/products (same endpoint the admin UI uses)
    let req = build_request("GET", "/v1/products", &[], None);
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let products = body["products"].as_array().expect("products array");
    let found = products
        .iter()
        .find(|p| p["id"] == product_id)
        .expect("product visible in list");
    let catalog = found["entitlements_catalog"]
        .as_array()
        .expect("entitlements_catalog should be an array, not null");
    assert_eq!(catalog.len(), 2, "both catalog entries should round-trip");
    assert_eq!(catalog[0]["slug"], "self_host");
    assert_eq!(catalog[1]["slug"], "unlimited_products");
}

/// Audit coverage for the `:52` merchant-profile / payment-provider model.
/// These queries are runtime-prepared (`sqlx::query(&format!(...))`), so column
/// errors only surface when executed — and the resolution path had no passing
/// test, which let an ambiguous-column bug ship in `get_merchant_profile_for_product`.
/// This drives the write + preference + resolution queries end to end so the
/// whole `:52` SQL surface is exercised by at least one green test.
#[tokio::test]
async fn merchant_profile_provider_resolution_queries_round_trip() {
    let (state, _tmp) = make_test_state().await;
    let now = "2026-06-12T00:00:00Z";

    // Default profile is created by migration 0020.
    let default = repo::get_default_merchant_profile(&state.db)
        .await
        .expect("get_default_merchant_profile")
        .expect("a default profile exists post-migration");

    // Profile CRUD reads/writes.
    let p2 = Uuid::new_v4().to_string();
    repo::create_merchant_profile(
        &state.db, &p2, "Recaps", None, None, None, None, None, false, now,
    )
    .await
    .expect("create_merchant_profile");
    repo::get_merchant_profile_by_id(&state.db, &p2)
        .await
        .expect("get_merchant_profile_by_id")
        .expect("created profile exists");
    assert!(
        repo::list_merchant_profiles(&state.db)
            .await
            .expect("list_merchant_profiles")
            .len()
            >= 2
    );

    // Attach a BTCPay provider to the default profile (store_id present so
    // build_provider can construct a client without a network call).
    let prov = Uuid::new_v4().to_string();
    repo::create_payment_provider(
        &state.db, &prov, &default.id, "btcpay", "Test BTCPay",
        "api-key", "http://btcpay.test", Some("wh-1"), Some("secret"), Some("store-1"), now,
    )
    .await
    .expect("create_payment_provider");
    repo::get_payment_provider_by_id(&state.db, &prov)
        .await
        .expect("get_payment_provider_by_id")
        .expect("created provider exists");
    assert_eq!(
        repo::list_payment_providers_for_profile(&state.db, &default.id)
            .await
            .expect("list_payment_providers_for_profile")
            .len(),
        1
    );
    repo::list_all_payment_providers(&state.db)
        .await
        .expect("list_all_payment_providers");

    // Rail preference write + read.
    repo::set_rail_preference(&state.db, &default.id, "lightning", &prov)
        .await
        .expect("set_rail_preference");
    assert_eq!(
        repo::list_rail_preferences_for_profile(&state.db, &default.id)
            .await
            .expect("list_rail_preferences_for_profile")
            .len(),
        1
    );

    // The production purchase path's resolution, both branches:
    //   - Lightning resolves via the explicit preference just set.
    //   - OnChain has no preference but BTCPay serves it → served-rail fallback.
    let (row_pref, _p) = state
        .resolve_provider_for_profile_rail(&default.id, keysat::payment::Rail::Lightning)
        .await
        .expect("resolve lightning via explicit preference");
    assert_eq!(row_pref.id, prov);
    let (row_fallback, _p) = state
        .resolve_provider_for_profile_rail(&default.id, keysat::payment::Rail::Onchain)
        .await
        .expect("resolve onchain via served-rail fallback");
    assert_eq!(row_fallback.id, prov);
}

/// The product → merchant-profile write path. The resolver
/// (`get_merchant_profile_for_product`) already reads
/// `products.merchant_profile_id`, but nothing wrote it until
/// `set_product_merchant_profile` landed. Drives create (NULL → default),
/// attach (resolves to the chosen profile), and clear (back to default),
/// plus the bad-id guard.
#[tokio::test]
async fn product_merchant_profile_write_path_round_trips() {
    let (state, _tmp) = make_test_state().await;
    let now = "2026-06-15T00:00:00Z";

    let default = repo::get_default_merchant_profile(&state.db)
        .await
        .expect("get_default_merchant_profile")
        .expect("a default profile exists post-migration");

    // Fresh product: no profile id set. The repo read returns None (the
    // column is NULL); the production resolver `for_product` applies the
    // default-profile fallback.
    let product = repo::create_product(&state.db, "profile-write", "Profile Write", "", 1_000, &json!({}))
        .await
        .expect("create_product");
    assert_eq!(product.merchant_profile_id, None);
    assert!(
        repo::get_merchant_profile_for_product(&state.db, &product.id)
            .await
            .expect("get_merchant_profile_for_product")
            .is_none(),
        "a NULL-profile product yields no direct match"
    );
    let resolved = keysat::merchant_profiles::for_product(&state, &product.id)
        .await
        .expect("for_product falls back to default");
    assert_eq!(resolved.id, default.id);

    // Attach to a second profile → reads back + resolves to that profile.
    let p2 = Uuid::new_v4().to_string();
    repo::create_merchant_profile(
        &state.db, &p2, "Second Biz", None, None, None, None, None, false, now,
    )
    .await
    .expect("create_merchant_profile");
    let attached = repo::set_product_merchant_profile(&state.db, &product.id, Some(&p2))
        .await
        .expect("set_product_merchant_profile attach");
    assert_eq!(attached.merchant_profile_id.as_deref(), Some(p2.as_str()));
    let resolved = keysat::merchant_profiles::for_product(&state, &product.id)
        .await
        .expect("for_product resolves to attached profile");
    assert_eq!(resolved.id, p2);

    // Clear back to NULL → resolver falls back to the default again.
    let cleared = repo::set_product_merchant_profile(&state.db, &product.id, None)
        .await
        .expect("set_product_merchant_profile clear");
    assert_eq!(cleared.merchant_profile_id, None);
    let resolved = keysat::merchant_profiles::for_product(&state, &product.id)
        .await
        .expect("for_product falls back to default after clear");
    assert_eq!(resolved.id, default.id);

    // Bad profile id is rejected with NotFound, not an FK-violation 500.
    let err = repo::set_product_merchant_profile(&state.db, &product.id, Some("does-not-exist"))
        .await
        .expect_err("bad profile id is rejected");
    assert!(matches!(err, keysat::error::AppError::NotFound(_)), "got {err:?}");
}

/// HTTP-layer coverage for the product → merchant-profile wiring: the
/// thin create/update handler arms (Some / Some(None) / Some(Some(bad)))
/// that the repo-level round-trip test above can't reach. Runtime-prepared
/// SQL means a typo in those arms only surfaces at execution.
#[tokio::test]
async fn admin_product_merchant_profile_endpoints() {
    let (state, _tmp) = make_test_state().await;
    let auth = format!("Bearer {}", TEST_ADMIN_KEY);

    // Second profile (repo-direct bypasses the Creator tier cap).
    let p2 = Uuid::new_v4().to_string();
    repo::create_merchant_profile(
        &state.db, &p2, "Second Biz", None, None, None, None, None, false,
        "2026-06-15T00:00:00Z",
    )
    .await
    .expect("create_merchant_profile");

    // Create with a valid profile id → 200, echoed back.
    let req = build_request(
        "POST",
        "/v1/admin/products",
        &[("authorization", &auth)],
        Some(json!({"slug": "mp-create", "name": "MP Create", "price_sats": 1000, "merchant_profile_id": p2})),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["merchant_profile_id"], json!(p2));
    let created_id = body["id"].as_str().expect("product id").to_string();

    // PATCH clear (merchant_profile_id: null) → 200, field cleared.
    let req = build_request(
        "PATCH",
        &format!("/v1/admin/products/{created_id}"),
        &[("authorization", &auth)],
        Some(json!({"merchant_profile_id": null})),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["merchant_profile_id"], json!(null));

    // PATCH set back to the valid profile → 200.
    let req = build_request(
        "PATCH",
        &format!("/v1/admin/products/{created_id}"),
        &[("authorization", &auth)],
        Some(json!({"merchant_profile_id": p2})),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["merchant_profile_id"], json!(p2));

    // PATCH to a nonexistent profile → 404.
    let req = build_request(
        "PATCH",
        &format!("/v1/admin/products/{created_id}"),
        &[("authorization", &auth)],
        Some(json!({"merchant_profile_id": "nope"})),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Create with a nonexistent profile → 404. The product row is created
    // before the profile is attached (same post-write order as the
    // entitlements catalog), so it persists with a NULL profile — benign:
    // it resolves to the default and the operator can reattach or delete.
    let req = build_request(
        "POST",
        "/v1/admin/products",
        &[("authorization", &auth)],
        Some(json!({"slug": "mp-bad", "name": "MP Bad", "price_sats": 1000, "merchant_profile_id": "nope"})),
    );
    let resp = send(&state, req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let orphan = repo::get_product_by_slug(&state.db, "mp-bad")
        .await
        .expect("get_product_by_slug")
        .expect("product persisted despite the profile-404");
    assert_eq!(orphan.merchant_profile_id, None);
}

