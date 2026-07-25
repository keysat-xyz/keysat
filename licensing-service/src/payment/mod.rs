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
///
/// `provider_health` is the shared auth-health map; the client this builds is
/// bound to it under `row.id`, so every call it makes — through the trait, or
/// through the `as_any()` downcast escapes in `subscriptions.rs` that reach the
/// raw client past the trait — reports to the same tracker.
pub fn build_provider(
    row: &crate::db::repo::PaymentProviderRow,
    public_base_url: Option<&str>,
    provider_health: &health::ProviderHealthMap,
) -> anyhow::Result<std::sync::Arc<dyn PaymentProvider>> {
    use crate::btcpay::client::BtcpayClient;
    use crate::payment::btcpay::BtcpayProvider;
    use crate::payment::zaprite::{ZapriteClient, ZapriteProvider};

    let sink = health::ProviderHealthSink::new(provider_health.clone(), row.id.clone());

    match ProviderKind::parse(&row.kind) {
        Some(ProviderKind::Btcpay) => {
            let store_id = row.store_id.as_deref().ok_or_else(|| {
                anyhow::anyhow!("BTCPay provider row {} missing store_id", row.id)
            })?;
            let webhook_secret = row.webhook_secret.clone().unwrap_or_default();
            let client =
                BtcpayClient::new(&row.base_url, &row.api_key, store_id).with_sink(sink);
            let provider = BtcpayProvider::new(client, webhook_secret)
                .with_public_base(public_base_url.map(|s| s.to_string()));
            Ok(std::sync::Arc::new(provider))
        }
        Some(ProviderKind::Zaprite) => {
            let client =
                ZapriteClient::new(row.base_url.clone(), row.api_key.clone()).with_sink(sink);
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

    /// Liveness probe: make one cheap, authenticated, read-only call so the
    /// daemon learns whether this provider's API key still works.
    ///
    /// Driven by `reconcile::tick` on a
    /// [`PROBE_INTERVAL`](health::PROBE_INTERVAL) throttle, because detection is
    /// otherwise entirely passive: `reconcile` and `subscriptions` both
    /// early-return when idle, so an instance between sales makes zero provider
    /// calls and a revoked key stays invisible until a buyer reaches checkout.
    ///
    /// The result is only ever *observed* — `Ok(())` versus an error is not
    /// itself the signal. What matters is that the call reached the client's
    /// `send()`, which recorded the HTTP status against the provider's entry in
    /// [`health::ProviderHealthMap`] under that call's **probe label**, which is
    /// how a 403 here (a permission the sell path does not need) is kept out of
    /// the `cannot_sell` streak.
    ///
    /// **Deliberately no default body.** A defaulted trait method is the
    /// `pay_lightning_invoice` trap one line above: under an earlier design its
    /// `bail!`ing default recorded every Lightning tip as a provider failure. A
    /// defaulted probe would be quieter and worse — a provider that forgot to
    /// implement it would report healthy forever, which is precisely the state
    /// this whole feature exists to detect. Requiring the method makes a new
    /// provider decide, at compile time, how it answers.
    async fn probe_auth(&self) -> Result<()>;

    /// Hatch for compat-era downcasting. Lets `AppState`'s legacy
    /// `btcpay_client()` accessor reach the inner BTCPay-specific
    /// client. v0.3 will retire the compat accessors and remove this.
    fn as_any(&self) -> &dyn Any;
}

#[cfg(test)]
mod tests {
    //! [`build_provider`] is attachment sites 1 and 2 of the auth-health sink.
    //! These drive a provider it built against a throwaway 401 server and read
    //! the shared map back out, so nothing about the wiring is asserted from a
    //! hand-built value.
    //!
    //! The two paths worth proving separately are the ones the trait cannot
    //! see: the `as_any()` downcast escapes in `subscriptions.rs`, which reach
    //! the raw client past the trait, and `pay_lightning_invoice`, which never
    //! reaches a client at all on a non-Lightning provider.

    use super::health::{ProviderAuthHealth, ProviderHealthMap};
    use super::*;
    use axum::{http::StatusCode, Router};
    use tokio::net::TcpListener;

    /// Same shape as the stubs in `btcpay::client` and `zaprite::client`'s test
    /// modules. Duplicated rather than shared for the same reason they are:
    /// a shared helper would be a new module outside this change's blast radius.
    async fn spawn_stub(status: StatusCode, body: &'static str) -> String {
        let app = Router::new().fallback(move || async move { (status, body) });
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        format!("http://{addr}")
    }

    fn row(id: &str, kind: &str, base_url: &str) -> crate::db::repo::PaymentProviderRow {
        crate::db::repo::PaymentProviderRow {
            id: id.to_string(),
            merchant_profile_id: "profile-1".to_string(),
            kind: kind.to_string(),
            label: format!("Test {kind}"),
            api_key: "dead-key".to_string(),
            base_url: base_url.to_string(),
            webhook_id: None,
            webhook_secret: Some("secret".to_string()),
            store_id: Some("store-1".to_string()),
            connected_at: "2026-07-24T00:00:00Z".to_string(),
            updated_at: "2026-07-24T00:00:00Z".to_string(),
        }
    }

    fn health_of(map: &ProviderHealthMap, id: &str) -> ProviderAuthHealth {
        map.read().expect("not poisoned").get(id).cloned().unwrap_or_default()
    }

    fn invoice_params<'a>(redirect: &'a str, order_id: &'a str) -> CreateInvoiceParams<'a> {
        CreateInvoiceParams {
            amount: Money::sats(1000),
            redirect_url: redirect,
            metadata: serde_json::json!({}),
            external_order_id: order_id,
            buyer_email: None,
            allow_save_payment_profile: None,
        }
    }

    /// Site 1. The provider `build_provider` returns must report under the
    /// **row's** id, not some other key — a mis-bound key would file a revoked
    /// provider's failures against a healthy one.
    #[tokio::test]
    async fn build_provider_binds_the_btcpay_client_to_the_row_id() {
        let base = spawn_stub(StatusCode::UNAUTHORIZED, "revoked").await;
        let map: ProviderHealthMap = Default::default();

        let provider = build_provider(&row("prov-btc", "btcpay", &base), None, &map)
            .expect("build_provider");
        provider
            .create_invoice(invoice_params("https://x/thanks", "inv-1"))
            .await
            .unwrap_err();
        // The reconcile loop's call, through the same client.
        provider.get_invoice_status("inv-1").await.unwrap_err();

        let h = health_of(&map, "prov-btc");
        assert_eq!(h.auth_dead.consecutive, 2);
        assert_eq!(h.last_status, Some(401));
        assert_eq!(map.read().expect("not poisoned").len(), 1);
    }

    /// Site 2.
    #[tokio::test]
    async fn build_provider_binds_the_zaprite_client_to_the_row_id() {
        let base = spawn_stub(StatusCode::UNAUTHORIZED, "revoked").await;
        let map: ProviderHealthMap = Default::default();

        let provider = build_provider(&row("prov-zap", "zaprite", &base), None, &map)
            .expect("build_provider");
        provider
            .create_invoice(invoice_params("https://x/thanks", "inv-1"))
            .await
            .unwrap_err();

        let h = health_of(&map, "prov-zap");
        assert_eq!(h.auth_dead.consecutive, 1);
        assert_eq!(h.last_status, Some(401));
    }

    /// Two rows sharing one map keep separate streaks. This is what makes the
    /// map safe for a multi-provider profile, where one revoked key must not
    /// implicate the other provider.
    #[tokio::test]
    async fn two_rows_get_independent_entries() {
        let bad = spawn_stub(StatusCode::UNAUTHORIZED, "revoked").await;
        let good = spawn_stub(StatusCode::OK, r#"{"id":"ord-1","checkoutUrl":"https://x/y"}"#).await;
        let map: ProviderHealthMap = Default::default();

        let failing =
            build_provider(&row("prov-a", "zaprite", &bad), None, &map).expect("build_provider");
        let healthy =
            build_provider(&row("prov-b", "zaprite", &good), None, &map).expect("build_provider");

        failing
            .create_invoice(invoice_params("https://x/thanks", "inv-1"))
            .await
            .unwrap_err();
        healthy
            .create_invoice(invoice_params("https://x/thanks", "inv-2"))
            .await
            .expect("2xx must succeed");

        assert_eq!(health_of(&map, "prov-a").auth_dead.consecutive, 1);
        assert_eq!(health_of(&map, "prov-b").auth_dead.consecutive, 0);
        assert!(health_of(&map, "prov-b").last_success_at.is_some());
    }

    /// **The `as_any()` escapes.** `subscriptions.rs` reaches the raw
    /// `ZapriteClient` past the trait in two places — the capture path
    /// (`zaprite.client().get_order` / `get_contact`) and the auto-charge path
    /// (`zaprite.client().charge_order_with_profile`) — and those are the calls
    /// a recurring subscription actually makes. A trait decorator would have
    /// missed every one of them; that is precisely why tracking lives in the
    /// client. This drives the same downcast the production code does.
    #[tokio::test]
    async fn the_as_any_escape_paths_record() {
        let base = spawn_stub(StatusCode::UNAUTHORIZED, "revoked").await;
        let map: ProviderHealthMap = Default::default();
        let provider = build_provider(&row("prov-zap", "zaprite", &base), None, &map)
            .expect("build_provider");

        let zaprite = provider
            .as_any()
            .downcast_ref::<crate::payment::zaprite::ZapriteProvider>()
            .expect("downcast to ZapriteProvider");

        // subscriptions.rs capture path.
        zaprite.client().get_order("ord-1").await.unwrap_err();
        zaprite.client().get_contact("con-1").await.unwrap_err();
        // subscriptions.rs auto-charge path.
        zaprite
            .client()
            .charge_order_with_profile("ord-1", "pp-1")
            .await
            .unwrap_err();

        let h = health_of(&map, "prov-zap");
        assert_eq!(
            h.auth_dead.consecutive, 3,
            "every call reached past the trait must still reach the sink"
        );
    }

    /// `AppState::btcpay_client()` hands out a **clone** of the inner client
    /// (`api/mod.rs`: `.map(|p| p.client().clone())`), and the legacy BTCPay
    /// call sites work off that clone. Nothing else pins that a clone keeps its
    /// sink — it does only because the field is part of the derived `Clone`, so
    /// a hand-written `Clone` that forgot it would silently un-track every one
    /// of those call sites. This drives the exact expression that accessor uses.
    #[tokio::test]
    async fn a_cloned_btcpay_client_keeps_its_sink() {
        let base = spawn_stub(StatusCode::UNAUTHORIZED, "revoked").await;
        let map: ProviderHealthMap = Default::default();
        let provider = build_provider(&row("prov-btc", "btcpay", &base), None, &map)
            .expect("build_provider");

        let cloned = provider
            .as_any()
            .downcast_ref::<crate::payment::btcpay::BtcpayProvider>()
            .expect("downcast to BtcpayProvider")
            .client()
            .clone();

        cloned.get_invoice("inv-1").await.unwrap_err();

        assert_eq!(health_of(&map, "prov-btc").auth_dead.consecutive, 1);
    }

    /// The inert-status rule, end to end through a real client rather than at
    /// the pure-rule layer. A provider outage must neither manufacture an auth
    /// alert nor paper over one — and the second half is the dangerous one: a
    /// 5xx that quietly reset the streak would let a revoked key hide behind
    /// any flaky provider.
    #[tokio::test]
    async fn a_5xx_does_not_disturb_a_running_streak() {
        let map: ProviderHealthMap = Default::default();
        let bad = spawn_stub(StatusCode::UNAUTHORIZED, "revoked").await;
        let flaky = spawn_stub(StatusCode::BAD_GATEWAY, "upstream down").await;

        let revoked =
            build_provider(&row("prov-1", "zaprite", &bad), None, &map).expect("build_provider");
        revoked
            .create_invoice(invoice_params("https://x/thanks", "inv-1"))
            .await
            .unwrap_err();
        revoked
            .create_invoice(invoice_params("https://x/thanks", "inv-2"))
            .await
            .unwrap_err();

        // Same provider row, now answering 502.
        let outage =
            build_provider(&row("prov-1", "zaprite", &flaky), None, &map).expect("build_provider");
        outage
            .create_invoice(invoice_params("https://x/thanks", "inv-3"))
            .await
            .unwrap_err();

        let h = health_of(&map, "prov-1");
        assert_eq!(h.auth_dead.consecutive, 2, "a 502 must not reset");
        assert_eq!(h.last_status, Some(502));
        assert_eq!(h.last_success_at, None, "and must not count as a success");
    }

    /// **The tipping regression.** `pay_lightning_invoice` on a provider with
    /// no outbound Lightning bails without issuing any HTTP request, so it must
    /// record nothing at all. Tracking at the trait layer would have counted
    /// every tip attempt against a Zaprite-connected operator as a provider
    /// auth failure and eventually alerted on a perfectly healthy key.
    #[tokio::test]
    async fn pay_lightning_invoice_on_a_non_lightning_provider_records_nothing() {
        let base = spawn_stub(StatusCode::UNAUTHORIZED, "revoked").await;
        let map: ProviderHealthMap = Default::default();
        let provider = build_provider(&row("prov-zap", "zaprite", &base), None, &map)
            .expect("build_provider");

        // The real tipping call site (`tipping.rs`) invokes exactly this.
        let err = provider
            .pay_lightning_invoice("lnbc1...")
            .await
            .expect_err("a non-Lightning provider must refuse");
        assert!(err.to_string().contains("does not support outbound Lightning"));

        assert!(
            map.read().expect("not poisoned").is_empty(),
            "a refusal that never left the daemon is not a provider failure"
        );
    }
}
