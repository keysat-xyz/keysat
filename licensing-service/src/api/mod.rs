//! HTTP API surface.
//!
//! Route layout (v1):
//!
//! | Method | Path                                   | Purpose                                     |
//! |--------|----------------------------------------|---------------------------------------------|
//! | GET    | `/`                                    | service info + public key                   |
//! | GET    | `/healthz`                             | health check                                |
//! | GET    | `/thank-you`                           | post-payment landing (BTCPay redirect tgt)  |
//! | GET    | `/admin/`                              | embedded admin web UI (SPA, client-gated)   |
//! | GET    | `/admin/<path>`                        | static assets for the embedded admin UI     |
//! | GET    | `/v1/pubkey`                           | PEM-encoded Ed25519 public key              |
//! | GET    | `/v1/products`                         | list active products                        |
//! | GET    | `/v1/products/:slug`                   | single product                              |
//! | POST   | `/v1/purchase`                         | start purchase, returns BTCPay URL          |
//! | GET    | `/v1/purchase/:invoice_id`             | poll purchase status + license if ready     |
//! | POST   | `/v1/redeem`                           | redeem a 'free_license' code, no BTCPay     |
//! | POST   | `/v1/validate`                         | validate a license key                      |
//! | POST   | `/v1/machines/activate`                | explicit seat activation                    |
//! | POST   | `/v1/machines/heartbeat`               | seat heartbeat                              |
//! | POST   | `/v1/machines/deactivate`              | free a seat (client-initiated)              |
//! | POST   | `/v1/btcpay/webhook`                   | BTCPay webhook landing                      |
//! | Admin endpoints require `Authorization: Bearer $KEYSAT_ADMIN_API_KEY`                 |
//! | POST   | `/v1/admin/products`                   | create product                              |
//! | PATCH  | `/v1/admin/products/:id/active`        | activate / deactivate                       |
//! | POST   | `/v1/admin/licenses`                   | manually issue license (comp/dev)           |
//! | GET    | `/v1/admin/licenses`                   | list licenses by product                    |
//! | GET    | `/v1/admin/licenses/search`            | search by email / npub / invoice            |
//! | GET    | `/v1/admin/licenses/summary`           | aggregate counts (total/active/24h/7d)      |
//! | GET    | `/v1/admin/revenue/summary`            | lifetime / 30d / 7d / 24h sats earned       |
//! | POST   | `/v1/admin/licenses/:id/revoke`        | revoke a license                            |
//! | POST   | `/v1/admin/licenses/:id/suspend`       | suspend (reversible)                        |
//! | POST   | `/v1/admin/licenses/:id/unsuspend`     | unsuspend                                   |
//! | POST   | `/v1/admin/policies`                   | create policy (license template)            |
//! | GET    | `/v1/admin/policies`                   | list policies for product                   |
//! | PATCH  | `/v1/admin/policies/:id/active`        | activate / deactivate policy                |
//! | GET    | `/v1/admin/machines`                   | list machines for a license                 |
//! | POST   | `/v1/admin/machines/:id/deactivate`    | force-kick a machine                        |
//! | POST   | `/v1/admin/webhook-endpoints`          | register webhook subscriber                 |
//! | GET    | `/v1/admin/webhook-endpoints`          | list webhook subscribers                    |
//! | PATCH  | `/v1/admin/webhook-endpoints/:id/active` | enable/disable                            |
//! | DELETE | `/v1/admin/webhook-endpoints/:id`      | delete webhook subscriber                   |
//! | POST   | `/v1/admin/discount-codes`             | create discount / referral code             |
//! | GET    | `/v1/admin/discount-codes`             | list discount codes                         |
//! | GET    | `/v1/admin/discount-codes/:id`         | one code with redemption history            |
//! | PATCH  | `/v1/admin/discount-codes/:id/active`  | enable / disable code                       |
//! | PATCH  | `/v1/admin/discount-codes/:id`         | edit amount / max_uses / expires / desc     |
//! | DELETE | `/v1/admin/discount-codes/:id`         | hard-delete (refused if redeemed)           |
//! | GET    | `/v1/discount-codes/preview`           | PUBLIC: preview discount on a product       |
//! | GET    | `/v1/admin/audit`                      | list audit log entries                      |
//! | POST   | `/admin/login`                         | PUBLIC: web UI password login (sets cookie) |
//! | POST   | `/admin/logout`                        | clear session cookie                        |
//! | GET    | `/admin/login/status`                  | PUBLIC: {has_password, logged_in}           |
//! | POST   | `/v1/admin/web-password`               | admin-only: set/rotate web UI password      |

