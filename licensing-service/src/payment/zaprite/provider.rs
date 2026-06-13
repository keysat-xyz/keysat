//! `PaymentProvider` trait impl for Zaprite.
//!
//! Translates the Keysat-side trait surface (typed enums, sat
//! denominations, abstract `ProviderWebhookEvent`) to/from
//! Zaprite's REST API (BTC currency code, JSON status enums,
//! externalUniqId-based webhook authentication).

use crate::payment::{
    CreateInvoiceParams, CreatedInvoiceHandle, Money, PaymentProvider, PaymentReceipt,
    ProviderInvoiceSnapshot, ProviderInvoiceStatus, ProviderKind, ProviderWebhookEvent,
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

        // If we're going to ask Zaprite to save the buyer's payment
        // profile (recurring first cycle), Zaprite REQUIRES an
        // explicit `contactId` on the order — passing only
        // `customerData: { email }` returns
        // `400 contactId is required when allowSavePaymentProfile is true`
        // even though their llms.txt docs claim contactId is
        // optional. The API is the source of truth, so we create a
        // contact first and pass its id below.
        //
        // Three paths:
        //   1. Recurring + buyer_email present → create contact,
        //      attach contactId, set allow_save_payment_profile=true.
        //   2. Recurring + buyer_email MISSING → can't create a
        //      contact (Zaprite requires email). Log a warning and
        //      degrade to one-shot mode for THIS cycle — the buyer
        //      gets a license, but subsequent renewals will fall
        //      through to manual-pay (zaprite_payment_profile_id
        //      stays NULL). Reason for degrading rather than failing:
        //      blocking the purchase entirely is worse than letting
        //      the operator collect cycle-1 revenue and prompt the
        //      buyer for an email at next renewal.
        //   3. Non-recurring → no contact needed; pass customerData
        //      only (current behavior preserved).
        let want_save_profile = params.allow_save_payment_profile == Some(true);
        let (contact_id, effective_allow_save) = if want_save_profile {
            match params.buyer_email {
                Some(email) => {
                    let contact = self
                        .client
                        .create_contact(email, None)
                        .await
                        .context("ZapriteProvider.create_invoice: create_contact")?;
                    let id = contact
                        .get("id")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            anyhow!(
                                "Zaprite create_contact response missing 'id': {contact}"
                            )
                        })?
                        .to_string();
                    (Some(id), Some(true))
                }
                None => {
                    tracing::warn!(
                        external_order_id = %params.external_order_id,
                        "recurring purchase has no buyer_email; degrading to one-shot \
                         (allow_save_payment_profile=false). Renewals for this \
                         subscription will fall back to manual-pay."
                    );
                    (None, None)
                }
            }
        } else {
            (None, params.allow_save_payment_profile)
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
            // `charge_order_with_profile`. May be reset to None
            // above if we couldn't satisfy Zaprite's contactId
            // requirement.
            allow_save_payment_profile: effective_allow_save,
            contact_id,
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
    ) -> Result<ProviderInvoiceSnapshot> {
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
        let status = match status_str {
            "PAID" | "COMPLETE" | "OVERPAID" => ProviderInvoiceStatus::Settled,
            "PENDING" | "PROCESSING" | "UNDERPAID" => ProviderInvoiceStatus::Pending,
            // Zaprite doesn't have explicit Expired/Refunded states
            // in the enum we saw — but they may surface those via
            // webhook events even when the order's "status" field
            // doesn't change. Fall-through covers any future
            // additions defensively.
            _ => ProviderInvoiceStatus::Invalid,
        };
        // The amount the order is denominated for, for the advisory
        // settle-amount tripwire (see docs/guides/payments.md). We create
        // Zaprite orders priced in "BTC" with the amount already in sats
        // (see create_invoice above), so a Bitcoin currency maps straight
        // to sats. Zaprite's order schema isn't fully documented, so this
        // is best-effort: an absent/unparseable amount yields None and the
        // tripwire is skipped. A non-Bitcoin currency is passed through so
        // the tripwire can flag the unexpected currency.
        let amount = match (
            order.get("currency").and_then(|v| v.as_str()),
            order.get("amount").and_then(|v| v.as_i64()),
        ) {
            // Zaprite spells Bitcoin as "BTC" with the amount already in sats
            // (see create_invoice above); "SAT" is accepted defensively. Both
            // map to our canonical sat unit. Non-positive → None (skip).
            (Some("BTC") | Some("SAT"), Some(sats)) if sats > 0 => Some(Money::sats(sats)),
            (Some(cur), Some(v)) if v > 0 => Some(Money {
                currency: cur.to_string(),
                amount: v,
            }),
            _ => None,
        };
        Ok(ProviderInvoiceSnapshot { status, amount })
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

        // Zaprite event shape: their docs don't enumerate event names
        // or payload shape. The `:49` sandbox test surfaced an empty
        // event_type because we were only checking the top-level
        // `event` field; Zaprite seems to put it elsewhere. We now
        // probe four common top-level field names — first non-empty
        // string wins. If even that fails, dump the raw payload at
        // WARN so we can see what Zaprite actually sends and add the
        // correct field name here.
        let event_type = ["event", "eventType", "type", "name"]
            .iter()
            .find_map(|field| {
                v.get(*field)
                    .and_then(|s| s.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
            })
            .unwrap_or_default();

        if event_type.is_empty() {
            // Truncated to 2KB to bound log volume on weird payloads.
            let raw_preview = String::from_utf8_lossy(body);
            let truncated = if raw_preview.len() > 2048 {
                format!(
                    "{}…[truncated {} bytes]",
                    &raw_preview[..2048],
                    raw_preview.len() - 2048
                )
            } else {
                raw_preview.to_string()
            };
            tracing::warn!(
                payload = %truncated,
                "Zaprite webhook: no event/eventType/type/name field found at top \
                 level — webhook will be treated as non-actionable. Inspect the \
                 payload above to find the actual event-name field and add it to \
                 the probe list in validate_webhook."
            );
        }
        let provider_invoice_id = v
            .pointer("/data/id")
            .or_else(|| v.pointer("/data/object/id"))
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
            // Zaprite's primary delivery shape (sandbox-confirmed :50):
            // a generic `order.change` event that just says "something
            // about this order changed" — the receiver has to look at
            // `/data/status` to figure out what actually changed. They
            // do NOT (empirically) send the convention-suggested
            // `order.paid` / `order.complete` events — every state
            // transition comes through as `order.change` and the
            // payload's status field tells the story. Branch on
            // status here so we dispatch the right action.
            //
            // Status values from Zaprite's get_invoice_status mapping:
            //   PAID | COMPLETE | OVERPAID  → settled
            //   EXPIRED                     → expired
            //   INVALID | CANCELLED         → invalid
            //   PENDING | PROCESSING |
            //   UNDERPAID                   → in-flight; no action yet
            //   <anything else>             → Other (logged + ignored)
            "order.change" => {
                let status = v
                    .pointer("/data/status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("");
                match status {
                    "PAID" | "COMPLETE" | "OVERPAID" => {
                        ProviderWebhookEvent::InvoiceSettled {
                            provider_invoice_id: id,
                        }
                    }
                    "EXPIRED" => ProviderWebhookEvent::InvoiceExpired {
                        provider_invoice_id: id,
                    },
                    "INVALID" | "CANCELLED" => {
                        ProviderWebhookEvent::InvoiceInvalid {
                            provider_invoice_id: id,
                        }
                    }
                    // In-flight transitions (PENDING/PROCESSING/UNDERPAID)
                    // and anything unfamiliar fall through to Other — the
                    // handler logs them as non-actionable, which is right:
                    // we don't want to fire the settle hook every time
                    // Zaprite transitions an order from PENDING to
                    // PROCESSING on the way to PAID. The terminal-state
                    // delivery is what actually drives our state machine.
                    _ => ProviderWebhookEvent::Other {
                        kind: format!("order.change[status={status}]"),
                        provider_invoice_id: provider_invoice_id,
                    },
                }
            }
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
