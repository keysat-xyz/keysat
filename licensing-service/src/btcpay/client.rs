//! Minimal BTCPay Greenfield API client — only the endpoints this service
//! actually calls. Add more as needs grow.

use crate::payment::health::ProviderHealthSink;
use crate::payment::ProviderHttpError;
use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::SystemTime;

/// Whether a failing BTCPay call reads the response body for its message.
///
/// A named enum rather than a `bool` parameter, mirroring the same seam in
/// `payment::zaprite::client`, so the two adjacent chokepoints read alike and
/// the call sites say what they mean. (Zaprite's variant set differs — it has
/// sites that must read the body on the SUCCESS path too, which BTCPay has
/// none of.)
enum BodyRead {
    /// Failure path only, `unwrap_or_default()`: an unreadable body becomes
    /// `""` and never becomes an error of its own.
    LossyOnError,
    /// Never read. Only `get_invoice`, whose message omits the body.
    Never,
}

#[derive(Clone)]
pub struct BtcpayClient {
    http: Client,
    base_url: String,
    api_key: String,
    store_id: String,
    /// Where [`send`](Self::send) reports every observed HTTP status, so a
    /// revoked or de-scoped API key surfaces as an operator alert. `None` for a
    /// client built with no `payment_providers` row to attribute calls to.
    health: Option<ProviderHealthSink>,
}

/// Response subset from `POST /api/v1/stores/{storeId}/invoices`.
#[derive(Debug, Deserialize)]
pub struct CreatedInvoice {
    pub id: String,
    #[serde(rename = "checkoutLink")]
    pub checkout_link: String,
    pub status: String,
}

/// Fields we include when creating an invoice. BTCPay accepts many more; we
/// only send what we need.
#[derive(Debug, Serialize)]
struct CreateInvoiceRequest<'a> {
    amount: String,
    currency: &'a str,
    metadata: serde_json::Value,
    checkout: CheckoutOptions<'a>,
}

#[derive(Debug, Serialize)]
struct CheckoutOptions<'a> {
    #[serde(rename = "redirectURL")]
    redirect_url: Option<&'a str>,
    #[serde(rename = "redirectAutomatically")]
    redirect_automatically: bool,
}

