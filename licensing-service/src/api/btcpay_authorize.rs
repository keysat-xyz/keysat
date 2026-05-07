//! BTCPay one-click authorize flow.
//!
//! Instead of making the operator generate an API key by hand and paste it
//! into a form, we use BTCPay's "authorize" redirect flow:
//!
//! 1. Operator clicks "Connect BTCPay" in StartOS — the wrapper action
//!    calls `POST /v1/admin/btcpay/connect` (with the admin bearer token)
//!    and gets back a BTCPay URL to open in the operator's browser.
//! 2. The operator, already logged into BTCPay on the same box, sees a
//!    consent page listing the permissions this service is requesting. They
//!    click **Authorize**.
//! 3. BTCPay POSTs back to our `/v1/btcpay/authorize/callback` with the
//!    newly-minted API key and the store(s) it was scoped to.
//! 4. We persist the key, pick the target store, register the webhook (with
//!    a freshly-generated secret), and save everything in `btcpay_config`.
//! 5. From that moment on, the `BtcpayProvider` (held as an `Arc<dyn
//!    PaymentProvider>` in `AppState.payment`) is populated
//!    and purchase / webhook endpoints work.
//!
//! If the callback fails for any reason, the operator is shown an error page
//! and can retry. The admin endpoint requires the admin bearer token; the
//! callback path uses the CSRF `state` token to tie a callback back to the
//! issuing operator session.

use crate::api::{admin::require_admin, AppState};
use crate::btcpay::client::{self as btcpay_client, BtcpayClient};
use crate::btcpay::config as btcpay_cfg;
use crate::error::{AppError, AppResult};
use crate::payment::btcpay::BtcpayProvider;
use std::sync::Arc;
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    Form, Json,
};
use data_encoding::BASE32_NOPAD;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Permissions we request on the authorize page. Each is namespaced by
/// `btcpay.store.*` which means BTCPay will prompt the operator to pick
/// which store(s) to grant.
const REQUESTED_PERMISSIONS: &[&str] = &[
    "btcpay.store.canviewstoresettings",
    "btcpay.store.canmodifystoresettings", // to register the webhook
    "btcpay.store.canviewinvoices",
    "btcpay.store.cancreateinvoice",
    "btcpay.store.canmodifyinvoices",
];

#[derive(Debug, Serialize)]
pub struct ConnectResp {
    /// URL the operator should open in their browser to authorize.
    pub authorize_url: String,
    /// CSRF state token tied to this round trip.
    pub state: String,
}

/// Admin endpoint: starts a connect round trip. Returns the BTCPay authorize
/// URL for the StartOS wrapper action to open in the operator's browser.
pub async fn start_connect(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<ConnectResp>> {
    require_admin(&state, &headers)?;

    // Idempotency: if BTCPay is already connected, refuse to issue a new
    // authorize URL. Re-clicking Connect today produces a duplicate
    // webhook subscription on BTCPay, which results in every payment
    // event being delivered to Keysat twice. Make the operator go
    // through Disconnect first if they really want to re-authorize.
    if let Ok(Some(existing)) = btcpay_cfg::load(&state.db).await {
        return Err(AppError::Conflict(format!(
            "BTCPay is already connected (store {}). Run 'Disconnect BTCPay' first if you need to re-authorize.",
            existing.store_id,
        )));
    }

    // Random 20-byte token, base32-encoded, for the CSRF `state` parameter.
    let mut raw = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut raw);
    let state_token = BASE32_NOPAD.encode(&raw);

    btcpay_cfg::record_authorize_state(&state.db, &state_token)
        .await
        .map_err(AppError::Internal)?;

    // Construct the authorize URL per BTCPay's docs.
    // https://docs.btcpayserver.org/API/Greenfield/v1/#api-keys
    //
    // CSRF state must travel inside the `redirect` URL itself, NOT as a
    // separate query param on the outer authorize URL. Empirical
    // observation against BTCPay: arbitrary query params on the
    // authorize URL are NOT forwarded to the redirect target. The
    // redirect URL is preserved verbatim, so any params we encode INTO
    // it survive the round-trip.
    let redirect = format!(
        "{}/v1/btcpay/authorize/callback?state={}",
        state.config.public_base_url,
        urlencoding::encode(&state_token),
    );
    let perm_params = REQUESTED_PERMISSIONS
        .iter()
        .map(|p| format!("permissions={}", urlencoding::encode(p)))
        .collect::<Vec<_>>()
        .join("&");

    // The authorize URL is followed by the operator's BROWSER, so the host
    // must be reachable from outside the container. Use the explicit
    // `btcpay_browser_url` if the wrapper provided it; fall back to
    // `btcpay_url` only for dev/local setups (where they're the same).
    let authorize_base = state
        .config
        .btcpay_browser_url
        .as_deref()
        .unwrap_or(&state.config.btcpay_url);
    let authorize_url = format!(
        "{}/api-keys/authorize?applicationName={}&applicationIdentifier={}&strict=true&selectiveStores=true&redirect={}&{perm_params}",
        authorize_base,
        urlencoding::encode("Keysat"),
        urlencoding::encode("keysat"),
        urlencoding::encode(&redirect),
    );

    Ok(Json(ConnectResp {
        authorize_url,
        state: state_token,
    }))
}

