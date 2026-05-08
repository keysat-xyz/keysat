//! Payment-provider abstraction.
//!
//! Today there's exactly one provider, BTCPay. v0.3 adds Zaprite. The
//! daemon stores the active provider as a trait object so adding new
//! providers is a single-impl drop-in.
//!
//! ## Why a trait
//!
//! Pre-v0.2 the daemon hard-coded BTCPay assumptions in `webhook.rs`,
//! `purchase.rs`, `reconcile.rs`, and `tipping.rs`. Adding Zaprite would
//! have meant either parallel code paths (gross) or post-hoc retrofitting
//! (worse). The `PaymentProvider` trait is a one-time refactor that lets
//! every later provider slot in cleanly.
//!
//! ## Trait surface
//!
//! Just the operations the rest of the daemon actually needs:
//!
//! - `kind()`         — provider identity, for logs / audit / admin UI
//! - `create_invoice` — make a hosted-checkout session, return a URL
//! - `get_invoice_status` — for the reconcile loop (webhook misses)
//! - `validate_webhook` — provider-specific signature scheme + parse
//! - `pay_lightning_invoice` — for the tip-recipient flow; default impl
//!   returns a "not supported" error so providers without a Lightning
//!   payout capability can stay silent.
//!
//! ## What stays out of the trait
//!
//! Provider-specific setup (OAuth-style consent flows, webhook
//! registration, store enumeration) lives in provider-specific modules
//! like `api::btcpay_authorize`. Those modules are responsible for
//! constructing a provider impl and handing it to
//! `AppState::set_payment_provider`.

use anyhow::Result;
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use std::any::Any;

pub mod btcpay;
pub mod zaprite;

/// Settings-table key that records which provider the operator
/// last activated. Used by the boot-time loader to pick which
/// provider to load when both `btcpay_config` and `zaprite_config`
/// are populated. Values: `'btcpay'` | `'zaprite'`. Absent means
/// "use whichever single provider is configured" (back-compat
/// for installs that pre-date this setting).
pub const SETTING_ACTIVE_PROVIDER: &str = "active_payment_provider";

/// Convenience getter for the active-provider setting. Returns
/// `Some(ProviderKind)` if the operator has explicitly chosen
/// one, `None` if they haven't (caller falls back to the
/// load-order heuristic).
pub async fn read_active_provider_preference(
    pool: &sqlx::SqlitePool,
) -> Option<ProviderKind> {
    match crate::db::repo::settings_get(pool, SETTING_ACTIVE_PROVIDER).await {
        Ok(Some(s)) => match s.as_str() {
            "btcpay" => Some(ProviderKind::Btcpay),
            "zaprite" => Some(ProviderKind::Zaprite),
            _ => None,
        },
        _ => None,
    }
}

/// Persist the operator's active-provider preference. Called by
/// the connect endpoints (Connect BTCPay, Connect Zaprite) and
/// by the new "Activate <provider>" endpoint that flips between
/// already-configured providers without re-authorizing.
pub async fn write_active_provider_preference(
    pool: &sqlx::SqlitePool,
    kind: ProviderKind,
) -> anyhow::Result<()> {
    let value = kind.as_str();
    crate::db::repo::settings_set(pool, SETTING_ACTIVE_PROVIDER, Some(value))
        .await
        .map_err(|e| anyhow::anyhow!("write active provider preference: {e:#}"))?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Btcpay,
    Zaprite,
}

impl ProviderKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::Btcpay => "btcpay",
            ProviderKind::Zaprite => "zaprite",
        }
    }
}

/// A monetary amount + the unit it's denominated in.
///
/// We carry currency through the system because v0.3 adds USD/EUR for
/// card payments via Zaprite. v0.2 still emits everything as `SAT`
/// since BTCPay invoices are sat-denominated for our flow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Money {
    /// The currency code. ISO 4217 for fiat; `SAT` and `BTC` for Bitcoin.
    pub currency: String,
    /// The amount in the currency's smallest indivisible unit (sats for
    /// BTC, cents for USD, etc.). Using i64 because integer math is
    /// cheaper than decimals and we never need fractional sats.
    pub amount: i64,
}

impl Money {
    pub fn sats(amount: i64) -> Self {
        Money {
            currency: "SAT".to_string(),
            amount,
        }
    }
}

