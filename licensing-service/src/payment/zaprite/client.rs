//! Thin HTTP client for Zaprite's `/v1/*` API.
//!
//! Maps directly to the OpenAPI spec at api.zaprite.com/openapi.json.
//! Returns the raw JSON shapes for now — the `ZapriteProvider` impl
//! turns them into the trait's typed enums.

use anyhow::{anyhow, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ZapriteClient {
    pub base_url: String,
    pub api_key: String,
    http: reqwest::Client,
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
        }
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

    /// `POST /v1/orders` — create an order. Returns the full order
    /// JSON so the caller can pull whichever fields it needs
    /// (`id`, `checkoutUrl`, `status`, etc.).
    pub async fn create_order(&self, body: &CreateOrderBody<'_>) -> Result<Value> {
        let url = format!("{}/v1/orders", self.base_url);
        let resp = self
            .http
            .post(&url)
            .headers(self.auth_headers()?)
            .json(body)
            .send()
            .await
            .context("Zaprite create_order request")?;
        let status = resp.status();
        let raw = resp.text().await.context("read create_order body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "Zaprite create_order returned HTTP {status}: {raw}"
            ));
        }
        serde_json::from_str(&raw).context("parse create_order response")
    }

    /// `GET /v1/orders/{id}` — fetch an order by Zaprite id OR by
    /// externalUniqId (Zaprite accepts either). Used by the
    /// reconcile loop to catch missed webhooks.
    pub async fn get_order(&self, order_id: &str) -> Result<Value> {
        let encoded = urlencoding::encode(order_id);
        let url = format!("{}/v1/orders/{encoded}", self.base_url);
        let resp = self
            .http
            .get(&url)
            .headers(self.auth_headers()?)
            .send()
            .await
            .context("Zaprite get_order request")?;
        let status = resp.status();
        let raw = resp.text().await.context("read get_order body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "Zaprite get_order({order_id}) returned HTTP {status}: {raw}"
            ));
        }
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
        let resp = self
            .http
            .post(&url)
            .headers(self.auth_headers()?)
            .json(&body)
            .send()
            .await
            .context("Zaprite charge_order_with_profile request")?;
        let status = resp.status();
        let raw = resp.text().await.context("read charge body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "Zaprite charge_order_with_profile returned HTTP {status}: {raw}"
            ));
        }
        serde_json::from_str(&raw).context("parse charge response")
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
        let resp = self
            .http
            .get(&url)
            .headers(self.auth_headers()?)
            .send()
            .await
            .context("Zaprite get_contact request")?;
        let status = resp.status();
        let raw = resp.text().await.context("read get_contact body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "Zaprite get_contact({contact_id}) returned HTTP {status}: {raw}"
            ));
        }
        serde_json::from_str(&raw).context("parse get_contact response")
    }

    /// Smoke test for Connect-flow validation. Pings `GET /v1/orders`
    /// (the list endpoint) — auth-guarded, so a 200 confirms the
    /// API key works against the right org.
    pub async fn ping(&self) -> Result<()> {
        let url = format!("{}/v1/orders?limit=1", self.base_url);
        let resp = self
            .http
            .get(&url)
            .headers(self.auth_headers()?)
            .send()
            .await
            .context("Zaprite ping request")?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(anyhow!(
            "Zaprite ping returned HTTP {status}: {body}"
        ))
    }
}