impl BtcpayClient {
    pub fn new(base_url: &str, api_key: &str, store_id: &str) -> Self {
        Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("reqwest client"),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            store_id: store_id.to_string(),
            health: None,
        }
    }

    /// Bind this client's calls to the shared auth-health map for one
    /// `payment_providers` row.
    ///
    /// A builder rather than a `new()` parameter, deliberately: the provider id
    /// is not known at every construction site (the connect flows build a
    /// client before, or without, a row), and several test fixtures construct
    /// clients that have nothing to report to. Making it a parameter would put
    /// a `None` at all of those.
    pub fn with_sink(mut self, sink: ProviderHealthSink) -> Self {
        self.health = Some(sink);
        self
    }

    /// The one place a `&self` BTCPay request is executed and a non-success
    /// status is turned into an error. Every non-2xx from this client leaves
    /// through here **as** a [`ProviderHttpError`] — not merely carrying one
    /// as a source — which is what the failure-alert tracker keys on.
    ///
    /// The three call sites do **not** agree on wording, on whether a
    /// transport failure gets a `.context(..)`, or on whether the failure path
    /// reads the body — and all three differences are visible to operators in
    /// the logs. So `send` takes them as parameters rather than imposing one
    /// shape:
    ///
    /// - `transport_ctx` — `Some(ctx)` adds that context to a connect/timeout
    ///   failure; `None` propagates the bare `reqwest` error (`get_invoice`).
    /// - `body_read` — whether the failure path reads the response body.
    /// - `message` — builds the operator-facing text from the status and that
    ///   body. It becomes the error's `message` **field**, not a `.context(..)`
    ///   wrapper, so `{e:#}` stays byte-identical to the pre-collapse text;
    ///   see [`ProviderHttpError`] for why a public route depends on that.
    ///
    /// On success the `Response` is returned untouched, so each caller parses
    /// it exactly as it did before.
    async fn send(
        &self,
        req: reqwest::RequestBuilder,
        label: &'static str,
        transport_ctx: Option<&'static str>,
        body_read: BodyRead,
        message: impl FnOnce(reqwest::StatusCode, &str) -> String,
    ) -> Result<reqwest::Response> {
        let sent = req.send().await;
        let resp = match transport_ctx {
            Some(ctx) => sent.context(ctx)?,
            None => sent?,
        };

        let status = resp.status();

        // Report the outcome the instant the status is known — before the
        // success branch, and before any body read. Ordering is load-bearing:
        // it keeps this identical to Zaprite's `send`, where a body read sits
        // between the two and can fail with `?`, and it means a provider
        // answering 401 with a truncated body still counts. See the wiring note
        // on `ProviderAuthHealth`.
        if let Some(sink) = &self.health {
            sink.record(label, status.as_u16(), SystemTime::now());
        }

        if status.is_success() {
            return Ok(resp);
        }

        let body = match body_read {
            BodyRead::LossyOnError => resp.text().await.unwrap_or_default(),
            BodyRead::Never => String::new(),
        };
        Err(anyhow::Error::new(ProviderHttpError {
            status: status.as_u16(),
            label,
            message: message(status, &body),
        }))
    }

    /// Create an invoice priced in satoshis. BTCPay accepts "BTC" currency
    /// with decimal amounts; we convert sats → BTC here.
    pub async fn create_invoice(
        &self,
        amount_sats: i64,
        metadata: serde_json::Value,
        redirect_url: Option<&str>,
    ) -> Result<CreatedInvoice> {
        let url = format!(
            "{}/api/v1/stores/{}/invoices",
            self.base_url, self.store_id
        );
        let amount_btc = format!("{:.8}", amount_sats as f64 / 100_000_000.0);

        let body = CreateInvoiceRequest {
            amount: amount_btc,
            currency: "BTC",
            metadata,
            checkout: CheckoutOptions {
                redirect_url,
                redirect_automatically: true,
            },
        };

        let resp = self
            .send(
                self.http
                    .post(&url)
                    .header("Authorization", format!("token {}", self.api_key))
                    .header("Content-Type", "application/json")
                    .json(&body),
                "btcpay.create_invoice",
                Some("calling BTCPay create-invoice"),
                BodyRead::LossyOnError,
                |status, text| format!("BTCPay create-invoice returned {status}: {text}"),
            )
            .await?;

        let invoice: CreatedInvoice = resp
            .json()
            .await
            .context("parsing BTCPay create-invoice response")?;
        Ok(invoice)
    }

    /// Pay a BOLT11 Lightning invoice from the operator's BTCPay node.
    /// Used by the tip-recipient flow. Returns the BTCPay payment record so
    /// the caller can extract the payment hash and surface it in the audit
    /// log. Errors if the store has no internal LN node or the node refuses
    /// the payment (insufficient liquidity, invoice already paid, etc.).
    ///
    /// BTCPay endpoint:
    ///   POST /api/v1/stores/{storeId}/lightning/BTC/invoices/pay
    ///   { "BOLT11": "<bolt11>" }
    ///
    /// The BTC path-component is the cryptoCode; on BTCPay-Server it's
    /// always "BTC" for the Bitcoin Lightning network.
    pub async fn pay_lightning_invoice(&self, bolt11: &str) -> Result<serde_json::Value> {
        let url = format!(
            "{}/api/v1/stores/{}/lightning/BTC/invoices/pay",
            self.base_url, self.store_id
        );
        let body = json!({ "BOLT11": bolt11 });
        let resp = self
            .send(
                self.http
                    .post(&url)
                    .header("Authorization", format!("token {}", self.api_key))
                    .header("Content-Type", "application/json")
                    .json(&body),
                "btcpay.pay_lightning_invoice",
                Some("calling BTCPay pay-lightning-invoice"),
                BodyRead::LossyOnError,
                |status, text| format!("BTCPay pay-lightning-invoice returned {status}: {text}"),
            )
            .await?;

        let payment: serde_json::Value = resp
            .json()
            .await
            .context("parsing BTCPay pay-lightning-invoice response")?;
        Ok(payment)
    }

    /// Fetch invoice state for reconciliation on startup / admin queries.
    /// Not used in the hot path; webhooks are the source of truth.
    pub async fn get_invoice(&self, invoice_id: &str) -> Result<serde_json::Value> {
        let url = format!(
            "{}/api/v1/stores/{}/invoices/{}",
            self.base_url, self.store_id, invoice_id
        );
        let resp = self
            .send(
                self.http
                    .get(&url)
                    .header("Authorization", format!("token {}", self.api_key)),
                "btcpay.get_invoice",
                // Deliberately none: this site has always propagated the bare
                // reqwest transport error.
                None,
                // Deliberately Never: this site has never interpolated the body.
                BodyRead::Never,
                |status, _| format!("BTCPay get-invoice returned {status}"),
            )
            .await?;
        Ok(resp.json().await?)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    // Helper to quickly construct sample metadata for invoice correlation.
    pub fn invoice_metadata(product_id: &str, internal_invoice_id: &str) -> serde_json::Value {
        json!({
            "orderId": internal_invoice_id,
            "productId": product_id,
            "source": "keysat",
        })
    }
}