pub mod admin;
pub mod admin_ui;
pub mod auth;
pub mod btcpay_authorize;
pub mod discount_codes;
pub mod machines;
pub mod policies;
pub mod products;
pub mod purchase;
pub mod subscriptions;
pub mod upgrade;
pub mod buy_page;
pub mod issuer_key;
pub mod redeem;
pub mod self_license;
pub mod session_layer;
pub mod tier;
pub mod validate;
pub mod community;
pub mod db_info;
pub mod payment_provider;
pub mod rates_admin;
pub mod recover;
pub mod zaprite_authorize;
pub mod webhook;
pub mod webhook_deliveries;
pub mod webhook_endpoints;

use crate::btcpay::client::BtcpayClient;
use crate::config::Config;
use crate::crypto::keys::ServerKeypair;
use crate::error::{AppError, AppResult};
use axum::{
    extract::FromRef,
    routing::{get, patch, post},
    Json, Router,
};
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub keypair: Arc<ServerKeypair>,
    /// Active payment provider (BTCPay today, Zaprite eventually).
    /// `None` until the operator completes a connect flow. Stored as
    /// `Arc<dyn ...>` so call sites get cheap clones; swapped under a
    /// write lock when the operator runs Connect / Disconnect.
    pub payment: Arc<RwLock<Option<Arc<dyn crate::payment::PaymentProvider>>>>,
    pub config: Arc<Config>,
    /// Keysat-licenses-Keysat tier. Read at boot, swapped when the
    /// operator activates a fresh license via the admin endpoint.
    pub self_tier: Arc<RwLock<crate::license_self::Tier>>,
    /// BTC/fiat rate cache for multi-currency products. See
    /// src/rates.rs. Process-global so cached rates aren't refetched
    /// per-request.
    pub rates: Arc<crate::rates::RateCache>,
}

impl AppState {
    /// Provider-agnostic accessor. New code should use this; legacy
    /// `btcpay_client()` / `btcpay_webhook_secret()` accessors remain
    /// for v0.2 compat and will retire as call sites migrate in v0.3.
    pub async fn payment_provider(
        &self,
    ) -> AppResult<Arc<dyn crate::payment::PaymentProvider>> {
        let guard = self.payment.read().await;
        guard
            .as_ref()
            .cloned()
            .ok_or(AppError::BtcpayNotConfigured)
    }

    /// Compat: returns the BTCPay-specific HTTP client, by clone, when
    /// the active provider is BTCPay. Falls back to
    /// `BtcpayNotConfigured` either when no provider is connected OR
    /// when the active provider isn't BTCPay (so Zaprite-only operators
    /// in v0.3 will get a clean error from BTCPay-specific code paths
    /// that haven't been migrated yet).
    pub async fn btcpay_client(&self) -> AppResult<BtcpayClient> {
        let guard = self.payment.read().await;
        let provider = guard.as_ref().ok_or(AppError::BtcpayNotConfigured)?;
        provider
            .as_any()
            .downcast_ref::<crate::payment::btcpay::BtcpayProvider>()
            .map(|p| p.client().clone())
            .ok_or(AppError::BtcpayNotConfigured)
    }

    /// Compat: returns the BTCPay HMAC webhook secret. See
    /// `btcpay_client()` for compat-error semantics.
    pub async fn btcpay_webhook_secret(&self) -> AppResult<String> {
        let guard = self.payment.read().await;
        let provider = guard.as_ref().ok_or(AppError::BtcpayNotConfigured)?;
        provider
            .as_any()
            .downcast_ref::<crate::payment::btcpay::BtcpayProvider>()
            .map(|p| p.webhook_secret().to_string())
            .ok_or(AppError::BtcpayNotConfigured)
    }

    /// Swap the active payment provider. Called by `btcpay_authorize`
    /// (and, later, `zaprite_authorize`).
    pub async fn set_payment_provider(
        &self,
        provider: Arc<dyn crate::payment::PaymentProvider>,
    ) {
        let mut guard = self.payment.write().await;
        *guard = Some(provider);
    }

    /// Clear the active payment provider (Disconnect flow).
    pub async fn clear_payment_provider(&self) {
        let mut guard = self.payment.write().await;
        *guard = None;
    }
}

