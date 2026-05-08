//! Zaprite payment-provider implementation.
//!
//! Zaprite is an alternative to BTCPay that brokers Bitcoin
//! settlement (Lightning + on-chain via the operator's connected
//! wallet) AND fiat card payments (via Stripe / Square / etc.). It
//! lets operators accept USD/EUR card payments without running
//! their own merchant infrastructure — a meaningful market
//! expansion for sellers whose customers don't all hold BTC.
//!
//! The Keysat-side surface is identical to BTCPay because both
//! providers implement the abstract `PaymentProvider` trait. The
//! call sites in `purchase.rs`, `webhook.rs`, `reconcile.rs`, and
//! `tipping.rs` don't know or care which provider is active.
//!
//! ## Auth model
//!
//! Bearer token. Operators create an API key at
//! `app.zaprite.com/org/<org_id>/settings/api`, paste it into the
//! Keysat admin UI's "Connect Zaprite" action, and the daemon
//! stores it in `zaprite_config` (DB-backed singleton row, encrypted
//! at the StartOS volume layer).
//!
//! ## Webhook security
//!
//! Zaprite does NOT publish a webhook signature scheme — neither
//! HMAC nor JWT. Their docs explicitly call out receiver-side
//! idempotency as the security model: "process the same business
//! event more than once."
//!
//! Our defense is the **externalUniqId round-trip**. When we
//! create an order via `POST /v1/orders` we attach our local
//! invoice UUID as `externalUniqId`. When a webhook arrives, the
//! validate_webhook impl extracts the order id from the payload,
//! looks up the local invoice by Zaprite's id (which we recorded
//! at create time), and only acts if the row exists and is in an
//! expected state. An attacker spoofing a webhook would need to
//! know a UUID we never put on the wire to reach a real local
//! invoice.
//!
//! See `ZAPRITE_INTEGRATION_SPEC.md` at the repo root for the
//! full design + the API discovery notes.

pub mod client;
pub mod config;
pub mod provider;

pub use client::ZapriteClient;
pub use provider::ZapriteProvider;