/// Standalone helpers for the authorize / bootstrap flow. These operate
/// *before* a full `BtcpayClient` exists, since we don't yet know which
/// store the API key is scoped to.

#[derive(Debug, Deserialize)]
pub struct StoreSummary {
    pub id: String,
    pub name: String,
}

/// List the stores the given API key has access to.
pub async fn list_stores(base_url: &str, api_key: &str) -> Result<Vec<StoreSummary>> {
    let url = format!("{}/api/v1/stores", base_url.trim_end_matches('/'));
    let resp = Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?
        .get(&url)
        .header("Authorization", format!("token {api_key}"))
        .send()
        .await
        .context("calling BTCPay list-stores")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "BTCPay list-stores returned {status}: {text}"
        ));
    }
    Ok(resp.json::<Vec<StoreSummary>>().await?)
}

#[derive(Debug, Deserialize)]
pub struct CreatedWebhook {
    pub id: String,
    pub secret: Option<String>,
}

/// Register a webhook on the given store pointing at `callback_url` and
/// subscribing to the three invoice lifecycle events we care about.
pub async fn create_webhook(
    base_url: &str,
    api_key: &str,
    store_id: &str,
    callback_url: &str,
    secret: &str,
) -> Result<CreatedWebhook> {
    let url = format!(
        "{}/api/v1/stores/{store_id}/webhooks",
        base_url.trim_end_matches('/')
    );
    let body = json!({
        "url": callback_url,
        "enabled": true,
        "automaticRedelivery": true,
        "secret": secret,
        "authorizedEvents": {
            "everything": false,
            "specificEvents": [
                "InvoiceSettled",
                "InvoiceExpired",
                "InvoiceInvalid",
            ],
        },
    });
    let resp = Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?
        .post(&url)
        .header("Authorization", format!("token {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .context("calling BTCPay create-webhook")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "BTCPay create-webhook returned {status}: {text}"
        ));
    }
    Ok(resp.json::<CreatedWebhook>().await?)
}

/// Delete a webhook on the given store. Used by the Disconnect flow so
/// that re-authorizing later doesn't leave behind a duplicate webhook
/// pointing at this Keysat install.
pub async fn delete_webhook(
    base_url: &str,
    api_key: &str,
    store_id: &str,
    webhook_id: &str,
) -> Result<()> {
    let url = format!(
        "{}/api/v1/stores/{store_id}/webhooks/{webhook_id}",
        base_url.trim_end_matches('/')
    );
    let resp = Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?
        .delete(&url)
        .header("Authorization", format!("token {api_key}"))
        .send()
        .await
        .context("calling BTCPay delete-webhook")?;
    if !resp.status().is_success() && resp.status().as_u16() != 404 {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "BTCPay delete-webhook returned {status}: {text}"
        ));
    }
    // 404 is treated as success — the webhook is already gone.
    Ok(())
}