/// Fields BTCPay sends back on the callback. BTCPay POSTs `apiKey`,
/// `userId`, and `permissions[]` as a form body. It also preserves any
/// query-string parameters on the redirect URL — we use that for `state`.
#[derive(Debug, Deserialize)]
pub struct CallbackForm {
    #[serde(rename = "apiKey")]
    pub api_key: String,
    #[serde(rename = "userId")]
    pub user_id: Option<String>,
    // BTCPay posts `permissions` one-per-occurrence; serde_urlencoded turns
    // that into a repeated string. We don't actually need to parse them
    // individually — we just re-verify via list_stores.
    #[serde(default)]
    pub permissions: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub state: String,
}

/// The real callback endpoint — POST form-encoded.
pub async fn callback(
    State(state): State<AppState>,
    Query(q): Query<CallbackQuery>,
    Form(form): Form<CallbackForm>,
) -> AppResult<Response> {
    finish_connect(&state, &q.state, &form.api_key).await?;
    Ok(success_page("BTCPay connected successfully. You can close this tab and return to StartOS."))
}

/// Some BTCPay deployments send the apiKey back as a query string on a GET.
/// Handle that too for robustness.
#[derive(Debug, Deserialize)]
pub struct CallbackGetQuery {
    pub state: String,
    #[serde(rename = "apiKey")]
    pub api_key: Option<String>,
    /// Error message if BTCPay declined / operator clicked "Deny".
    pub error: Option<String>,
}

pub async fn callback_get(
    State(state): State<AppState>,
    Query(q): Query<CallbackGetQuery>,
) -> Response {
    if let Some(err) = q.error {
        return Html(format!(
            "<html><body><h2>BTCPay authorization failed</h2><p>{}</p></body></html>",
            html_escape::encode_text(&err)
        ))
        .into_response();
    }
    let Some(api_key) = q.api_key else {
        // Some installs POST; in that case a bare GET with no apiKey is
        // possible if the operator refreshes the tab. Redirect to root.
        return Redirect::to("/").into_response();
    };
    match finish_connect(&state, &q.state, &api_key).await {
        Ok(()) => success_page(
            "BTCPay connected successfully. You can close this tab and return to StartOS.",
        ),
        Err(e) => Html(format!(
            "<html><body><h2>BTCPay authorization failed</h2><p>{}</p></body></html>",
            html_escape::encode_text(&e.to_string())
        ))
        .into_response(),
    }
}

/// Admin endpoint: list payment methods configured on the connected
/// BTCPay store. Proxies to BTCPay's `/api/v1/stores/{id}/payment-methods`.
/// Used by the wrapper / future web UI to surface a "no wallet
/// configured" state.
pub async fn payment_methods(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let cfg = btcpay_cfg::load(&state.db)
        .await
        .map_err(AppError::Internal)?
        .ok_or(AppError::BtcpayNotConfigured)?;
    let methods = btcpay_client::list_payment_methods(&cfg.base_url, &cfg.api_key, &cfg.store_id)
        .await
        .map_err(|e| AppError::Upstream(format!("BTCPay list-payment-methods: {e}")))?;

    // Return both the raw array for callers that want detail, and a
    // boolean summary for the common "is anything configured?" check.
    let count = methods.len();
    Ok(Json(json!({
        "store_id": cfg.store_id,
        "count": count,
        "methods": methods,
    })))
}

/// Admin endpoint: report current BTCPay connection status.
pub async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;

    let cfg = btcpay_cfg::load(&state.db).await.map_err(AppError::Internal)?;
    Ok(Json(match cfg {
        None => json!({ "connected": false }),
        Some(c) => json!({
            "connected": true,
            "store_id": c.store_id,
            "webhook_id": c.webhook_id,
            "base_url": c.base_url,
        }),
    }))
}

// --- internals ---

