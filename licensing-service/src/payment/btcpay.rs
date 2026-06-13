//! BTCPay implementation of the [`PaymentProvider`] trait.
//!
//! Wraps the existing `BtcpayClient` (in `crate::btcpay::client`) and
//! the existing webhook signature verifier
//! (`crate::btcpay::webhook::verify_signature`). All BTCPay-specific
//! types and HTTP shape stay in `crate::btcpay::*`; this file is just
//! the trait-shaped facade.

use super::{
    CreateInvoiceParams, CreatedInvoiceHandle, Money, PaymentProvider, PaymentReceipt,
    ProviderInvoiceSnapshot, ProviderInvoiceStatus, ProviderKind, ProviderWebhookEvent,
};
use crate::btcpay::client::BtcpayClient;
use crate::btcpay::webhook::{verify_signature, WebhookEvent as BtcpayWebhookEvent};
use anyhow::{anyhow, Context, Result};
use axum::http::HeaderMap;
use serde_json::Value;
use std::any::Any;

const BTCPAY_SIG_HEADER: &str = "BTCPay-Sig";

/// Active BTCPay provider. Wraps the lower-level HTTP client and the
/// HMAC secret that BTCPay signs webhooks with. Constructed by
/// `api::btcpay_authorize` after the operator completes the OAuth flow.
///
/// `public_base` is BTCPay's PUBLIC URL (the StartTunnel / clearnet
/// one). Optional because it may not be known yet during very-first-
/// boot. When set, every checkout URL returned by `create_invoice`
/// gets its host rewritten from the internal `.startos` hostname to
/// this public host, so buyers actually receive a URL they can open
/// in their browser.
pub struct BtcpayProvider {
    pub(crate) client: BtcpayClient,
    pub(crate) webhook_secret: String,
    pub(crate) public_base: Option<String>,
}

impl BtcpayProvider {
    pub fn new(client: BtcpayClient, webhook_secret: String) -> Self {
        Self {
            client,
            webhook_secret,
            public_base: None,
        }
    }

    pub fn with_public_base(mut self, public_base: Option<String>) -> Self {
        self.public_base = public_base.filter(|s| !s.trim().is_empty());
        self
    }

    /// Compat accessor for code paths that haven't yet migrated to the
    /// `PaymentProvider` trait. Returns the underlying BTCPay-specific
    /// client by clone (the client is `Clone` and stores only an HTTP
    /// client + a few strings; cloning is cheap).
    pub fn client(&self) -> &BtcpayClient {
        &self.client
    }

    pub fn webhook_secret(&self) -> &str {
        &self.webhook_secret
    }
}

/// Rewrite the host (scheme + host + port) of `url_in` to that of
/// `public_base`, preserving the path, query, and fragment. Used to
/// turn `http://btcpayserver.startos:23000/i/abc?x=y` into
/// `https://btcpay.keysat.xyz/i/abc?x=y` before handing the URL to a
/// buyer's browser. Returns the input unchanged if either URL fails
/// to parse — bad-URL handling stays in the caller.
///
/// `pub(crate)` so other modules (like `api::purchase`) can apply the
/// same rewrite when they go through the compat-shim BtcpayClient
/// path instead of the PaymentProvider trait.
pub(crate) fn rewrite_to_public(url_in: &str, public_base: &str) -> String {
    let parsed_in = match url::Url::parse(url_in) {
        Ok(u) => u,
        Err(_) => return url_in.to_string(),
    };
    let parsed_pub = match url::Url::parse(public_base) {
        Ok(u) => u,
        Err(_) => return url_in.to_string(),
    };
    let mut out = parsed_pub.clone();
    out.set_path(parsed_in.path());
    out.set_query(parsed_in.query());
    out.set_fragment(parsed_in.fragment());
    out.to_string()
}