impl FromRef<AppState> for SqlitePool {
    fn from_ref(app: &AppState) -> Self {
        app.db.clone()
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/healthz", get(healthz))
        .route("/thank-you", get(thank_you))
        // Public buyer-facing purchase page. Server-renders an HTML
        // page for a given product slug; the inlined form POSTs to
        // /v1/purchase and redirects to BTCPay checkout.
        .route("/buy/:slug", get(buy_page::render))
        // Admin web UI — embedded into the binary at compile time via
        // rust-embed (see api/admin_ui.rs). The HTML page itself is
        // public; the SPA gates access client-side using the admin API
        // key, which is enforced server-side on every /v1/admin/*
        // call.
        .route("/admin", get(admin_ui::admin_root_redirect))
        .route("/admin/", get(admin_ui::admin_index))
        .route("/admin/*path", get(admin_ui::admin_asset))
        .route("/v1/pubkey", get(pubkey))
        .route("/v1/products", get(products::list))
        .route("/v1/products/:slug", get(products::get))
        .route("/v1/purchase", post(purchase::start))
        .route("/v1/purchase/:invoice_id", get(purchase::status))
        .route("/v1/redeem", post(redeem::redeem))
        .route("/v1/validate", post(validate::validate))
        // Buyer self-service recovery (lost key → re-derive from
        // settled invoice + buyer email).
        .route("/recover", get(recover::page))
        .route("/v1/recover", post(recover::recover))
        // Client-facing machine endpoints.
        .route("/v1/machines/activate", post(machines::activate))
        .route("/v1/machines/heartbeat", post(machines::heartbeat))
        .route("/v1/machines/deactivate", post(machines::deactivate))
        .route("/v1/btcpay/webhook", post(webhook::handle))
        .route(
            "/v1/admin/btcpay/connect",
            post(btcpay_authorize::start_connect),
        )
        .route(
            "/v1/btcpay/authorize/callback",
            post(btcpay_authorize::callback).get(btcpay_authorize::callback_get),
        )
        .route(
            "/v1/admin/btcpay/status",
            get(btcpay_authorize::status),
        )
        .route(
            "/v1/admin/btcpay/disconnect",
            post(btcpay_authorize::disconnect),
        )
        .route(
            "/v1/admin/btcpay/payment-methods",
            get(btcpay_authorize::payment_methods),
        )
        // Zaprite — alternative payment provider with native fiat-card
        // support. The connect flow is much simpler than BTCPay's because
        // Zaprite doesn't have an OAuth-style consent endpoint; the
        // operator pastes an API key from their Zaprite dashboard.
        .route(
            "/v1/admin/zaprite/connect",
            post(zaprite_authorize::connect),
        )
        .route(
            "/v1/admin/zaprite/disconnect",
            post(zaprite_authorize::disconnect),
        )
        .route(
            "/v1/admin/zaprite/status",
            get(zaprite_authorize::status),
        )
        // Provider-agnostic active-payment-provider control.
        // Operators with both BTCPay and Zaprite configured can flip
        // the active one without re-running Connect.
        .route(
            "/v1/admin/payment-provider/status",
            get(payment_provider::status),
        )
        .route(
            "/v1/admin/payment-provider/activate",
            post(payment_provider::activate),
        )
        // Zaprite webhook landing — operator points Zaprite's
        // webhook setting at this URL. Same handler as
        // /v1/btcpay/webhook because the underlying validate_webhook
        // is on the trait surface and the active provider self-
        // identifies its event shape.
        .route("/v1/zaprite/webhook", post(webhook::handle))
        .route("/v1/admin/products", post(admin::create_product))
        .route(
            "/v1/admin/products/:id",
            patch(admin::update_product).delete(admin::delete_product),
        )
        .route(
            "/v1/admin/products/:id/active",
            patch(admin::set_product_active),
        )
        // Both GET (list) and POST (issue) on the same path — must be chained
        // onto a single MethodRouter, because axum's Router::route replaces.
        .route(
            "/v1/admin/licenses",
            get(admin::list_licenses).post(admin::issue_license),
        )
        .route(
            "/v1/admin/licenses/search",
            get(admin::search_licenses),
        )
        .route(
            "/v1/admin/licenses/summary",
            get(admin::licenses_summary),
        )
        .route(
            "/v1/admin/licenses/counts",
            get(admin::license_counts),
        )
        .route(
            "/v1/admin/revenue/summary",
            get(admin::revenue_summary),
        )
        .route(
            "/v1/admin/licenses/:id/revoke",
            post(admin::revoke_license),
        )
        .route(
            "/v1/admin/licenses/:id/suspend",
            post(admin::suspend_license),
        )
        .route(
            "/v1/admin/licenses/:id/unsuspend",
            post(admin::unsuspend_license),
        )
        // Policies (license templates).
        .route(
            "/v1/admin/policies",
            get(policies::list).post(policies::create),
        )
        .route(
            "/v1/admin/policies/:id",
            patch(policies::update).delete(policies::delete),
        )
        .route(
            "/v1/admin/policies/:id/active",
            patch(policies::set_active),
        )
        .route(
            "/v1/admin/policies/:id/public",
            patch(policies::set_public),
        )
        .route(
            "/v1/admin/policies/:id/tip",
            patch(policies::set_tip),
        )
        // Public tier listing — drives the /buy/<slug> tier picker.
        .route(
            "/v1/products/:slug/policies",
            get(policies::list_public_policies),
        )
        .route("/v1/admin/tips", get(policies::list_tips))
        // Subscriptions (recurring billing) — admin list + cancel.
        .route(
            "/v1/admin/subscriptions",
            get(subscriptions::admin_list),
        )
        .route(
            "/v1/admin/subscriptions/:id/cancel",
            post(subscriptions::admin_cancel),
        )
        // Buyer self-service cancel — auth via license key in the body.
        .route(
            "/v1/subscriptions/cancel",
            post(subscriptions::buyer_cancel),
        )
        // Tier upgrades (buyer self-service). Quote is read-only;
        // start kicks off a payment for the prorated charge.
        // Both auth via signed license_key in the body, same model
        // as /v1/recover and /v1/subscriptions/cancel.
        .route("/v1/upgrade-quote", post(upgrade::quote))
        .route("/v1/upgrade", post(upgrade::start))
        // Admin force-change: skip ladder rules, optional skip_payment
        // for comp upgrades. Bears full audit trail.
        .route(
            "/v1/admin/licenses/:id/change-tier",
            post(upgrade::admin_change),
        )
        // Machines (admin views).
        .route("/v1/admin/machines", get(machines::admin_list))
        .route(
            "/v1/admin/machines/:id/deactivate",
            post(machines::admin_deactivate),
        )
        // Webhook subscribers.
        .route(
            "/v1/admin/webhook-endpoints",
            get(webhook_endpoints::list).post(webhook_endpoints::create),
        )
        .route(
            "/v1/admin/webhook-endpoints/:id/active",
            patch(webhook_endpoints::set_active),
        )
        .route(
            "/v1/admin/webhook-endpoints/:id",
            axum::routing::delete(webhook_endpoints::delete),
        )
        // Webhook delivery history (the dead-letter inspection +
        // manual-retry surface; see webhook_deliveries.rs for why).
        .route(
            "/v1/admin/webhook-deliveries",
            get(webhook_deliveries::list),
        )
        .route(
            "/v1/admin/webhook-deliveries/:id/retry",
            post(webhook_deliveries::retry),
        )
        // Database health snapshot — operator-facing sanity check
        // against the catastrophic-loss risk; see db_info.rs.
        .route("/v1/admin/db-info", get(db_info::get))
        // BTC/fiat rate cache — operator-facing view of what the
        // daemon would quote for fiat-priced products. See
        // src/rates.rs for the source chain (Kraken → Coinbase
        // → CoinGecko) and TTL caching semantics.
        .route("/v1/admin/rates", get(rates_admin::get))
        .route("/v1/admin/rates/refresh", post(rates_admin::refresh))
        // Opt-in community analytics. Off by default; toggling on
        // requires the operator to confirm a collector URL.
        .route(
            "/v1/admin/community-analytics",
            get(community::get).post(community::set),
        )
        .route(
            "/v1/admin/community-analytics/reset",
            post(community::reset),
        )
        // Discount / referral codes.
        .route(
            "/v1/admin/discount-codes",
            get(discount_codes::list).post(discount_codes::create),
        )
        .route(
            "/v1/admin/discount-codes/:id",
            get(discount_codes::get_one)
                .patch(discount_codes::update)
                .delete(discount_codes::delete),
        )
        .route(
            "/v1/admin/discount-codes/:id/active",
            patch(discount_codes::set_active),
        )
        // Public preview — buyer hits this from the buy page when they
        // click Apply on a discount code. Returns kind + computed
        // discounted price, doesn't consume a redemption slot.
        .route(
            "/v1/discount-codes/preview",
            get(discount_codes::preview),
        )
        // Audit log.
        .route("/v1/admin/audit", get(admin::list_audit))
        // Live-mutable settings.
        .route(
            "/v1/admin/settings/operator-name",
            get(admin::get_operator_name).post(admin::set_operator_name),
        )
        // Keysat self-license (Keysat-licenses-Keysat).
        .route(
            "/v1/admin/self-license",
            get(self_license::status).post(self_license::activate),
        )
        // Issuer-key import — admin-only, master-bootstrap path. No
        // StartOS Action surface; documented in MASTER_KEYPAIR_PROCEDURE.md.
        .route("/v1/admin/import-issuer-key", post(issuer_key::import))
        // Public read of the issuer's signing public key — used by the
        // admin Overview "Embed your public key" tip and by SDK consumers.
        .route("/v1/issuer/public-key", get(issuer_key::public))
        // Tier model — drives the admin sidebar's persistent upgrade banner.
        .route("/v1/admin/tier", get(tier::admin_status))
        // Web-UI password auth (v0.1.0:28+).
        .route("/admin/login", post(auth::login))
        .route("/admin/logout", post(auth::logout))
        .route("/admin/login/status", get(auth::login_status))
        .route("/v1/admin/web-password", post(auth::set_password))
        // Bridge cookie-based sessions onto the existing API-key require_admin
        // guard. Has to be the last layer so it runs first (axum applies
        // layers in reverse-of-declaration order).
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            session_layer::session_to_bearer,
        ))
        .with_state(state)
}