/// Revoke a BTCPay API key. Best-effort — failures are logged by the
/// caller but don't block the local Disconnect from completing.
pub async fn revoke_api_key(base_url: &str, api_key: &str) -> Result<()> {
    let url = format!("{}/api/v1/api-keys/current", base_url.trim_end_matches('/'));
    let resp = Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?
        .delete(&url)
        .header("Authorization", format!("token {api_key}"))
        .send()
        .await
        .context("calling BTCPay revoke-api-key")?;
    if !resp.status().is_success() && resp.status().as_u16() != 404 {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "BTCPay revoke-api-key returned {status}: {text}"
        ));
    }
    Ok(())
}

/// List the payment methods configured on a store. Used by the
/// post-connect "missing wallet" detection. Returns the raw JSON array
/// because the per-method shape varies (onchain vs LN, BTC vs altcoins).
/// Empty array → no payment methods configured.
pub async fn list_payment_methods(
    base_url: &str,
    api_key: &str,
    store_id: &str,
) -> Result<Vec<serde_json::Value>> {
    let url = format!(
        "{}/api/v1/stores/{store_id}/payment-methods",
        base_url.trim_end_matches('/')
    );
    let resp = Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?
        .get(&url)
        .header("Authorization", format!("token {api_key}"))
        .send()
        .await
        .context("calling BTCPay list-payment-methods")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "BTCPay list-payment-methods returned {status}: {text}"
        ));
    }
    let raw: serde_json::Value = resp.json().await?;
    Ok(raw
        .as_array()
        .cloned()
        .unwrap_or_default())
}

/// Resolve the Bitcoin **network** a store settles on, for the scoped
/// payment-connect gate (`plans/agent-payment-connect-scope.md` §6.1).
///
/// Lists the store's payment methods, finds the on-chain BTC method
/// (`paymentMethodId` is `BTC-CHAIN` on BTCPay 2.x, `BTC` on 1.x — never
/// hardcode), fetches a receive address, and classifies the address prefix.
///
/// Returns:
/// - `Ok(Some(network))` when positively determined;
/// - `Ok(None)` when it **cannot** be determined (no on-chain method, no
///   address, Lightning-only store, BTCPay not yet synced → `503`, or an
///   unrecognized prefix). The caller MUST fail closed (treat `None` as
///   mainnet and deny the scoped connect).
///
/// The address endpoint requires `btcpay.store.canmodifystoresettings`, which
/// the daemon's authorize flow already requests (see `REQUESTED_PERMISSIONS`).
pub async fn fetch_onchain_network(
    base_url: &str,
    api_key: &str,
    store_id: &str,
) -> Result<Option<super::network::BitcoinNetwork>> {
    // Any failure to enumerate methods → undetermined → caller fails closed.
    // Swallow the error here (uniform with the non-2xx wallet/address branch
    // below) and log a body-free reason at warn; detail only at debug so an
    // upstream error body never lands in normal logs on this sensitive path.
    let methods = match list_payment_methods(base_url, api_key, store_id).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                store = %store_id,
                "fetch_onchain_network: could not list payment methods; network undetermined"
            );
            tracing::debug!(error = %format!("{e:#}"), "btcpay list-payment-methods error detail");
            return Ok(None);
        }
    };
    // Find the on-chain BTC method. Lightning ids (`BTC-LN`,
    // `BTC_LightningLike`, …) are deliberately excluded.
    let Some(pmid) = methods.iter().find_map(|m| {
        let id = m.get("paymentMethodId").and_then(|v| v.as_str())?;
        match id.to_ascii_uppercase().as_str() {
            "BTC-CHAIN" | "BTC" => Some(id.to_string()),
            _ => None,
        }
    }) else {
        return Ok(None); // no on-chain BTC method → undetermined → fail closed
    };

    // `pmid` is BTCPay-supplied; percent-encode it as a path segment so a
    // hostile/buggy server returning an odd id can't corrupt the URL (it would
    // only ever 4xx → Ok(None) → deny anyway, but keep the request well-formed).
    let url = format!(
        "{}/api/v1/stores/{store_id}/payment-methods/{}/wallet/address",
        base_url.trim_end_matches('/'),
        urlencoding::encode(&pmid),
    );
    let resp = Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?
        .get(&url)
        .header("Authorization", format!("token {api_key}"))
        .send()
        .await
        .context("calling BTCPay wallet/address")?;
    if !resp.status().is_success() {
        // 503 (BTCPay not synced / on-chain service down), 404/422 (no wallet),
        // 403 (insufficient perms) — none let us positively determine the
        // network, so report undetermined and let the caller fail closed.
        return Ok(None);
    }
    // A 2xx with a non-JSON body (misconfigured BTCPay) is likewise "can't
    // determine" → Ok(None). Parsing via Ok(None) instead of `?` also keeps any
    // body snippet reqwest attaches to a parse error out of warn-level logs.
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "btcpay wallet/address: non-JSON body; network undetermined");
            return Ok(None);
        }
    };
    let address = body.get("address").and_then(|v| v.as_str()).unwrap_or("");
    Ok(super::network::classify_address_network(address))
}

