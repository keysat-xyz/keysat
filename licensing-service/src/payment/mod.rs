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
//! - `get_invoice_status` — authoritative status + amount, for the reconcile
//!   loop (webhook misses) and the webhook settle-confirmation gate
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
use thiserror::Error;

pub mod btcpay;
/// Per-provider auth-health tracking — the rule that turns observed call
/// outcomes into an operator alert. Kept out of this module deliberately; see
/// `health::ProviderAuthHealth` for why.
pub mod health;
pub mod zaprite;

/// A provider API call that reached the provider and came back with a
/// non-success HTTP status.
///
/// Both provider clients (`crate::btcpay::client::BtcpayClient` and
/// `crate::payment::zaprite::client::ZapriteClient`) return this as a **bare**
/// `anyhow::Error::new(..)` — never under a `.context(..)`.
///
/// That is deliberate and load-bearing. The call site's operator-facing text
/// is carried in the `message` **field**, so both `{e}` and `{e:#}` render
/// exactly what they rendered before the clients were collapsed onto a single
/// `send()`, while `err.downcast_ref::<ProviderHttpError>()` still recovers
/// the status. Attaching the text as context instead would add a chained
/// suffix under `{e:#}`, and `api/purchase.rs:595` feeds that straight into
/// `AppError::Upstream`, whose payload is returned verbatim (`error.rs`
/// redacts only `Database | Internal`) in the body of the **unauthenticated**
/// `POST /v1/purchase`. So context-wrapping here would silently change a
/// public route's response. `tests/api.rs` pins that body.
///
/// `label` names the specific call (`"btcpay.create_invoice"`,
/// `"zaprite.ping"`, …), not just the provider. The provider-failure alert
/// rule has to tell a hot-path 403 (the operator cannot sell) from a
/// liveness-probe 403 (the probe touches a broader permission than the hot
/// path, so a key that sells fine can 403 there forever), and the label is
/// what carries that distinction to whatever consumes this error.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct ProviderHttpError {
    pub status: u16,
    pub label: &'static str,
    /// The call site's own wording, verbatim. A field rather than a
    /// `.context(..)` — see above.
    pub message: String,
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

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "btcpay" => Some(Self::Btcpay),
            "zaprite" => Some(Self::Zaprite),
            _ => None,
        }
    }
}

/// Buyer-facing payment method. The buy page renders a picker over these
/// (when a merchant profile exposes more than one); the routing layer maps
/// the buyer's pick to a specific provider via the profile's attached
/// providers + optional `merchant_profile_rail_preferences` tie-breakers.
///
/// Rails-per-provider-kind are **inherent** (declared by each provider
/// impl's `served_rails()` trait method), not configurable per provider
/// row. BTCPay serves Lightning + OnChain. Zaprite serves Card +
/// Lightning + OnChain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rail {
    Lightning,
    Onchain,
    Card,
}

impl Rail {
    pub fn as_str(&self) -> &'static str {
        match self {
            Rail::Lightning => "lightning",
            Rail::Onchain => "onchain",
            Rail::Card => "card",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "lightning" => Some(Self::Lightning),
            "onchain" | "on-chain" | "on_chain" => Some(Self::Onchain),
            "card" => Some(Self::Card),
            _ => None,
        }
    }
}

/// Static rails served by a provider kind. Returned by
/// `PaymentProvider::served_rails()`; centralized here so callers that
/// just want to know "what does kind X support" (e.g., the admin UI's
/// connect-flow guidance) don't have to instantiate a provider.
pub fn rails_for_kind(kind: ProviderKind) -> Vec<Rail> {
    match kind {
        ProviderKind::Btcpay => vec![Rail::Lightning, Rail::Onchain],
        ProviderKind::Zaprite => vec![Rail::Card, Rail::Lightning, Rail::Onchain],
    }
}

/// Build a typed `PaymentProvider` trait object from a `payment_providers`
/// row. Dispatch on `kind`. Used by the AppState provider cache when
/// resolving by provider id.
pub fn build_provider(
    row: &crate::db::repo::PaymentProviderRow,
    public_base_url: Option<&str>,
) -> anyhow::Result<std::sync::Arc<dyn PaymentProvider>> {
    use crate::btcpay::client::BtcpayClient;
    use crate::payment::btcpay::BtcpayProvider;
    use crate::payment::zaprite::{ZapriteClient, ZapriteProvider};

    match ProviderKind::parse(&row.kind) {
        Some(ProviderKind::Btcpay) => {
            let store_id = row.store_id.as_deref().ok_or_else(|| {
                anyhow::anyhow!("BTCPay provider row {} missing store_id", row.id)
            })?;
            let webhook_secret = row.webhook_secret.clone().unwrap_or_default();
            let client = BtcpayClient::new(&row.base_url, &row.api_key, store_id);
            let provider = BtcpayProvider::new(client, webhook_secret)
                .with_public_base(public_base_url.map(|s| s.to_string()));
            Ok(std::sync::Arc::new(provider))
        }
        Some(ProviderKind::Zaprite) => {
            let client = ZapriteClient::new(row.base_url.clone(), row.api_key.clone());
            Ok(std::sync::Arc::new(ZapriteProvider::new(client)))
        }
        None => Err(anyhow::anyhow!(
            "unknown payment provider kind {:?} on row {}",
            row.kind,
            row.id
        )),
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
    /// Ask the provider to prompt the buyer to save their payment
    /// profile for future merchant-initiated charges. Zaprite honors
    /// this for autopay-supporting rails (Stripe card, etc.); BTCPay
    /// has no equivalent concept and silently ignores it. Set
    /// `Some(true)` on the FIRST cycle of a recurring purchase so the
    /// renewal worker can later call `charge_order_with_profile`
    /// against the saved profile. `None` / `Some(false)` is the
    /// one-shot default.
    pub allow_save_payment_profile: Option<bool>,
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

/// The provider's current view of an invoice: its `status` plus the amount
/// the provider has the invoice denominated for. Returned by
/// `PaymentProvider::get_invoice_status`.
///
/// `amount` is the price the provider has on record for the invoice (what we
/// asked it to charge), normalized to `SAT` when the provider used a Bitcoin
/// unit. It is `None` when the response carried no parseable amount/currency.
/// `status` is the load-bearing settle gate; `amount` feeds only the
/// **advisory** settle-amount tripwire in `api::webhook` / `reconcile` —
/// callers treat `None` as "no opinion" and MUST NOT gate issuance on it.
/// See docs/guides/payments.md.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInvoiceSnapshot {
    pub status: ProviderInvoiceStatus,
    pub amount: Option<Money>,
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

    /// Payment rails this provider can settle. Default impl uses the
    /// static `rails_for_kind()` mapping; impls only override if they
    /// expose a non-default set (e.g., a degraded BTCPay configured
    /// without Lightning support — not currently a Keysat concern).
    fn served_rails(&self) -> Vec<Rail> {
        rails_for_kind(self.kind())
    }

    async fn create_invoice(
        &self,
        params: CreateInvoiceParams<'_>,
    ) -> Result<CreatedInvoiceHandle>;

    async fn get_invoice_status(
        &self,
        provider_invoice_id: &str,
    ) -> Result<ProviderInvoiceSnapshot>;

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