async fn root(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<serde_json::Value> {
    // Live-read the operator name from the settings table so admin
    // updates take effect without a daemon restart. Falls back to the
    // env-var-loaded config if the DB row hasn't been set yet (fresh
    // installs, or installs that pre-date this feature).
    let operator = match crate::db::repo::settings_get(
        &state.db,
        crate::api::admin::SETTING_OPERATOR_NAME,
    )
    .await
    {
        Ok(Some(v)) => Some(v),
        _ => state.config.operator_name.clone(),
    };
    Json(json!({
        "service": "keysat",
        "version": env!("CARGO_PKG_VERSION"),
        "operator": operator,
        "public_key_pem": state.keypair.public_key_pem,
        "key_algorithm": "ed25519",
        "key_format_version": crate::crypto::KEY_VERSION,
    }))
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({ "ok": true }))
}

/// HTML "thank you" landing page that BTCPay redirects buyers to after a
/// settled invoice. Reads `?invoice_id=<id>` from the query string,
/// renders a Keysat-branded polling page that calls
/// /v1/purchase/<invoice_id> every few seconds until the response
/// includes a `license_key`, then renders the license inline in a
/// certificate-style card with a Copy button. Same visual language
/// as the buy page's free-license success state.
async fn thank_you(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Html<String> {
    let invoice_id = params.get("invoice_id").cloned().unwrap_or_default();
    let invoice_id_safe = html_escape(&invoice_id);
    let invoice_id_json = serde_json::to_string(&invoice_id).unwrap_or_else(|_| "\"\"".into());
    // Live-read operator_name from the settings table; fall back to the
    // env-var config; final fallback to a neutral brand name.
    let live = crate::db::repo::settings_get(
        &state.db,
        crate::api::admin::SETTING_OPERATOR_NAME,
    )
    .await
    .ok()
    .flatten();
    let operator_str = live
        .as_deref()
        .or(state.config.operator_name.as_deref())
        .unwrap_or("Keysat");
    let operator = html_escape(operator_str);
    let body = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Payment received — {operator}</title>
<link href="https://fonts.googleapis.com/css2?family=Manrope:wght@400;500;600;700&family=Inter:wght@400;500;600;700&family=JetBrains+Mono:wght@400;500;600&display=swap" rel="stylesheet">
<style>
:root {{
  --navy-950:#0E1F33; --navy-900:#142A47; --navy-800:#1E3A5F;
  --cream-50:#FBF9F2; --cream-100:#F5F1E8; --cream-200:#EDE7D7;
  --gold-700:#8A6F3D; --gold-500:#BFA068; --gold-400:#D4B985;
  --ink-900:#0E1F33; --ink-700:#2C3E54; --ink-500:#5A6B7F;
  --success:#2D7A5F; --success-bg:#E3F0EA;
  --danger:#B23A3A; --danger-bg:#F4E0E0;
  --border-1:rgba(14,31,51,0.12);
  --border-2:rgba(14,31,51,0.20);
  --font-display:'Manrope','Helvetica Neue',Arial,sans-serif;
  --font-body:'Inter','Helvetica Neue',Arial,sans-serif;
  --font-mono:'JetBrains Mono',ui-monospace,'SF Mono',Menlo,monospace;
  --shadow-md:0 2px 4px rgba(14,31,51,0.06),0 4px 12px rgba(14,31,51,0.06);
}}
*{{box-sizing:border-box}} html,body{{margin:0;padding:0}}
body {{
  font-family:var(--font-body); color:var(--ink-900);
  background:var(--cream-100);
  background-image:
    radial-gradient(rgba(14,31,51,0.025) 1px, transparent 1px),
    radial-gradient(rgba(138,111,61,0.022) 1px, transparent 1px);
  background-size:3px 3px, 7px 7px;
  -webkit-font-smoothing:antialiased; min-height:100vh;
}}
.topbar {{
  background:rgba(245,241,232,0.85); backdrop-filter:blur(10px);
  border-bottom:1px solid var(--border-1); padding:14px 24px;
}}
.topbar .inner {{
  max-width:680px; margin:0 auto;
  display:flex; align-items:center; gap:12px;
  font-family:var(--font-display); font-weight:500; font-size:14px;
  letter-spacing:0.28em; text-transform:uppercase; color:var(--navy-900);
}}
.topbar .operator {{
  font-family:var(--font-body); font-size:12px;
  letter-spacing:0.04em; text-transform:none;
  color:var(--ink-500); margin-left:auto;
}}
.wrap {{ max-width:560px; margin:48px auto; padding:0 24px; }}
.eyebrow {{
  font-size:11.5px; font-weight:700; letter-spacing:0.18em;
  text-transform:uppercase; color:var(--gold-700); margin-bottom:14px;
  display:inline-flex; align-items:center; gap:10px;
}}
.eyebrow::before {{ content:''; display:inline-block; width:28px; height:1px; background:var(--gold-500); }}
h1 {{
  font-family:var(--font-display); font-weight:500; font-size:38px;
  line-height:1.05; letter-spacing:-0.022em; color:var(--navy-950); margin:0 0 14px;
}}
.lede {{ font-size:16px; line-height:1.55; color:var(--ink-700); margin:0 0 28px; }}
.pending-card, .license-success, .error-card {{
  background:var(--cream-50); border:1px solid var(--border-1);
  border-radius:14px; box-shadow:var(--shadow-md);
  padding:32px 32px 28px; position:relative;
}}
.license-success, .pending-card {{
  box-shadow:0 0 0 1px var(--gold-500) inset, var(--shadow-md);
}}
.license-success::before, .license-success::after,
.pending-card::before, .pending-card::after {{
  content:''; position:absolute; left:14px; right:14px;
  height:1px; background:var(--gold-500); opacity:0.5;
}}
.license-success::before, .pending-card::before {{ top:14px; }}
.license-success::after, .pending-card::after {{ bottom:14px; }}
.stamp {{
  font-size:10px; font-weight:700; letter-spacing:0.22em;
  text-transform:uppercase; color:var(--gold-700);
  text-align:center; margin-bottom:16px;
}}
.pending-card h2 {{
  font-family:var(--font-display); font-weight:500; font-size:22px;
  color:var(--navy-950); margin:0 0 6px; letter-spacing:-0.015em; text-align:center;
}}
.pending-card .sub, .license-success .sub {{
  font-size:14px; color:var(--ink-500); text-align:center; margin:0 0 22px;
}}
.spinner {{
  width:32px; height:32px; border-radius:50%;
  border:3px solid var(--border-1); border-top-color:var(--gold-500);
  animation:spin 1s linear infinite;
  margin:18px auto 22px;
}}
@keyframes spin {{ to {{ transform:rotate(360deg); }} }}
.status-detail {{
  font-family:var(--font-mono); font-size:12.5px;
  background:var(--cream-100); border:1px solid var(--border-1);
  border-radius:7px; padding:8px 12px;
  color:var(--ink-700); text-align:center;
}}
.invoice-ref {{
  margin-top:12px; padding:8px 12px;
  font-family:var(--font-mono); font-size:11.5px;
  color:var(--ink-500); text-align:center;
}}
.invoice-ref code {{
  background:var(--cream-100); border:1px solid var(--border-1);
  padding:1px 6px; border-radius:5px; color:var(--ink-700);
}}
.license-success h2 {{
  font-family:var(--font-display); font-weight:500; font-size:22px;
  color:var(--navy-950); margin:0 0 6px; letter-spacing:-0.015em; text-align:center;
}}
.field-label {{
  font-size:11px; font-weight:600; letter-spacing:0.12em;
  text-transform:uppercase; color:var(--ink-500); margin-bottom:6px;
}}
.key-box {{
  background:var(--navy-950); color:var(--cream-50);
  padding:14px 16px; border-radius:8px;
  font-family:var(--font-mono); font-size:12.5px;
  word-break:break-all; line-height:1.5;
  display:flex; align-items:flex-start; gap:12px;
}}
.key-box .key-text {{ flex:1; }}
.key-box button {{
  background:rgba(245,241,232,0.10); color:var(--cream-50);
  border:0; padding:6px 10px; border-radius:6px;
  font-family:var(--font-body); font-size:11.5px; cursor:pointer;
  flex-shrink:0;
}}
.key-box button:hover {{ background:rgba(245,241,232,0.20); }}
.save-note {{
  margin-top:14px; font-size:13px; color:var(--ink-700);
  background:var(--cream-100); border:1px solid var(--border-1);
  border-radius:8px; padding:10px 14px;
}}
.save-note strong {{ color:var(--navy-950); }}
.error-card {{
  border-color:rgba(178,58,58,0.3); background:var(--danger-bg);
  color:#8a2828; font-size:14px;
}}
.hide {{ display:none !important; }}
footer.kfooter {{
  text-align:center; font-size:12px; color:var(--ink-500);
  margin-top:48px; padding:18px;
}}
footer.kfooter a {{ color:var(--ink-500); text-decoration:none; }}
footer.kfooter a:hover {{ color:var(--navy-900); }}
/* Mobile breakpoint — desktop-rhythm padding crowds 360-390px screens. */
@media (max-width:480px) {{
  .topbar {{ padding:12px 16px; }}
  .topbar .inner {{ font-size:13px; letter-spacing:0.22em; gap:8px; }}
  .topbar .operator {{ font-size:11px; }}
  .wrap {{ margin:24px auto; padding:0 16px; }}
  h1 {{ font-size:clamp(26px, 7vw, 38px); }}
  .lede {{ font-size:15px; margin:0 0 22px; }}
  .pending-card, .license-success, .error-card {{ padding:22px 20px 20px; }}
  .pending-card h2 {{ font-size:20px; }}
  footer.kfooter {{ margin-top:32px; padding:14px; }}
}}
</style>
</head>
<body>

<div class="topbar">
  <div class="inner">
    <span>Keysat</span>
    <span class="operator">Sold by {operator}</span>
  </div>
</div>

<div class="wrap">
  <div class="eyebrow">Payment received</div>
  <h1 id="page-title">Issuing your license&hellip;</h1>
  <p class="lede" id="page-lede">Your Bitcoin payment was received. We&rsquo;re waiting for it to settle on the network and for the license to be signed. This usually takes under a minute once the next block confirms.</p>

  <!-- pending state (default): polling for the license -->
  <div class="pending-card" id="pending-card">
    <div class="stamp">&mdash; Awaiting confirmation &mdash;</div>
    <h2>Hang tight.</h2>
    <p class="sub">This page will refresh automatically when your license is ready. Safe to bookmark this URL and come back later — your license will be here.</p>
    <div class="spinner" aria-hidden="true"></div>
    <div class="status-detail" id="status-detail">checking status&hellip;</div>
    <div class="invoice-ref" id="invoice-ref"></div>
  </div>

  <!-- success state: license card -->
  <div class="license-success hide" id="license-success" role="region" aria-label="License issued">
    <div class="stamp">&mdash; License issued &mdash;</div>
    <h2>You&rsquo;re licensed.</h2>
    <p class="sub">Your signed license is below. Save it before closing this tab.</p>
    <div class="field-label">License key</div>
    <div class="key-box">
      <span class="key-text" id="license-key-text">&hellip;</span>
      <button id="license-key-copy">Copy</button>
    </div>
    <div class="save-note">
      <strong>Save this somewhere safe.</strong> The key is signed at issue time and verifies offline against the seller&rsquo;s public key. You don&rsquo;t need to come back here.
    </div>
  </div>

  <!-- error state: invoice not found, or unrecoverable -->
  <div class="error-card hide" id="error-card" role="alert">
    <div id="error-msg">Something went wrong looking up this purchase.</div>
  </div>
</div>

<footer class="kfooter">
  <span>Powered by <a href="https://keysat.xyz" target="_blank" rel="noopener">Keysat</a> &middot; Bitcoin-paid software licensing</span>
</footer>

<script>
(function() {{
  const INVOICE_ID = {invoice_id_json};
  if (!INVOICE_ID) {{
    document.getElementById('pending-card').classList.add('hide');
    document.getElementById('error-card').classList.remove('hide');
    document.getElementById('error-msg').textContent = 'No invoice id supplied. Looking for your license? Check your email or contact the seller.';
    return;
  }}

  const pendingCard = document.getElementById('pending-card');
  const successCard = document.getElementById('license-success');
  const errorCard = document.getElementById('error-card');
  const statusDetail = document.getElementById('status-detail');
  const keyText = document.getElementById('license-key-text');
  const errorMsg = document.getElementById('error-msg');
  const pageTitle = document.getElementById('page-title');
  const pageLede = document.getElementById('page-lede');
  const invoiceRef = document.getElementById('invoice-ref');
  if (invoiceRef) {{
    invoiceRef.innerHTML = 'Reference for support: <code>' +
      INVOICE_ID.replace(/[<>&]/g, '') + '</code>';
  }}

  // Copy button.
  document.getElementById('license-key-copy').addEventListener('click', async function() {{
    try {{
      await navigator.clipboard.writeText(keyText.textContent);
      this.textContent = 'Copied';
      setTimeout(() => {{ this.textContent = 'Copy'; }}, 1400);
    }} catch (e) {{}}
  }});

  function showSuccess(licenseKey) {{
    pendingCard.classList.add('hide');
    errorCard.classList.add('hide');
    keyText.textContent = licenseKey;
    successCard.classList.remove('hide');
    pageTitle.textContent = 'Your license is ready.';
    pageLede.textContent = 'Save the key below — it verifies offline against the seller’s public key. You can close this tab when you’re done.';
  }}
  function showError(msg) {{
    pendingCard.classList.add('hide');
    successCard.classList.add('hide');
    errorMsg.textContent = msg;
    errorCard.classList.remove('hide');
    pageTitle.textContent = 'Something went wrong.';
    pageLede.textContent = 'See the message below for details.';
  }}

  // Adaptive polling: tight cadence for the first 2 minutes (most invoices
  // settle within one block), then back off so a slow block + clearnet flake
  // doesn't burn battery/data on the buyer's phone. URL is bookmark-friendly:
  // a buyer can close this tab and return any time — polling resumes from
  // wherever the invoice currently is.
  let attempt = 0;
  let elapsedMs = 0;
  const TIGHT_MS = 3000;     // 0–2 min  → poll every 3s
  const MED_MS   = 10000;    // 2–10 min → poll every 10s
  const SLOW_MS  = 30000;    // 10–30 min→ poll every 30s
  const TIGHT_DEADLINE = 2  * 60 * 1000;
  const MED_DEADLINE   = 10 * 60 * 1000;
  const HARD_DEADLINE  = 30 * 60 * 1000;

  function nextDelay() {{
    if (elapsedMs < TIGHT_DEADLINE) return TIGHT_MS;
    if (elapsedMs < MED_DEADLINE)   return MED_MS;
    return SLOW_MS;
  }}

  function waitingCopy(status) {{
    const min = Math.floor(elapsedMs / 60000);
    if (status === 'pending' || status === 'processing') {{
      if (min < 2) return 'invoice ' + status + ' — should settle within a block (~10 min).';
      if (min < 10) return 'invoice ' + status + ' — waiting for block confirmation. Safe to leave this tab open or bookmark this URL and come back.';
      return 'invoice ' + status + ' — slow block. Still polling. Bookmark this URL and refresh later if you close the tab.';
    }}
    return 'invoice status: ' + (status || 'pending');
  }}

  async function poll() {{
    attempt++;
    try {{
      const r = await fetch('/v1/purchase/' + encodeURIComponent(INVOICE_ID));
      if (r.status === 404) {{
        return showError('Invoice not found. The link may have been mistyped.');
      }}
      if (!r.ok) {{
        statusDetail.textContent = 'server returned HTTP ' + r.status + ' (will retry)';
        return scheduleNext();
      }}
      const j = await r.json();
      if (j.license_key) {{
        return showSuccess(j.license_key);
      }}
      const status = j.status || 'pending';
      statusDetail.textContent = waitingCopy(status);
      if (status === 'expired' || status === 'invalid') {{
        return showError('Payment was not completed (status: ' + status + '). If you sent funds, contact the seller and reference your invoice id above.');
      }}
      scheduleNext();
    }} catch (err) {{
      statusDetail.textContent = 'network error (retrying): ' + (err.message || err);
      scheduleNext();
    }}
  }}
  function scheduleNext() {{
    if (elapsedMs >= HARD_DEADLINE) {{
      statusDetail.textContent =
        'still waiting after 30 minutes. Bookmark this URL and refresh in a few minutes — your license will appear automatically once the invoice settles. If you still see this in an hour, contact the seller and reference the invoice id at the top of this page.';
      return;
    }}
    const d = nextDelay();
    elapsedMs += d;
    setTimeout(poll, d);
  }}
  poll();
}})();
</script>

</body>
</html>"#
    );
    axum::response::Html(body)
}

/// Minimal HTML escape for the operator name. Keeps this module dependency-free.
fn html_escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&#39;".to_string(),
            _ => c.to_string(),
        })
        .collect()
}

async fn pubkey(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<serde_json::Value> {
    Json(json!({
        "algorithm": "ed25519",
        "public_key_pem": state.keypair.public_key_pem,
    }))
}