#[cfg(test)]
mod tests {
    //! Pins the three `&self` sites that were collapsed onto
    //! [`BtcpayClient::send`]: each must still render exactly the text it
    //! rendered before the collapse, and must now carry a
    //! [`ProviderHttpError`] with the right status and label.
    //!
    //! The six free functions below `impl BtcpayClient` are deliberately NOT
    //! covered here — they build their own reqwest client, cannot take
    //! `&self`, and were left untouched.

    use super::*;
    use crate::payment::health::{ProviderAuthHealth, ProviderHealthMap};
    use axum::{http::StatusCode, Router};
    use tokio::net::TcpListener;

    /// Nothing listens on port 1 (binding it needs root), so a connect there
    /// fails at the transport layer — before any HTTP status exists. Pins the
    /// half of `send`'s contract that the status-code tests cannot reach.
    const REFUSED: &str = "http://127.0.0.1:1";

    /// Throwaway HTTP server on an ephemeral port that answers every method
    /// and path with one fixed status + body. Same shape as `tests/worker.rs`'s
    /// `spawn_500_receiver`, the established local-stub pattern in this repo.
    async fn spawn_stub(status: StatusCode, body: &'static str) -> String {
        let app = Router::new().fallback(move || async move { (status, body) });
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn create_invoice_non_success_keeps_message_and_carries_typed_error() {
        let base = spawn_stub(StatusCode::PAYMENT_REQUIRED, "nope").await;
        let client = BtcpayClient::new(&base, "tok", "store1");

        let err = client
            .create_invoice(1000, json!({}), None)
            .await
            .expect_err("non-2xx must error");

        assert_eq!(
            err.to_string(),
            "BTCPay create-invoice returned 402 Payment Required: nope"
        );
        // The alternate form must be byte-identical too, not just `{e}`.
        // `api/purchase.rs:595` feeds `{e:#}` into `AppError::Upstream`, which
        // is returned verbatim in the body of the UNAUTHENTICATED
        // `POST /v1/purchase` — so a chained suffix here would change a public
        // route's response. Carrying the text as a field, not a context, is
        // what keeps these two equal.
        assert_eq!(
            format!("{err:#}"),
            "BTCPay create-invoice returned 402 Payment Required: nope"
        );

        let typed = err
            .downcast_ref::<ProviderHttpError>()
            .expect("must be a ProviderHttpError");
        assert_eq!(typed.status, 402);
        assert_eq!(typed.label, "btcpay.create_invoice");
    }

    #[tokio::test]
    async fn pay_lightning_invoice_non_success_keeps_message_and_carries_typed_error() {
        let base = spawn_stub(StatusCode::CONFLICT, "insufficient liquidity").await;
        let client = BtcpayClient::new(&base, "tok", "store1");

        let err = client
            .pay_lightning_invoice("lnbc1...")
            .await
            .expect_err("non-2xx must error");

        assert_eq!(
            err.to_string(),
            "BTCPay pay-lightning-invoice returned 409 Conflict: insufficient liquidity"
        );

        let typed = err
            .downcast_ref::<ProviderHttpError>()
            .expect("must be a ProviderHttpError");
        assert_eq!(typed.status, 409);
        assert_eq!(typed.label, "btcpay.pay_lightning_invoice");
    }

    /// `get_invoice` is the odd one out: it has never read the body on the
    /// failure path, so the body must NOT appear in the message even though
    /// the stub sends one.
    #[tokio::test]
    async fn get_invoice_non_success_omits_the_body_and_carries_typed_error() {
        let base = spawn_stub(StatusCode::NOT_FOUND, "no such invoice").await;
        let client = BtcpayClient::new(&base, "tok", "store1");

        let err = client
            .get_invoice("inv-1")
            .await
            .expect_err("non-2xx must error");

        assert_eq!(err.to_string(), "BTCPay get-invoice returned 404 Not Found");
        assert_eq!(
            format!("{err:#}"),
            "BTCPay get-invoice returned 404 Not Found"
        );
        assert!(
            !err.to_string().contains("no such invoice"),
            "get-invoice must not interpolate the response body"
        );

        let typed = err
            .downcast_ref::<ProviderHttpError>()
            .expect("must be a ProviderHttpError");
        assert_eq!(typed.status, 404);
        assert_eq!(typed.label, "btcpay.get_invoice");
    }

    /// A 2xx must still flow through untouched — `send` returns the response
    /// and the caller parses it as before.
    #[tokio::test]
    async fn get_invoice_success_still_parses_the_body() {
        let base = spawn_stub(StatusCode::OK, r#"{"id":"inv-1","status":"Settled"}"#).await;
        let client = BtcpayClient::new(&base, "tok", "store1");

        let v = client.get_invoice("inv-1").await.expect("2xx must succeed");
        assert_eq!(v["status"], "Settled");
    }

    #[tokio::test]
    async fn create_invoice_success_still_deserializes_the_typed_response() {
        let base = spawn_stub(
            StatusCode::OK,
            r#"{"id":"inv-1","checkoutLink":"https://pay.test/i/inv-1","status":"New"}"#,
        )
        .await;
        let client = BtcpayClient::new(&base, "tok", "store1");

        let inv = client
            .create_invoice(1000, json!({}), None)
            .await
            .expect("2xx must succeed");
        assert_eq!(inv.id, "inv-1");
        assert_eq!(inv.checkout_link, "https://pay.test/i/inv-1");
        assert_eq!(inv.status, "New");
    }

    // -----------------------------------------------------------------
    // Auth-health reporting. `send` is the only place a status is observed,
    // so these drive a real client against a real (throwaway) server and
    // read the shared map back out.
    // -----------------------------------------------------------------

    fn health_of(map: &ProviderHealthMap, id: &str) -> ProviderAuthHealth {
        map.read().expect("not poisoned").get(id).cloned().unwrap_or_default()
    }

    #[tokio::test]
    async fn a_401_from_a_real_client_reaches_the_sink() {
        let base = spawn_stub(StatusCode::UNAUTHORIZED, "revoked").await;
        let map: ProviderHealthMap = Default::default();
        let client = BtcpayClient::new(&base, "tok", "store1")
            .with_sink(ProviderHealthSink::new(map.clone(), "prov-1"));

        client
            .create_invoice(1000, json!({}), None)
            .await
            .expect_err("non-2xx must error");

        let h = health_of(&map, "prov-1");
        assert_eq!(h.auth_dead.consecutive, 1);
        assert_eq!(h.last_status, Some(401));
        assert!(h.auth_dead.first_failure_at.is_some());
    }

    /// Every `&self` site reports, not just the one wired first. `get_invoice`
    /// matters most here: it is the reconcile loop's call, so it is what a
    /// background sweep would surface a revoked key through.
    #[tokio::test]
    async fn every_self_site_reports_to_the_sink() {
        let base = spawn_stub(StatusCode::UNAUTHORIZED, "revoked").await;
        let map: ProviderHealthMap = Default::default();
        let client = BtcpayClient::new(&base, "tok", "store1")
            .with_sink(ProviderHealthSink::new(map.clone(), "prov-1"));

        client.create_invoice(1000, json!({}), None).await.unwrap_err();
        client.pay_lightning_invoice("lnbc1...").await.unwrap_err();
        client.get_invoice("inv-1").await.unwrap_err();

        assert_eq!(health_of(&map, "prov-1").auth_dead.consecutive, 3);
    }

    /// A 2xx records a success and clears a running streak — the recovery
    /// transition the admin card renders.
    #[tokio::test]
    async fn a_2xx_records_a_success_and_clears_the_streak() {
        let map: ProviderHealthMap = Default::default();

        let bad = spawn_stub(StatusCode::UNAUTHORIZED, "revoked").await;
        let failing = BtcpayClient::new(&bad, "tok", "store1")
            .with_sink(ProviderHealthSink::new(map.clone(), "prov-1"));
        failing.get_invoice("inv-1").await.unwrap_err();
        failing.get_invoice("inv-1").await.unwrap_err();
        assert_eq!(health_of(&map, "prov-1").auth_dead.consecutive, 2);

        let good = spawn_stub(StatusCode::OK, r#"{"status":"Settled"}"#).await;
        let ok = BtcpayClient::new(&good, "tok", "store1")
            .with_sink(ProviderHealthSink::new(map.clone(), "prov-1"));
        ok.get_invoice("inv-1").await.expect("2xx must succeed");

        let h = health_of(&map, "prov-1");
        assert_eq!(h.auth_dead.consecutive, 0);
        assert!(h.last_success_at.is_some());
    }

    /// A 2xx whose body will not deserialize still records a **success**. The
    /// recording point is the status, not the call's ultimate outcome — the
    /// deliberate corollary of recording before the body read. A parse failure
    /// says nothing about whether the API key works.
    #[tokio::test]
    async fn a_2xx_that_fails_to_parse_still_records_a_success() {
        let base = spawn_stub(StatusCode::OK, "not json at all").await;
        let map: ProviderHealthMap = Default::default();
        let client = BtcpayClient::new(&base, "tok", "store1")
            .with_sink(ProviderHealthSink::new(map.clone(), "prov-1"));

        let err = client
            .create_invoice(1000, json!({}), None)
            .await
            .expect_err("unparseable body must error");
        assert_eq!(err.to_string(), "parsing BTCPay create-invoice response");

        let h = health_of(&map, "prov-1");
        assert!(h.last_success_at.is_some());
        assert_eq!(h.auth_dead.consecutive, 0);
    }

    /// An outcome that never reached a status records nothing at all — not a
    /// failure, not a success. A provider being unreachable is not evidence
    /// about its API key, in either direction, so the map stays empty rather
    /// than gaining an inert entry.
    #[tokio::test]
    async fn a_transport_failure_records_nothing() {
        let map: ProviderHealthMap = Default::default();
        let client = BtcpayClient::new(REFUSED, "tok", "store1")
            .with_sink(ProviderHealthSink::new(map.clone(), "prov-1"));

        client.create_invoice(1000, json!({}), None).await.unwrap_err();
        client.get_invoice("inv-1").await.unwrap_err();

        assert!(map.read().expect("not poisoned").is_empty());
    }

    /// The transport path is the other half of what `send` parameterized, and
    /// nothing else pins it: two sites add a `.context(..)` and `get_invoice`
    /// deliberately adds none. A transport failure never yields a
    /// `ProviderHttpError` — there is no status to carry.
    #[tokio::test]
    async fn transport_failures_keep_their_per_site_context() {
        let client = BtcpayClient::new(REFUSED, "tok", "store1");

        let err = client
            .create_invoice(1000, json!({}), None)
            .await
            .expect_err("connect must fail");
        assert_eq!(err.to_string(), "calling BTCPay create-invoice");
        assert!(err.downcast_ref::<ProviderHttpError>().is_none());

        let err = client
            .pay_lightning_invoice("lnbc1...")
            .await
            .expect_err("connect must fail");
        assert_eq!(err.to_string(), "calling BTCPay pay-lightning-invoice");
        assert!(err.downcast_ref::<ProviderHttpError>().is_none());

        // `get_invoice` propagates the bare reqwest error, as it always has —
        // no Keysat wording is layered on top.
        let err = client
            .get_invoice("inv-1")
            .await
            .expect_err("connect must fail");
        assert!(
            !err.to_string().contains("BTCPay"),
            "get_invoice must not gain a context it never had, got: {err}"
        );
        assert!(err.downcast_ref::<ProviderHttpError>().is_none());
    }
}