async fn finish_connect(state: &AppState, state_token: &str, api_key: &str) -> AppResult<()> {
    btcpay_cfg::consume_authorize_state(&state.db, state_token)
        .await
        .map_err(|_| AppError::Unauthorized)?;

    let base_url = &state.config.btcpay_url;

    // Enumerate stores the key has access to. With `selectiveStores=true`
    // the operator picked specific stores during authorize; we pick the
    // first one that the key can see.
    let stores = btcpay_client::list_stores(base_url, api_key)
        .await
        .map_err(|e| AppError::Upstream(format!("BTCPay list-stores: {e}")))?;
    let store = stores
        .into_iter()
        .next()
        .ok_or_else(|| AppError::BadRequest(
            "The authorized API key has access to zero stores. Re-run connect and pick a store.".into()
        ))?;

    // Generate a strong webhook secret, then register the webhook on BTCPay.
    let mut raw_secret = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw_secret);
    let webhook_secret = BASE32_NOPAD.encode(&raw_secret);

    let callback_url = format!("{}/v1/btcpay/webhook", state.config.public_base_url);

    let created_webhook = btcpay_client::create_webhook(
        base_url,
        api_key,
        &store.id,
        &callback_url,
        &webhook_secret,
    )
    .await
    .map_err(|e| AppError::Upstream(format!("BTCPay create-webhook: {e}")))?;

    // Persist.
    let cfg = btcpay_cfg::BtcpayConfig {
        base_url: base_url.clone(),
        api_key: api_key.to_string(),
        store_id: store.id.clone(),
        webhook_id: Some(created_webhook.id.clone()),
        webhook_secret: webhook_secret.clone(),
    };
    btcpay_cfg::save(&state.db, &cfg)
        .await
        .map_err(AppError::Internal)?;

    // Swap runtime — wrap a fresh BtcpayProvider into the
    // PaymentProvider trait object held by AppState. Pass the
    // public-facing BTCPay URL too so that checkout URLs returned to
    // buyers get rewritten from the internal Docker hostname to a
    // browser-reachable host.
    let client = BtcpayClient::new(base_url, api_key, &store.id);
    let provider = Arc::new(
        BtcpayProvider::new(client, webhook_secret)
            .with_public_base(state.config.btcpay_public_url.clone()),
    );
    state.set_payment_provider(provider).await;

    tracing::info!(
        store = %store.id,
        store_name = %store.name,
        webhook_id = %created_webhook.id,
        "BTCPay connected via authorize flow"
    );
    Ok(())
}

fn success_page(msg: &str) -> Response {
    let body = format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>BTCPay connected</title>
<style>body{{font-family:system-ui,sans-serif;max-width:480px;margin:4rem auto;padding:1rem;line-height:1.5}}
h2{{color:#0a7}}</style></head>
<body><h2>✓ {msg}</h2></body></html>"#,
        msg = html_escape::encode_text(msg)
    );
    (StatusCode::OK, Html(body)).into_response()
}

/// Admin endpoint: disconnect BTCPay. Best-effort revocation of the
/// webhook + API key on BTCPay's side, then unconditional clear of the
/// local config row. If BTCPay is unreachable, the local state is still
/// cleared and the operator gets a warning to clean up BTCPay manually.
pub async fn disconnect(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = crate::api::admin::request_context(&headers);

    let cfg = btcpay_cfg::load(&state.db)
        .await
        .map_err(AppError::Internal)?;
    let Some(cfg) = cfg else {
        return Ok(Json(json!({
            "ok": true,
            "noop": true,
            "message": "BTCPay was not connected; nothing to do.",
        })));
    };

    // Capture metadata for the response BEFORE we clear local state.
    let store_id = cfg.store_id.clone();
    let webhook_id = cfg.webhook_id.clone();

    // Best-effort remote cleanup. We DON'T short-circuit if either of
    // these calls fails — the operator's intent is to disconnect, and
    // leaving local state pointing at a remote we no longer trust is
    // worse than leaving orphan state on the BTCPay side. Any failures
    // are surfaced in the response so the operator can manually clean
    // up on BTCPay if needed.
    let mut warnings: Vec<String> = Vec::new();
    if let Some(webhook_id) = webhook_id.as_deref() {
        if let Err(e) = btcpay_client::delete_webhook(
            &cfg.base_url,
            &cfg.api_key,
            &cfg.store_id,
            webhook_id,
        )
        .await
        {
            warnings.push(format!(
                "Could not delete BTCPay webhook {webhook_id}: {e}. \
                 You may want to manually delete it in BTCPay's store webhook settings."
            ));
        }
    }
    if let Err(e) = btcpay_client::revoke_api_key(&cfg.base_url, &cfg.api_key).await {
        warnings.push(format!(
            "Could not revoke BTCPay API key: {e}. \
             You may want to manually revoke it in BTCPay's account API-keys page."
        ));
    }

    btcpay_cfg::clear(&state.db)
        .await
        .map_err(AppError::Internal)?;

    // Replace the runtime payment provider so subsequent purchase
    // attempts return BtcpayNotConfigured cleanly.
    state.clear_payment_provider().await;

    let _ = crate::db::repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "btcpay.disconnect",
        Some("btcpay_config"),
        None,
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "store_id": store_id, "webhook_id": webhook_id }),
    )
    .await;

    Ok(Json(json!({
        "ok": true,
        "noop": false,
        "store_id": store_id,
        "webhook_id": webhook_id,
        "warnings": warnings,
    })))
}
