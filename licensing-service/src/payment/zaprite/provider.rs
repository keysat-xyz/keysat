//! `PaymentProvider` trait impl for Zaprite.
//!
//! Translates the Keysat-side trait surface (typed enums, sat
//! denominations, abstract `ProviderWebhookEvent`) to/from
//! Zaprite's REST API (BTC currency code, JSON status enums,
//! externalUniqId-based webhook authentication).

use crate::payment::{
    CreateInvoiceParams, CreatedInvoiceHandle, Money, PaymentProvider, PaymentReceipt,
    ProviderInvoiceStatus, ProviderKind, ProviderWebhookEvent,
};
use anyhow::{anyhow, Context, Result};
use axum::http::HeaderMap;
use serde_json::Value;
use std::any::Any;

use super::client::{CreateOrderBody, ZapriteClient};

#[derive(Debug, Clone)]
pub struct ZapriteProvider {
    client: ZapriteClient,
}

impl ZapriteProvider {
    pub fn new(client: ZapriteClient) -> Self {
        Self { client }
    }

    pub fn client(&self) -> &ZapriteClient {
        &self.client
    }
}

#[async_trait::async_trait]
impl PaymentProvider for ZapriteProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Zaprite
    }

    async fn create_invoice(
        &self,
        params: CreateInvoiceParams<'_>,
    ) -> Result<CreatedInvoiceHandle> {
        // Zaprite's currency enum spells Bitcoin as "BTC" with
        // amounts in the smallest indivisible unit (sats). Our
        // trait passes Money in either SAT or fiat; map both:
        //   SAT  → Zaprite "BTC", amount unchanged
        //   USD  → Zaprite "USD", amount in cents (already)
        //   EUR  → "EUR", same
        // Anything else → bail; we only ship the three currencies
        // the rest of Keysat understands today.
        let (currency_code, amount) = match params.amount.currency.as_str() {
            "SAT" => ("BTC", params.amount.amount),
            "USD" => ("USD", params.amount.amount),
            "EUR" => ("EUR", params.amount.amount),
            other => {
                return Err(anyhow!(
                    "ZapriteProvider.create_invoice: unsupported currency '{other}'; \
                     only SAT, USD, EUR mapped today"
                ))
            }
        };

        // Build the Zaprite order. externalUniqId carries OUR
        // invoice UUID; this is what the webhook handler uses as
        // the trust anchor (see `validate_webhook` below).
        let label = format!("Keysat order {}", params.external_order_id);
        let body = CreateOrderBody {
            amount,
            currency: currency_code,
            external_uniq_id: params.external_order_id,
            redirect_url: params.redirect_url,
            label: Some(&label),
            metadata: Some(params.metadata),
            customer_data: params.buyer_email.map(|email| {
                serde_json::json!({ "email": email })
            }),
            // For one-shot purchases (`None` / `Some(false)`) we
            // don't prompt the buyer to save their card. The
            // recurring-subscriptions purchase path sets this to
            // `Some(true)` on the FIRST cycle of a sub so Zaprite
            // shows the save-payment-profile prompt; subsequent
            // cycles are then merchant-initiated charges against
            // the saved profile via
            // `charge_order_with_profile`.
            allow_save_payment_profile: params.allow_save_payment_profile,
        };

        let order = self
            .client
            .create_order(&body)
            .await
            .context("ZapriteProvider.create_invoice")?;

        // Pull the fields we need from the response JSON.
        let provider_invoice_id = order
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Zaprite create_order response missing 'id': {order}"))?
            .to_string();
        let checkout_url = order
            .get("checkoutUrl")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow!("Zaprite create_order response missing 'checkoutUrl': {order}")
            })?
            .to_string();

        Ok(CreatedInvoiceHandle {
            provider_invoice_id,
            checkout_url,
        })
    }

    async fn get_invoice_status(
        &self,
        provider_invoice_id: &str,
    ) -> Result<ProviderInvoiceStatus> {
        let order = self
            .client
            .get_order(provider_invoice_id)
            .await
            .context("ZapriteProvider.get_invoice_status")?;
        // Zaprite enum: PENDING | PROCESSING | PAID | COMPLETE |
        // OVERPAID | UNDERPAID. We map liberally:
        //   PAID, COMPLETE, OVERPAID  → Settled (operator gets
        //                                paid; buyer's overpay is
        //                                their problem to reclaim
        //                                via Zaprite if they want)
        //   UNDERPAID                 → Pending (buyer hasn't
        //                                covered the full amount;
        //                                Zaprite waits for them
        //                                to top up before flipping
        //                                to PAID)
        //   PENDING, PROCESSING       → Pending
        //   <anything else>           → Invalid (defensive)
        let status_str = order
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        Ok(match status_str {
            "PAID" | "COMPLETE" | "OVERPAID" => ProviderInvoiceStatus::Settled,
            "PENDING" | "PROCESSING" | "UNDERPAID" => ProviderInvoiceStatus::Pending,
            // Zaprite doesn't have explicit Expired/Refunded states
            // in the enum we saw — but they may surface those via
            // webhook events even when the order's "status" field
            // doesn't change. Fall-through covers any future
            // additions defensively.
            _ => ProviderInvoiceStatus::Invalid,
        })
    }

    /// Validate an incoming webhook delivery from Zaprite.
    ///
    /// Zaprite does NOT expose an HMAC-style signing scheme for
    /// webhooks (verified via the public OpenAPI spec + dashboard
    /// inspection in May 2026 — see ZAPRITE_INTEGRATION_SPEC.md).
    /// Their docs explicitly designate receiver-side idempotency
    /// as the security model.
    ///
    /// Our defense: trust the **`externalUniqId`** in the payload,
    /// which we set to OUR local invoice UUID at order creation.
    /// An attacker spoofing a webhook would need to know a UUID
    /// we never put on the wire to reach a real local invoice.
    /// The webhook handler in `api::webhook` then re-resolves the
    /// row by Zaprite's `id` (also in the payload) and only acts
    /// if the local row exists in an expected state.
    ///
    /// We don't validate the headers at all here — there's no
    /// signature header to validate. If Zaprite later adds HMAC
    /// signing and exposes a secret, this function gets a
    /// constant-time HMAC-SHA256 verification step against the
    /// stored secret.
    fn validate_webhook(
        &self,
        _headers: &HeaderMap,
        body: &[u8],
    ) -> Result<ProviderWebhookEvent> {
        let v: Value = serde_json::from_slice(body)
            .context("Zaprite webhook body must be JSON")?;

        // Zaprite event shape (from OpenAPI excerpt + ecosystem
        // conventions): top-level `event` string + `data.id`
        // (the order UUID). Examples expected:
        //   order.paid, order.complete, order.overpaid, order.underpaid,
        //   order.pending, order.expired, order.refunded
        // We map liberally and let unknowns fall through to Other.
        let event_type = v
            .get("event")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let provider_invoice_id = v
            .pointer("/data/id")
            .or_else(|| v.get("orderId"))
            .or_else(|| v.get("id"))
            .and_then(|s| s.as_str())
            .map(|s| s.to_string());

        let id = provider_invoice_id.clone().ok_or_else(|| {
            anyhow!("Zaprite webhook payload missing order id: {v}")
        })?;

        Ok(match event_type.as_str() {
            "order.paid" | "order.complete" | "order.overpaid" => {
                ProviderWebhookEvent::InvoiceSettled {
                    provider_invoice_id: id,
                }
            }
            "order.expired" => ProviderWebhookEvent::InvoiceExpired {
                provider_invoice_id: id,
            },
            "order.invalid" | "order.cancelled" => ProviderWebhookEvent::InvoiceInvalid {
                provider_invoice_id: id,
            },
            "order.refunded" => ProviderWebhookEvent::InvoiceRefunded {
                provider_invoice_id: id,
                refunded_amount: None, // amount field shape TBD when we see a real refund event
            },
            other => ProviderWebhookEvent::Other {
                kind: other.to_string(),
                provider_invoice_id: provider_invoice_id,
            },
        })
    }

    /// Zaprite doesn't (currently) operate a Lightning node on
    /// behalf of operators — they broker payments TO the operator's
    /// connected wallet, but don't expose an outbound LN-pay API.
    /// Tipping flows that need outbound LN payments must use a
    /// BTCPay-connected operator instead.
    async fn pay_lightning_invoice(&self, _bolt11: &str) -> Result<PaymentReceipt> {
        anyhow::bail!(
            "ZapriteProvider does not support outbound Lightning payments. \
             Configure BTCPay as the active provider if you need tipping flows."
        )
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Money helper for callers translating from `i64` sat amounts.
#[allow(dead_code)] // exposed for symmetry with btcpay::sats; kept for v0.3 callers
pub fn sats(amount: i64) -> Money {
    Money::sats(amount)
}