/// Inputs for `create_invoice`. Bundled into a struct so the trait
/// signature stays stable as we add fields.
pub struct CreateInvoiceParams<'a> {
    pub amount: Money,
    /// Where the buyer is sent after a successful payment. The provider
    /// appends its own status fragments / query params as needed.
    pub redirect_url: &'a str,
    /// Arbitrary metadata pinned to the invoice on the provider's side.
    /// Used by Keysat to round-trip its internal invoice id back through
    /// webhook events (`metadata.orderId` for BTCPay; `externalOrderId`
    /// for Zaprite).
    pub metadata: serde_json::Value,
    /// Keysat's internal invoice id (UUID). Passed back in webhook
    /// events to correlate with the local row.
    pub external_order_id: &'a str,
    /// Buyer email if known. Some providers use this for receipts.
    pub buyer_email: Option<&'a str>,
}

/// Result of `create_invoice`. Whatever the provider returned, narrowed
/// to the two things the rest of Keysat actually needs.
#[derive(Debug, Clone)]
pub struct CreatedInvoiceHandle {
    /// Provider-side invoice id. BTCPay invoice id today; Zaprite order
    /// id later. Stored on the invoice row so we can reconcile.
    pub provider_invoice_id: String,
    /// Public URL the buyer is redirected to to pay.
    pub checkout_url: String,
}

/// Provider-agnostic invoice status used by the reconcile loop. Maps to
/// the daemon's existing `InvoiceStatus` model but stays decoupled so
/// the trait doesn't pull in domain types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderInvoiceStatus {
    Pending,
    Settled,
    Expired,
    Refunded,
    Invalid,
}

/// Parsed webhook event. Only the kinds Keysat actually acts on are
/// modeled; everything else falls into `Other` and is ignored.
#[derive(Debug, Clone)]
pub enum ProviderWebhookEvent {
    InvoiceSettled {
        provider_invoice_id: String,
    },
    InvoiceExpired {
        provider_invoice_id: String,
    },
    InvoiceInvalid {
        provider_invoice_id: String,
    },
    InvoiceRefunded {
        provider_invoice_id: String,
        refunded_amount: Option<Money>,
    },
    /// Anything else the provider sent. We log + 200 it so the provider
    /// stops retrying.
    Other {
        kind: String,
        provider_invoice_id: Option<String>,
    },
}

impl ProviderWebhookEvent {
    pub fn provider_invoice_id(&self) -> Option<&str> {
        match self {
            ProviderWebhookEvent::InvoiceSettled { provider_invoice_id }
            | ProviderWebhookEvent::InvoiceExpired { provider_invoice_id }
            | ProviderWebhookEvent::InvoiceInvalid { provider_invoice_id }
            | ProviderWebhookEvent::InvoiceRefunded {
                provider_invoice_id, ..
            } => Some(provider_invoice_id),
            ProviderWebhookEvent::Other {
                provider_invoice_id,
                ..
            } => provider_invoice_id.as_deref(),
        }
    }
}

/// Result of paying a Lightning invoice via the provider's LN node.
#[derive(Debug, Clone)]
pub struct PaymentReceipt {
    pub payment_hash: Option<String>,
    /// Raw provider response, for the audit log.
    pub raw: serde_json::Value,
}

/// The trait every payment provider implements.
///
/// Object-safe (uses `&dyn`/`Box<dyn>`) thanks to `#[async_trait]`. The
/// `Any` supertrait lets call sites that still need provider-specific
/// types (e.g., the BTCPay-specific authorize flow) downcast.
#[async_trait::async_trait]
pub trait PaymentProvider: Send + Sync + Any {
    fn kind(&self) -> ProviderKind;

    async fn create_invoice(
        &self,
        params: CreateInvoiceParams<'_>,
    ) -> Result<CreatedInvoiceHandle>;

    async fn get_invoice_status(
        &self,
        provider_invoice_id: &str,
    ) -> Result<ProviderInvoiceStatus>;

    /// Verify and parse a webhook delivery. Implementations are
    /// responsible for reading whatever signature header their provider
    /// uses, computing the expected HMAC, and constant-time comparing.
    fn validate_webhook(
        &self,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<ProviderWebhookEvent>;

    /// Pay a BOLT11 Lightning invoice via the provider's LN node.
    /// Default impl returns a "not supported" error so providers
    /// without LN payout capability don't have to override.
    async fn pay_lightning_invoice(&self, _bolt11: &str) -> Result<PaymentReceipt> {
        anyhow::bail!(
            "pay_lightning_invoice not supported by this payment provider"
        )
    }

    /// Hatch for compat-era downcasting. Lets `AppState`'s legacy
    /// `btcpay_client()` accessor reach the inner BTCPay-specific
    /// client. v0.3 will retire the compat accessors and remove this.
    fn as_any(&self) -> &dyn Any;
}