#[async_trait::async_trait]
impl PaymentProvider for BtcpayProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Btcpay
    }

    async fn create_invoice(
        &self,
        params: CreateInvoiceParams<'_>,
    ) -> Result<CreatedInvoiceHandle> {
        // BTCPay invoices in our flow are sat-denominated. If a future
        // caller hands us non-sat money for BTCPay, fail loudly — that's
        // a programming error, not a runtime condition.
        if params.amount.currency != "SAT" {
            anyhow::bail!(
                "BTCPayProvider.create_invoice expected SAT-denominated amount, got {}",
                params.amount.currency
            );
        }
        // The existing BtcpayClient::create_invoice already takes
        // (amount_sats, metadata, redirect_url). We pass through.
        let metadata = enrich_metadata(params.metadata, params.external_order_id);
        let created = self
            .client
            .create_invoice(params.amount.amount, metadata, Some(params.redirect_url))
            .await
            .context("BTCPay create-invoice")?;

        // Rewrite the checkout URL's host to the public BTCPay URL so
        // buyers actually get a link they can open. BTCPay derives the
        // checkout URL from whatever URL we used to call its API
        // (internal Docker hostname `btcpayserver.startos:23000`) —
        // useless to a buyer's browser. If `public_base` is set we
        // swap the host; if not, log loudly because that's a misconfig.
        let checkout_url = match &self.public_base {
            Some(pb) => {
                let rewritten = rewrite_to_public(&created.checkout_link, pb);
                tracing::info!(
                    original = %created.checkout_link,
                    rewritten = %rewritten,
                    public_base = %pb,
                    "checkout URL rewritten for buyer-reachability"
                );
                rewritten
            }
            None => {
                tracing::warn!(
                    original = %created.checkout_link,
                    "checkout URL NOT rewritten — public_base is None. \
                     Set BTCPAY_PUBLIC_URL via the wrapper, or ensure \
                     BTCPay's interface list includes a clearnet domain. \
                     Buyer will see the internal Docker hostname which \
                     is unreachable from outside."
                );
                created.checkout_link
            }
        };

        Ok(CreatedInvoiceHandle {
            provider_invoice_id: created.id,
            checkout_url,
        })
    }

    async fn get_invoice_status(
        &self,
        provider_invoice_id: &str,
    ) -> Result<ProviderInvoiceSnapshot> {
        let raw = self
            .client
            .get_invoice(provider_invoice_id)
            .await
            .context("BTCPay get-invoice")?;
        let status = match raw.get("status").and_then(|v| v.as_str()).unwrap_or("Pending") {
            "Settled" | "Complete" => ProviderInvoiceStatus::Settled,
            "Expired" => ProviderInvoiceStatus::Expired,
            "Invalid" => ProviderInvoiceStatus::Invalid,
            // Refunded isn't a top-level BTCPay status; if BTCPay ever
            // reports it via metadata we'd handle here. For now it falls
            // through to Pending.
            _ => ProviderInvoiceStatus::Pending,
        };
        // The amount the invoice is denominated for, for the advisory
        // settle-amount tripwire (see docs/guides/payments.md). We price
        // BTCPay invoices in "BTC" with a decimal amount = sats / 1e8 (see
        // btcpay/client.rs::create_invoice), so convert that back to sats —
        // f64 is exact for sat-magnitude integers and mirrors the inverse
        // conversion already used in the client. Any other currency
        // shouldn't occur in our flow; pass it through verbatim so the
        // tripwire downstream flags the unexpected currency. Absent or
        // unparseable amount → None ("no opinion"; tripwire skips it).
        let amount = match (
            raw.get("currency").and_then(|v| v.as_str()),
            raw.get("amount").and_then(|v| v.as_str()),
        ) {
            (Some("BTC"), Some(amt)) => amt
                .parse::<f64>()
                .ok()
                .map(|btc| (btc * 100_000_000.0).round() as i64)
                // Guard against garbage from the provider (negative/zero/NaN
                // → 0): a real invoice amount is positive. Non-positive → None
                // ("no opinion"), so the advisory tripwire skips it.
                .filter(|&sats| sats > 0)
                .map(Money::sats),
            (Some(cur), Some(amt)) => amt.parse::<i64>().ok().map(|v| Money {
                currency: cur.to_string(),
                amount: v,
            }),
            _ => None,
        };
        Ok(ProviderInvoiceSnapshot { status, amount })
    }

    fn validate_webhook(
        &self,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<ProviderWebhookEvent> {
        let sig = headers
            .get(BTCPAY_SIG_HEADER)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow!("missing {BTCPAY_SIG_HEADER} header"))?;
        verify_signature(&self.webhook_secret, sig, body)
            .context("BTCPay webhook signature")?;

        let parsed: BtcpayWebhookEvent = serde_json::from_slice(body)
            .context("malformed BTCPay webhook body")?;

        Ok(match parsed.event_type.as_str() {
            "InvoiceSettled" | "InvoicePaymentSettled" => ProviderWebhookEvent::InvoiceSettled {
                provider_invoice_id: parsed.invoice_id,
            },
            "InvoiceExpired" => ProviderWebhookEvent::InvoiceExpired {
                provider_invoice_id: parsed.invoice_id,
            },
            "InvoiceInvalid" => ProviderWebhookEvent::InvoiceInvalid {
                provider_invoice_id: parsed.invoice_id,
            },
            other => ProviderWebhookEvent::Other {
                kind: other.to_string(),
                provider_invoice_id: Some(parsed.invoice_id),
            },
        })
    }

    async fn pay_lightning_invoice(&self, bolt11: &str) -> Result<PaymentReceipt> {
        let raw = self
            .client
            .pay_lightning_invoice(bolt11)
            .await
            .context("BTCPay pay-lightning-invoice")?;
        let payment_hash = raw
            .get("paymentHash")
            .and_then(|v| v.as_str())
            .map(String::from);
        Ok(PaymentReceipt { payment_hash, raw })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Helper: ensure the provider-side metadata always includes our
/// internal invoice id so webhook events are correlatable. BTCPay
/// preserves arbitrary metadata fields and returns them on
/// `get_invoice` and on webhook deliveries.
fn enrich_metadata(mut metadata: Value, external_order_id: &str) -> Value {
    if !metadata.is_object() {
        metadata = serde_json::json!({});
    }
    if let Some(obj) = metadata.as_object_mut() {
        // BTCPay's checkout displays `orderId` if present.
        obj.entry("orderId")
            .or_insert_with(|| Value::String(external_order_id.to_string()));
        obj.entry("source")
            .or_insert_with(|| Value::String("keysat".to_string()));
    }
    metadata
}

/// Money helper for callers translating from `i64` sat amounts.
pub fn sats(amount: i64) -> Money {
    Money::sats(amount)
}
