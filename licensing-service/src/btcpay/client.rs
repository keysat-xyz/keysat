//! Minimal BTCPay Greenfield API client — only the endpoints this service
//! actually calls. Add more as needs grow.

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Clone)]
pub struct BtcpayClient {
    http: Client,
    base_url: String,
    api_key: String,
    store_id: String,
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
        }
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
            .http
            .post(&url)
            .header("Authorization", format!("token {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("calling BTCPay create-invoice")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "BTCPay create-invoice returned {status}: {text}"
            ));
        }

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
            .http
            .post(&url)
            .header("Authorization", format!("token {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("calling BTCPay pay-lightning-invoice")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "BTCPay pay-lightning-invoice returned {status}: {text}"
            ));
        }

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
            .http
            .get(&url)
            .header("Authorization", format!("token {}", self.api_key))
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow::anyhow!(
                "BTCPay get-invoice returned {}",
                resp.status()
            ));
        }
        Ok(resp.json().await?)
    }

    #[allow(dead_code)]
    pub fn store_id(&self) -> &str {
        &self.store_id
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
