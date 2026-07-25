//! Thin HTTP client for Zaprite's `/v1/*` API.
//!
//! Maps directly to the OpenAPI spec at api.zaprite.com/openapi.json.
//! Returns the raw JSON shapes for now — the `ZapriteProvider` impl
//! turns them into the trait's typed enums.

use crate::payment::health::ProviderHealthSink;
use crate::payment::ProviderHttpError;
use anyhow::{anyhow, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::Serialize;
use serde_json::Value;
use std::time::{Duration, SystemTime};

/// How one Zaprite call reads the response body.
///
/// The six call sites do not agree, and the disagreement is observable in the
/// error a caller gets back, so [`ZapriteClient::send`] reproduces each site's
/// behavior instead of picking one.
enum BodyRead {
    /// `resp.text().await.context(<ctx>)?` on **both** paths — the site needs
    /// the body to parse its success response, so a body-read failure surfaces
    /// as that context and never as a [`ProviderHttpError`].
    Always(&'static str),
    /// Failure path only, `unwrap_or_default()`. The success path never
    /// touches the body. Only `ping` works this way.
    LossyOnError,
}

#[derive(Debug, Clone)]
pub struct ZapriteClient {
    pub base_url: String,
    pub api_key: String,
    http: reqwest::Client,
    /// Where [`send`](Self::send) reports every observed HTTP status, so a
    /// revoked API key surfaces as an operator alert. `None` for a client built
    /// with no `payment_providers` row to attribute calls to — which includes
    /// the connect-time smoke test in `api::zaprite_authorize`, where the row
    /// does not exist yet.
    health: Option<ProviderHealthSink>,
}

/// Subset of `POST /v1/orders` request body — the fields Keysat
/// actually populates. Zaprite accepts many more (invoice line
/// items, contacts, etc.) that we don't need for the licensing
/// flow.
#[derive(Debug, Serialize)]
pub struct CreateOrderBody<'a> {
    pub amount: i64,
    pub currency: &'a str,
    /// OUR internal invoice UUID. The webhook handler uses this
    /// as the trust anchor — only orders Zaprite reports back
    /// with a matching externalUniqId are honored. Zaprite does
    /// NOT dedupe on this field; it's reconciliation only.
    #[serde(rename = "externalUniqId")]
    pub external_uniq_id: &'a str,
    /// URL we send the buyer to after Zaprite finishes the
    /// checkout (success or otherwise). Zaprite appends its own
    /// status fragments.
    #[serde(rename = "redirectUrl")]
    pub redirect_url: &'a str,
    /// Display label on Zaprite's checkout page + on Bitcoin
    /// transaction labels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<&'a str>,
    /// Free-form metadata Keysat round-trips for audit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    /// `{ email, name }` — set if the buyer provided one at
    /// checkout. Zaprite uses this for receipts.
    #[serde(rename = "customerData", skip_serializing_if = "Option::is_none")]
    pub customer_data: Option<Value>,
    /// `true` allows the buyer to save their card on Zaprite for
    /// recurring charges. Set when the policy is recurring.
    #[serde(rename = "allowSavePaymentProfile", skip_serializing_if = "Option::is_none")]
    pub allow_save_payment_profile: Option<bool>,
    /// Zaprite contact id to attach this order to. REQUIRED by
    /// Zaprite when `allow_save_payment_profile` is true — without
    /// it the create-order call returns
    /// `400 contactId is required when allowSavePaymentProfile is true`.
    /// Optional otherwise; passing it for one-shot purchases just
    /// associates the order with a known contact in the operator's
    /// Zaprite dashboard.
    #[serde(rename = "contactId", skip_serializing_if = "Option::is_none")]
    pub contact_id: Option<String>,
}

impl ZapriteClient {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("build reqwest client");
        Self {
            base_url,
            api_key: api_key.into(),
            http,
            health: None,
        }
    }

    /// Bind this client's calls to the shared auth-health map for one
    /// `payment_providers` row. See [`crate::btcpay::client::BtcpayClient::with_sink`]
    /// for why this is a builder and not a `new()` parameter.
    pub fn with_sink(mut self, sink: ProviderHealthSink) -> Self {
        self.health = Some(sink);
        self
    }

    fn auth_headers(&self) -> Result<HeaderMap> {
        let mut h = HeaderMap::new();
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.api_key))
                .map_err(|e| anyhow!("invalid bearer token: {e}"))?,
        );
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(h)
    }

    /// The one place a Zaprite request is executed and a non-success status is
    /// turned into an error. Every non-2xx from this client leaves through
    /// here **as** a [`ProviderHttpError`] — not merely carrying one as a
    /// source — which is what the failure-alert tracker keys on.
    ///
    /// The six call sites differ in wording, in the context they put on a
    /// transport failure, and in how they read the body — all of it visible to
    /// operators in the logs — so `send` takes those as parameters rather than
    /// imposing one shape. Returns the body text, empty when the site's
    /// [`BodyRead`] policy did not read it.
    ///
    /// `message` becomes the error's `message` **field**, not a `.context(..)`
    /// wrapper, so `{e:#}` stays byte-identical to the pre-collapse text; see
    /// [`ProviderHttpError`] for why a public route depends on that.
    async fn send(
        &self,
        req: reqwest::RequestBuilder,
        label: &'static str,
        transport_ctx: &'static str,
        body_read: BodyRead,
        message: impl FnOnce(reqwest::StatusCode, &str) -> String,
    ) -> Result<String> {
        let resp = req.send().await.context(transport_ctx)?;
        let status = resp.status();

        // Report the outcome the instant the status is known. This MUST stay
        // above the body read: `BodyRead::Always` reads with `?`, so a 401 whose
        // body cannot be read (truncated, chunked, connection dropped mid-body)
        // returns that context and never reaches the `ProviderHttpError` below.
        // Recording after the read would make a revoked key on a flaky link
        // invisible to the alert rule entirely — see the wiring note on
        // `ProviderAuthHealth` and risk 7 in the plan.
        if let Some(sink) = &self.health {
            sink.record(label, status.as_u16(), SystemTime::now());
        }

        let raw = match body_read {
            BodyRead::Always(ctx) => resp.text().await.context(ctx)?,
            BodyRead::LossyOnError if status.is_success() => String::new(),
            BodyRead::LossyOnError => resp.text().await.unwrap_or_default(),
        };

        if status.is_success() {
            return Ok(raw);
        }
        Err(anyhow::Error::new(ProviderHttpError {
            status: status.as_u16(),
            label,
            message: message(status, &raw),
        }))
    }

    /// `POST /v1/orders` — create an order. Returns the full order
    /// JSON so the caller can pull whichever fields it needs
    /// (`id`, `checkoutUrl`, `status`, etc.).
    pub async fn create_order(&self, body: &CreateOrderBody<'_>) -> Result<Value> {
        let url = format!("{}/v1/orders", self.base_url);
        let raw = self
            .send(
                self.http.post(&url).headers(self.auth_headers()?).json(body),
                "zaprite.create_order",
                "Zaprite create_order request",
                BodyRead::Always("read create_order body"),
                |status, raw| format!("Zaprite create_order returned HTTP {status}: {raw}"),
            )
            .await?;
        serde_json::from_str(&raw).context("parse create_order response")
    }

    /// `GET /v1/orders/{id}` — fetch an order by Zaprite id OR by
    /// externalUniqId (Zaprite accepts either). Used by the
    /// reconcile loop to catch missed webhooks.
    pub async fn get_order(&self, order_id: &str) -> Result<Value> {
        let encoded = urlencoding::encode(order_id);
        let url = format!("{}/v1/orders/{encoded}", self.base_url);
        let raw = self
            .send(
                self.http.get(&url).headers(self.auth_headers()?),
                "zaprite.get_order",
                "Zaprite get_order request",
                BodyRead::Always("read get_order body"),
                |status, raw| {
                    format!("Zaprite get_order({order_id}) returned HTTP {status}: {raw}")
                },
            )
            .await?;
        serde_json::from_str(&raw).context("parse get_order response")
    }

    /// `POST /v1/orders/charge` — charge an order against a
    /// previously-saved payment profile. Used by the recurring-
    /// subscriptions renewal worker (per the
    /// RECURRING_SUBSCRIPTIONS_DESIGN.md "Phase 2 — Renewal worker"
    /// section). Not invoked from one-shot purchase flow.
    pub async fn charge_order_with_profile(
        &self,
        order_id: &str,
        payment_profile_id: &str,
    ) -> Result<Value> {
        let url = format!("{}/v1/orders/charge", self.base_url);
        let body = serde_json::json!({
            "orderId": order_id,
            "paymentProfileId": payment_profile_id,
        });
        let raw = self
            .send(
                self.http
                    .post(&url)
                    .headers(self.auth_headers()?)
                    .json(&body),
                "zaprite.charge_order_with_profile",
                "Zaprite charge_order_with_profile request",
                BodyRead::Always("read charge body"),
                |status, raw| {
                    format!("Zaprite charge_order_with_profile returned HTTP {status}: {raw}")
                },
            )
            .await?;
        serde_json::from_str(&raw).context("parse charge response")
    }

    /// `POST /v1/contacts` — create a Zaprite contact. Required
    /// upstream step before creating an order with
    /// `allowSavePaymentProfile: true` (Zaprite needs to know which
    /// contact the saved profile attaches to). Returns the full
    /// contact JSON; the caller extracts `id` to pass as
    /// `contactId` on the subsequent order create.
    ///
    /// `legal_name` is required by Zaprite's schema; we fall back to
    /// the email itself when the buyer didn't supply a name. The
    /// operator can rename the contact in the Zaprite dashboard if
    /// they care about display polish.
    ///
    /// NOTE on duplicates: Zaprite's duplicate-email behavior on
    /// `POST /v1/contacts` is undocumented (their llms.txt explicitly
    /// says "Not documented"). Empirically we accept whatever Zaprite
    /// does — if they create a duplicate, the operator's Zaprite
    /// contact list gets a row per recurring purchase from the same
    /// buyer. The multi-provider work (planned `:47+`) will introduce
    /// a Keysat-side `zaprite_contacts` cache keyed on (email,
    /// provider_id) to dedup upfront. For sandbox testing + early
    /// production this is acceptable noise.
    pub async fn create_contact(
        &self,
        email: &str,
        name: Option<&str>,
    ) -> Result<Value> {
        let legal_name = name.unwrap_or(email);
        let url = format!("{}/v1/contacts", self.base_url);
        let body = serde_json::json!({
            "email": email,
            "legalName": legal_name,
        });
        let raw = self
            .send(
                self.http
                    .post(&url)
                    .headers(self.auth_headers()?)
                    .json(&body),
                "zaprite.create_contact",
                "Zaprite create_contact request",
                BodyRead::Always("read create_contact body"),
                |status, raw| format!("Zaprite create_contact returned HTTP {status}: {raw}"),
            )
            .await?;
        serde_json::from_str(&raw).context("parse create_contact response")
    }

    /// `GET /v1/contacts/{id}` — fetch a Zaprite contact, which
    /// includes the `paymentProfiles[]` array we mine for the
    /// saved-card id after a recurring first-cycle settle. Each
    /// profile has `id`, `method`, `expiresAt`, and a `sourceOrder`
    /// nested object whose `externalUniqId` is the invoice UUID we
    /// passed when creating the order — that's how we identify the
    /// profile the buyer just saved on the order that triggered
    /// this lookup.
    pub async fn get_contact(&self, contact_id: &str) -> Result<Value> {
        let encoded = urlencoding::encode(contact_id);
        let url = format!("{}/v1/contacts/{encoded}", self.base_url);
        let raw = self
            .send(
                self.http.get(&url).headers(self.auth_headers()?),
                "zaprite.get_contact",
                "Zaprite get_contact request",
                BodyRead::Always("read get_contact body"),
                |status, raw| {
                    format!("Zaprite get_contact({contact_id}) returned HTTP {status}: {raw}")
                },
            )
            .await?;
        serde_json::from_str(&raw).context("parse get_contact response")
    }

    /// Smoke test for Connect-flow validation. Pings `GET /v1/orders`
    /// (the list endpoint) — auth-guarded, so a 200 confirms the
    /// API key works against the right org.
    ///
    /// Labelled `"zaprite.ping"`, which is **not** a probe label: this is the
    /// operator standing at the Connect screen, where a 403 is a genuine "your
    /// key cannot do this" and must keep counting. The liveness probe is
    /// [`probe_auth`](Self::probe_auth), same HTTP call, different label.
    pub async fn ping(&self) -> Result<()> {
        self.ping_labeled("zaprite.ping").await
    }

    /// Liveness probe: does this API key still authenticate?
    ///
    /// The same authenticated, read-only `GET /v1/orders?limit=1` as
    /// [`ping`](Self::ping) — Zaprite has no cheaper "who am I" endpoint and
    /// this one is already proven — carrying
    /// [`ZAPRITE_PROBE_LABEL`](crate::payment::health::ZAPRITE_PROBE_LABEL)
    /// instead.
    ///
    /// **The label is the entire difference, and it is invisible to the
    /// compiler** (both are `&'static str`). Calling `ping()` here instead
    /// would file every probe 403 under a counting label — alerting on a key
    /// that sells perfectly — and every probe success as a Connect-time
    /// success. Hence the shared body sits behind `ping_labeled` and neither
    /// wrapper can drift onto the other's label.
    pub async fn probe_auth(&self) -> Result<()> {
        self.ping_labeled(crate::payment::health::ZAPRITE_PROBE_LABEL)
            .await
    }

    /// The shared body of [`ping`](Self::ping) and
    /// [`probe_auth`](Self::probe_auth).
    ///
    /// The operator-facing message deliberately does **not** vary with the
    /// label: it is one HTTP call, the wording is what a reader sees in the
    /// logs, and `ping`'s shape is pinned by a message test from Step 1. The
    /// label alone carries the classification, and it carries it to the health
    /// sink rather than to a human.
    async fn ping_labeled(&self, label: &'static str) -> Result<()> {
        let url = format!("{}/v1/orders?limit=1", self.base_url);
        self.send(
            self.http.get(&url).headers(self.auth_headers()?),
            label,
            "Zaprite ping request",
            // The only site that does not read the body on success, and reads
            // it lossily on failure.
            BodyRead::LossyOnError,
            |status, body| format!("Zaprite ping returned HTTP {status}: {body}"),
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Pins the six sites that were collapsed onto [`ZapriteClient::send`]:
    //! each must still render exactly the text it rendered before the
    //! collapse, and must now carry a [`ProviderHttpError`] with the right
    //! status and label.
    //!
    //! The behavior change actually hiding in this collapse is the body read:
    //! five sites read the body with `.context(..)?` on both paths, `ping`
    //! reads it lossily on the failure path only. `create_order_*` below pins
    //! both halves of that.

    use super::*;
    use crate::payment::health::{ProviderAuthHealth, ProviderHealthMap};
    use axum::{http::StatusCode, Router};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Throwaway HTTP server on an ephemeral port that answers every method
    /// and path with one fixed status + body. Same shape as `tests/worker.rs`'s
    /// `spawn_500_receiver`, the established local-stub pattern in this repo.
    /// Duplicated in `btcpay::client`'s test module rather than shared, so the
    /// refactor does not add a module outside its blast radius.
    async fn spawn_stub(status: StatusCode, body: &'static str) -> String {
        let app = Router::new().fallback(move || async move { (status, body) });
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        format!("http://{addr}")
    }

    /// A raw TCP stub that promises more body bytes in `Content-Length` than
    /// it writes, then closes. `reqwest` parses the 401 status line fine and
    /// then fails **while reading the body** — the one case where
    /// `BodyRead::Always` has to surface its own `.context(..)` rather than a
    /// `ProviderHttpError`, exactly as the pre-refactor code did.
    async fn spawn_truncated_body_401() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    // Read the request head so the client has begun writing
                    // before we reply; the content is irrelevant.
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 64\r\n\r\nshort",
                        )
                        .await;
                    let _ = sock.flush().await;
                    let _ = sock.shutdown().await;
                    // Drain whatever is still in flight before dropping. A
                    // request split across two TCP segments would otherwise
                    // leave unread bytes queued, and closing on those makes
                    // the OS send RST instead of FIN — which can discard the
                    // response we just wrote and turn this into a transport
                    // error rather than the body-read error under test.
                    while matches!(sock.read(&mut buf).await, Ok(n) if n > 0) {}
                });
            }
        });
        format!("http://{addr}")
    }

    fn order_body() -> CreateOrderBody<'static> {
        CreateOrderBody {
            amount: 1000,
            currency: "USD",
            external_uniq_id: "inv-1",
            redirect_url: "https://example.test/thanks",
            label: None,
            metadata: None,
            customer_data: None,
            allow_save_payment_profile: None,
            contact_id: None,
        }
    }

    #[tokio::test]
    async fn create_order_non_success_keeps_message_and_carries_typed_error() {
        let base = spawn_stub(StatusCode::UNAUTHORIZED, r#"{"error":"bad key"}"#).await;
        let client = ZapriteClient::new(&base, "k");

        let err = client
            .create_order(&order_body())
            .await
            .expect_err("non-2xx must error");

        assert_eq!(
            err.to_string(),
            r#"Zaprite create_order returned HTTP 401 Unauthorized: {"error":"bad key"}"#
        );
        // The alternate form must be byte-identical too, not just `{e}`.
        // `api/purchase.rs:595` feeds `{e:#}` into `AppError::Upstream`, which
        // is returned verbatim in the body of the UNAUTHENTICATED
        // `POST /v1/purchase` — so a chained suffix here would change a public
        // route's response. Carrying the text as a field, not a context, is
        // what keeps these two equal.
        assert_eq!(
            format!("{err:#}"),
            r#"Zaprite create_order returned HTTP 401 Unauthorized: {"error":"bad key"}"#
        );

        let typed = err
            .downcast_ref::<ProviderHttpError>()
            .expect("must be a ProviderHttpError");
        assert_eq!(typed.status, 401);
        assert_eq!(typed.label, "zaprite.create_order");
    }

    /// An empty error body still renders the pre-refactor shape — trailing
    /// separator and all — and still carries the typed error.
    #[tokio::test]
    async fn create_order_empty_body_keeps_the_pre_refactor_shape() {
        let base = spawn_stub(StatusCode::UNAUTHORIZED, "").await;
        let client = ZapriteClient::new(&base, "k");

        let err = client
            .create_order(&order_body())
            .await
            .expect_err("non-2xx must error");

        assert_eq!(
            err.to_string(),
            "Zaprite create_order returned HTTP 401 Unauthorized: "
        );
        assert_eq!(
            err.downcast_ref::<ProviderHttpError>()
                .expect("must be a ProviderHttpError")
                .status,
            401
        );
    }

    /// The one real behavior change hiding in the collapse: a non-2xx whose
    /// body cannot be read must still fail with `read create_order body`, NOT
    /// with the HTTP message. Pre-refactor the body read came first and used
    /// `?`, so it won — and it still must.
    #[tokio::test]
    async fn create_order_unreadable_body_keeps_its_own_context() {
        let base = spawn_truncated_body_401().await;
        let client = ZapriteClient::new(&base, "k");

        let err = client
            .create_order(&order_body())
            .await
            .expect_err("unreadable body must error");

        assert_eq!(err.to_string(), "read create_order body");
        assert!(
            err.downcast_ref::<ProviderHttpError>().is_none(),
            "a body-read failure is not an HTTP-status failure"
        );
    }

    #[tokio::test]
    async fn get_order_non_success_interpolates_the_order_id() {
        let base = spawn_stub(StatusCode::FORBIDDEN, "denied").await;
        let client = ZapriteClient::new(&base, "k");

        let err = client
            .get_order("ord 123")
            .await
            .expect_err("non-2xx must error");

        // The raw (un-percent-encoded) id is what the message has always used,
        // even though the URL encodes it.
        assert_eq!(
            err.to_string(),
            "Zaprite get_order(ord 123) returned HTTP 403 Forbidden: denied"
        );

        let typed = err
            .downcast_ref::<ProviderHttpError>()
            .expect("must be a ProviderHttpError");
        assert_eq!(typed.status, 403);
        assert_eq!(typed.label, "zaprite.get_order");
    }

    #[tokio::test]
    async fn charge_order_with_profile_non_success_keeps_message_and_carries_typed_error() {
        let base = spawn_stub(StatusCode::PAYMENT_REQUIRED, "card declined").await;
        let client = ZapriteClient::new(&base, "k");

        let err = client
            .charge_order_with_profile("ord-1", "pp-1")
            .await
            .expect_err("non-2xx must error");

        assert_eq!(
            err.to_string(),
            "Zaprite charge_order_with_profile returned HTTP 402 Payment Required: card declined"
        );

        let typed = err
            .downcast_ref::<ProviderHttpError>()
            .expect("must be a ProviderHttpError");
        assert_eq!(typed.status, 402);
        assert_eq!(typed.label, "zaprite.charge_order_with_profile");
    }

    #[tokio::test]
    async fn create_contact_non_success_keeps_message_and_carries_typed_error() {
        let base = spawn_stub(StatusCode::BAD_REQUEST, "email required").await;
        let client = ZapriteClient::new(&base, "k");

        let err = client
            .create_contact("buyer@example.test", None)
            .await
            .expect_err("non-2xx must error");

        assert_eq!(
            err.to_string(),
            "Zaprite create_contact returned HTTP 400 Bad Request: email required"
        );

        let typed = err
            .downcast_ref::<ProviderHttpError>()
            .expect("must be a ProviderHttpError");
        assert_eq!(typed.status, 400);
        assert_eq!(typed.label, "zaprite.create_contact");
    }

    #[tokio::test]
    async fn get_contact_non_success_interpolates_the_contact_id() {
        let base = spawn_stub(StatusCode::NOT_FOUND, "gone").await;
        let client = ZapriteClient::new(&base, "k");

        let err = client
            .get_contact("con-1")
            .await
            .expect_err("non-2xx must error");

        assert_eq!(
            err.to_string(),
            "Zaprite get_contact(con-1) returned HTTP 404 Not Found: gone"
        );

        let typed = err
            .downcast_ref::<ProviderHttpError>()
            .expect("must be a ProviderHttpError");
        assert_eq!(typed.status, 404);
        assert_eq!(typed.label, "zaprite.get_contact");
    }

    #[tokio::test]
    async fn ping_non_success_keeps_message_and_carries_typed_error() {
        let base = spawn_stub(StatusCode::UNAUTHORIZED, "invalid token").await;
        let client = ZapriteClient::new(&base, "k");

        let err = client.ping().await.expect_err("non-2xx must error");

        assert_eq!(
            err.to_string(),
            "Zaprite ping returned HTTP 401 Unauthorized: invalid token"
        );

        let typed = err
            .downcast_ref::<ProviderHttpError>()
            .expect("must be a ProviderHttpError");
        assert_eq!(typed.status, 401);
        assert_eq!(typed.label, "zaprite.ping");
    }

    /// The transport path is the other half of what `send` parameterized, and
    /// nothing else pins it: all six sites layer their own `.context(..)` on a
    /// connect/timeout failure, and none of them may produce a
    /// `ProviderHttpError` — there is no status to carry.
    #[tokio::test]
    async fn transport_failures_keep_their_per_site_context() {
        // Nothing listens on port 1 (binding it needs root), so every call
        // below fails before any HTTP status exists.
        let client = ZapriteClient::new("http://127.0.0.1:1", "k");

        let cases: Vec<(anyhow::Error, &str)> = vec![
            (
                client.create_order(&order_body()).await.unwrap_err(),
                "Zaprite create_order request",
            ),
            (
                client.get_order("ord-1").await.unwrap_err(),
                "Zaprite get_order request",
            ),
            (
                client
                    .charge_order_with_profile("ord-1", "pp-1")
                    .await
                    .unwrap_err(),
                "Zaprite charge_order_with_profile request",
            ),
            (
                client
                    .create_contact("buyer@example.test", None)
                    .await
                    .unwrap_err(),
                "Zaprite create_contact request",
            ),
            (
                client.get_contact("con-1").await.unwrap_err(),
                "Zaprite get_contact request",
            ),
            (client.ping().await.unwrap_err(), "Zaprite ping request"),
        ];

        for (err, expected) in cases {
            assert_eq!(err.to_string(), expected);
            assert!(
                err.downcast_ref::<ProviderHttpError>().is_none(),
                "{expected}: a transport failure is not an HTTP-status failure"
            );
        }
    }

    // -----------------------------------------------------------------
    // Auth-health reporting.
    // -----------------------------------------------------------------

    fn health_of(map: &ProviderHealthMap, id: &str) -> ProviderAuthHealth {
        map.read().expect("not poisoned").get(id).cloned().unwrap_or_default()
    }

    /// **The risk-7 proof, and the reason `send` records where it does.**
    ///
    /// Five of the six sites read the body with `?` *before* the status is
    /// turned into an error, so a 401 whose body cannot be read produces no
    /// `ProviderHttpError` at all — pinned as pre-existing by
    /// `create_order_unreadable_body_keeps_its_own_context` above. If the sink
    /// were fed from that error, or from anywhere below the body read, a
    /// revoked key on a flaky link would silently never count and the alert
    /// would never fire. Recording off the **status** is what closes it.
    ///
    /// Move the `sink.record(..)` call in `send` below the `let raw = ...`
    /// block and this test fails while every message-shape test above still
    /// passes — that gap is exactly what it exists to cover.
    #[tokio::test]
    async fn a_401_records_even_when_the_body_cannot_be_read() {
        let base = spawn_truncated_body_401().await;
        let map: ProviderHealthMap = Default::default();
        let client =
            ZapriteClient::new(&base, "k").with_sink(ProviderHealthSink::new(map.clone(), "prov-1"));

        let err = client
            .create_order(&order_body())
            .await
            .expect_err("unreadable body must error");

        // The error is still the body-read context, not an HTTP-status error:
        // this is the pre-existing behavior, deliberately unchanged.
        assert_eq!(err.to_string(), "read create_order body");
        assert!(err.downcast_ref::<ProviderHttpError>().is_none());

        // ...and the 401 counted anyway.
        let h = health_of(&map, "prov-1");
        assert_eq!(h.auth_dead.consecutive, 1);
        assert_eq!(h.last_status, Some(401));
    }

    #[tokio::test]
    async fn a_401_from_a_real_client_reaches_the_sink() {
        let base = spawn_stub(StatusCode::UNAUTHORIZED, "invalid token").await;
        let map: ProviderHealthMap = Default::default();
        let client =
            ZapriteClient::new(&base, "k").with_sink(ProviderHealthSink::new(map.clone(), "prov-1"));

        // Every site reports, including `ping` — whose label is the operator's
        // Connect validation and is deliberately NOT a probe label, so its
        // failures count like any other.
        client.create_order(&order_body()).await.unwrap_err();
        client.get_order("ord-1").await.unwrap_err();
        client.charge_order_with_profile("ord-1", "pp-1").await.unwrap_err();
        client.create_contact("buyer@example.test", None).await.unwrap_err();
        client.get_contact("con-1").await.unwrap_err();
        client.ping().await.unwrap_err();

        let h = health_of(&map, "prov-1");
        assert_eq!(h.auth_dead.consecutive, 6);
        assert_eq!(h.last_status, Some(401));
    }

    /// A 2xx whose body will not deserialize still records a **success** — the
    /// deliberate corollary of recording off the status rather than off the
    /// call's outcome. A malformed response says nothing about the API key.
    #[tokio::test]
    async fn a_2xx_that_fails_to_parse_still_records_a_success() {
        let base = spawn_stub(StatusCode::OK, "not json at all").await;
        let map: ProviderHealthMap = Default::default();
        let client =
            ZapriteClient::new(&base, "k").with_sink(ProviderHealthSink::new(map.clone(), "prov-1"));

        let err = client
            .create_order(&order_body())
            .await
            .expect_err("unparseable body must error");
        assert_eq!(err.to_string(), "parse create_order response");

        let h = health_of(&map, "prov-1");
        assert!(h.last_success_at.is_some());
        assert_eq!(h.auth_dead.consecutive, 0);
    }

    /// An outcome with no HTTP status records nothing in either direction.
    #[tokio::test]
    async fn a_transport_failure_records_nothing() {
        let map: ProviderHealthMap = Default::default();
        let client = ZapriteClient::new("http://127.0.0.1:1", "k")
            .with_sink(ProviderHealthSink::new(map.clone(), "prov-1"));

        client.create_order(&order_body()).await.unwrap_err();
        client.ping().await.unwrap_err();

        assert!(map.read().expect("not poisoned").is_empty());
    }

    /// A 2xx still flows through untouched: `ping` returns `Ok(())` without
    /// reading the body, and `create_order` still parses the body it read.
    #[tokio::test]
    async fn success_paths_are_unchanged() {
        let base = spawn_stub(StatusCode::OK, r#"{"id":"ord-1","checkoutUrl":"https://x/y"}"#).await;
        let client = ZapriteClient::new(&base, "k");

        client.ping().await.expect("2xx ping must succeed");

        let order = client
            .create_order(&order_body())
            .await
            .expect("2xx create_order must succeed");
        assert_eq!(order["id"], "ord-1");
    }
}
