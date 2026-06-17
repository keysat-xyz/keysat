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

use crate::api::{
    admin::{require_admin, require_scope},
    api_keys::{require_provider_connect, ConnectInitiator},
    AppState,
};
use crate::btcpay::client::{self as btcpay_client, BtcpayClient};
use crate::btcpay::config as btcpay_cfg;
use crate::btcpay::network::BitcoinNetwork;
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
    /// Merchant profile the resulting provider row will attach to.
    pub merchant_profile_id: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct StartConnectReq {
    /// Which merchant profile to attach the BTCPay provider to. NULL =
    /// the default profile (single-profile operators never see this).
    #[serde(default)]
    pub merchant_profile_id: Option<String>,
    /// Operator-set label for the resulting payment_providers row. NULL =
    /// auto-generated from the profile name.
    #[serde(default)]
    pub label: Option<String>,
}

/// Admin endpoint: starts a connect round trip. Returns the BTCPay authorize
/// URL for the StartOS wrapper action to open in the operator's browser.
///
/// Accepts an optional `merchant_profile_id` so Pro/Patron operators can
/// connect multiple BTCPay stores onto different profiles side-by-side.
/// Single-profile operators (Creator tier, or anyone without an explicit
/// pick) get the default profile.
pub async fn start_connect(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<StartConnectReq>>,
) -> AppResult<Json<ConnectResp>> {
    // Master key → connect any network. Scoped key with `payment_providers:write`
    // → permitted ONLY on a sandbox daemon (outer gate); the non-mainnet inner
    // gate is enforced at callback time once the store is known. See
    // `plans/agent-payment-connect-scope.md` §5.
    let (actor_hash, initiator) = require_provider_connect(&state, &headers).await?;
    let scoped_initiator = matches!(initiator, ConnectInitiator::Scoped);
    let req = body.map(|Json(b)| b).unwrap_or_default();

    // Resolve the target merchant profile (defaulting to the default).
    let profile = match req.merchant_profile_id.as_deref() {
        Some(id) => crate::merchant_profiles::get(&state.db, id)
            .await?
            .ok_or_else(|| AppError::BadRequest(format!("merchant profile {id} not found")))?,
        None => crate::merchant_profiles::require_default(&state.db).await?,
    };

    // Idempotency: refuse to issue a new authorize URL if the same
    // profile already has a BTCPay provider attached. Re-clicking
    // Connect would otherwise INSERT-conflict at callback time (unique
    // index on (merchant_profile_id, kind)) AND register a duplicate
    // BTCPay webhook, producing duplicate-deliveries on every settle.
    let existing = crate::db::repo::list_payment_providers_for_profile(&state.db, &profile.id)
        .await?;
    if existing.iter().any(|p| p.kind == "btcpay") {
        return Err(AppError::Conflict(format!(
            "merchant profile '{}' already has a BTCPay provider attached. \
             Disconnect it first if you want to re-authorize, or pick a different profile.",
            profile.name
        )));
    }

    // Random 20-byte token, base32-encoded, for the CSRF `state` parameter.
    let mut raw = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut raw);
    let state_token = BASE32_NOPAD.encode(&raw);

    btcpay_cfg::record_authorize_state(
        &state.db,
        &state_token,
        Some(&profile.id),
        scoped_initiator,
        // Only stored for scoped connects (the callback's audit row). Master
        // connects are covered by the StartOS action audit trail.
        scoped_initiator.then_some(actor_hash.as_str()),
    )
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

    let _ = req.label; // captured but not yet used — see finish_connect TODO for the future round-trip
    Ok(Json(ConnectResp {
        authorize_url,
        state: state_token,
        merchant_profile_id: profile.id,
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
    Ok(success_page("BTCPay connected successfully. You can close this tab and return to Keysat."))
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
            "BTCPay connected successfully. You can close this tab and return to Keysat.",
        ),
        // Carry the error's HTTP status onto the HTML page so a denied connect
        // (e.g. a scoped key targeting a mainnet store -> 400) surfaces as a
        // non-2xx an agent can detect, not a misleading 200. Matches the POST
        // callback, which propagates the status via `?`.
        Err(e) => (
            e.status_code(),
            Html(format!(
                "<html><body><h2>BTCPay authorization failed</h2><p>{}</p></body></html>",
                html_escape::encode_text(&e.to_string())
            )),
        )
            .into_response(),
    }
}

/// Admin endpoint: list payment methods configured on the connected
/// BTCPay store. Defaults to the default-profile's BTCPay provider for
/// back-compat with the existing admin UI; the new merchant-profile
/// admin endpoint passes an explicit `provider_id` query param when
/// multiple BTCPay providers exist.
pub async fn payment_methods(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    require_scope(&state, &headers, "payment_providers:read").await?;
    let default = crate::merchant_profiles::require_default(&state.db).await?;
    let rows = crate::db::repo::list_payment_providers_for_profile(&state.db, &default.id)
        .await?;
    let row = rows
        .into_iter()
        .find(|p| p.kind == "btcpay")
        .ok_or(AppError::BtcpayNotConfigured)?;
    let store_id = row.store_id.as_deref().unwrap_or("");
    let methods = btcpay_client::list_payment_methods(&row.base_url, &row.api_key, store_id)
        .await
        .map_err(|e| AppError::Upstream(format!("BTCPay list-payment-methods: {e:#}")))?;
    let count = methods.len();
    Ok(Json(json!({
        "store_id": store_id,
        "count": count,
        "methods": methods,
    })))
}

/// Admin endpoint: report BTCPay connection status for the default
/// profile (back-compat with the existing admin UI's payment-providers
/// card). Multi-profile operators use `/v1/admin/merchant-profiles` to
/// see all attached providers.
pub async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    require_scope(&state, &headers, "payment_providers:read").await?;
    let default = crate::merchant_profiles::get_default(&state.db).await?;
    let row = match &default {
        Some(profile) => {
            let rows = crate::db::repo::list_payment_providers_for_profile(&state.db, &profile.id)
                .await?;
            rows.into_iter().find(|p| p.kind == "btcpay")
        }
        None => None,
    };
    Ok(Json(match row {
        None => json!({ "connected": false }),
        Some(p) => json!({
            "connected": true,
            "provider_id": p.id,
            "store_id": p.store_id,
            "webhook_id": p.webhook_id,
            "base_url": p.base_url,
            "label": p.label,
            "merchant_profile_id": default.as_ref().map(|d| d.id.clone()),
            "merchant_profile_name": default.as_ref().map(|d| d.name.clone()),
        }),
    }))
}

// --- internals ---

async fn finish_connect(state: &AppState, state_token: &str, api_key: &str) -> AppResult<()> {
    // Recovers the `merchant_profile_id` recorded when the operator
    // kicked off the connect flow. NULL falls back to the default
    // profile (back-compat for state tokens from pre-0022 runs).
    let auth_state = btcpay_cfg::consume_authorize_state(&state.db, state_token)
        .await
        .map_err(|_| AppError::Unauthorized)?;
    let profile = match auth_state.merchant_profile_id.as_deref() {
        Some(id) => crate::merchant_profiles::get(&state.db, id)
            .await?
            .ok_or_else(|| AppError::BadRequest(format!(
                "merchant profile {id} no longer exists — the operator may have \
                 deleted it during the authorize round-trip. Reconnect from a \
                 valid profile."
            )))?,
        None => crate::merchant_profiles::require_default(&state.db).await?,
    };

    let base_url = &state.config.btcpay_url;

    // Enumerate stores the key has access to. With `selectiveStores=true`
    // the operator picked specific stores during authorize; we pick the
    // first one that the key can see.
    let stores = btcpay_client::list_stores(base_url, api_key)
        .await
        .map_err(|e| AppError::Upstream(format!("BTCPay list-stores: {e:#}")))?;
    let store = stores
        .into_iter()
        .next()
        .ok_or_else(|| AppError::BadRequest(
            "The authorized API key has access to zero stores. Re-run connect and pick a store.".into()
        ))?;

    // INNER gate (scoped initiators only): the target store must settle on a
    // non-mainnet network. This is the first point in the flow where we know
    // the store, so detection happens here — BEFORE registering any webhook or
    // persisting the provider. Fail closed: if the network can't be positively
    // determined as non-mainnet, treat it as mainnet and refuse. Master
    // initiators skip this entirely (they may connect any network).
    let resolved_network = if auth_state.scoped_initiator {
        let network = match btcpay_client::fetch_onchain_network(base_url, api_key, &store.id).await {
            Ok(Some(net)) => net,
            Ok(None) => {
                tracing::warn!(
                    store = %store.id,
                    "scoped BTCPay connect: on-chain network undetermined → fail-closed to mainnet (deny)"
                );
                BitcoinNetwork::Mainnet
            }
            Err(e) => {
                tracing::warn!(
                    store = %store.id, error = %format!("{e:#}"),
                    "scoped BTCPay connect: network detection errored → fail-closed to mainnet (deny)"
                );
                BitcoinNetwork::Mainnet
            }
        };
        if network.is_mainnet() {
            return Err(AppError::BadRequest(format!(
                "Scoped payment-provider connect is restricted to non-mainnet \
                 (regtest/testnet/signet) BTCPay stores; the selected store resolved \
                 to '{}'. Use the master admin key to connect a mainnet store.",
                network.as_str()
            )));
        }
        Some(network)
    } else {
        None
    };

    // Generate a strong webhook secret, then register the webhook on BTCPay.
    let mut raw_secret = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw_secret);
    let webhook_secret = BASE32_NOPAD.encode(&raw_secret);

    // Pre-generate the provider id so we can bake it into the webhook
    // URL we register with BTCPay. The webhook router routes by this
    // path-param id, isolating deliveries per-provider per-profile.
    let provider_id = uuid::Uuid::new_v4().to_string();
    let callback_url = format!(
        "{}/v1/btcpay/webhook/{}",
        state.config.public_base_url, provider_id
    );

    let created_webhook = btcpay_client::create_webhook(
        base_url,
        api_key,
        &store.id,
        &callback_url,
        &webhook_secret,
    )
    .await
    .map_err(|e| AppError::Upstream(format!("BTCPay create-webhook: {e:#}")))?;

    // Persist as a payment_providers row attached to the chosen profile.
    let label = format!("BTCPay — {}", profile.name);
    let now = chrono::Utc::now().to_rfc3339();
    crate::db::repo::create_payment_provider(
        &state.db,
        &provider_id,
        &profile.id,
        "btcpay",
        &label,
        api_key,
        base_url,
        Some(&created_webhook.id),
        Some(&webhook_secret),
        Some(&store.id),
        &now,
    )
    .await?;

    // If this is the first provider on the default profile, also
    // populate the back-compat singleton so the few remaining
    // state.payment_provider() callers work without a daemon restart.
    let existing = crate::db::repo::list_payment_providers_for_profile(&state.db, &profile.id)
        .await?;
    if profile.is_default && existing.len() == 1 {
        let client = BtcpayClient::new(base_url, api_key, &store.id);
        let provider = Arc::new(
            BtcpayProvider::new(client, webhook_secret.clone())
                .with_public_base(state.config.btcpay_public_url.clone()),
        );
        state.set_payment_provider(provider).await;
    }

    let network_str = resolved_network.map(|n| n.as_str());
    tracing::info!(
        provider_id = %provider_id,
        merchant_profile_id = %profile.id,
        store = %store.id,
        store_name = %store.name,
        webhook_id = %created_webhook.id,
        scoped = auth_state.scoped_initiator,
        network = network_str.unwrap_or("master/any"),
        "BTCPay connected via authorize flow"
    );

    // Audit every scoped connect (spec §7) — attributes the fund-redirection-
    // sensitive op to the initiating credential + the resolved network. Master
    // connects are already covered by the StartOS action audit trail.
    if auth_state.scoped_initiator {
        let _ = crate::db::repo::insert_audit(
            &state.db,
            "scoped_api_key",
            auth_state.initiator_actor_hash.as_deref(),
            "payment_provider.connect_scoped",
            Some("payment_provider"),
            Some(&provider_id),
            None,
            None,
            &json!({
                "kind": "btcpay",
                "store_id": store.id,
                "merchant_profile_id": profile.id,
                "network": network_str,
            }),
        )
        .await;
    }
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

#[derive(Debug, Deserialize, Default)]
pub struct DisconnectReq {
    /// Which provider row to disconnect. NULL = the BTCPay provider on
    /// the default merchant profile (back-compat for the existing admin
    /// UI's single-button Disconnect).
    #[serde(default)]
    pub provider_id: Option<String>,
}

/// Admin endpoint: disconnect a BTCPay provider. Best-effort revocation
/// of the webhook + API key on BTCPay's side, then unconditional delete
/// of the local payment_providers row. If BTCPay is unreachable, the
/// local state is still cleared and the operator gets a warning to
/// clean up BTCPay manually.
pub async fn disconnect(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<DisconnectReq>>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = crate::api::admin::request_context(&headers);
    let req = body.map(|Json(b)| b).unwrap_or_default();

    let provider_row = match req.provider_id.as_deref() {
        Some(pid) => crate::db::repo::get_payment_provider_by_id(&state.db, pid)
            .await?
            .filter(|p| p.kind == "btcpay"),
        None => {
            // Default-profile fallback for the existing admin UI.
            let default = crate::merchant_profiles::require_default(&state.db).await?;
            let rows = crate::db::repo::list_payment_providers_for_profile(&state.db, &default.id)
                .await?;
            rows.into_iter().find(|p| p.kind == "btcpay")
        }
    };
    let Some(provider_row) = provider_row else {
        return Ok(Json(json!({
            "ok": true,
            "noop": true,
            "message": "no BTCPay provider connected on the named profile",
        })));
    };

    let provider_id = provider_row.id.clone();
    let store_id = provider_row.store_id.clone().unwrap_or_default();
    let webhook_id = provider_row.webhook_id.clone();

    // Best-effort remote cleanup. We DON'T short-circuit if either of
    // these calls fails — the operator's intent is to disconnect, and
    // leaving local state pointing at a remote we no longer trust is
    // worse than leaving orphan state on the BTCPay side. Any failures
    // are surfaced in the response so the operator can manually clean
    // up on BTCPay if needed.
    let mut warnings: Vec<String> = Vec::new();
    if let Some(webhook_id) = webhook_id.as_deref() {
        if let Err(e) = btcpay_client::delete_webhook(
            &provider_row.base_url,
            &provider_row.api_key,
            &store_id,
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
    if let Err(e) = btcpay_client::revoke_api_key(&provider_row.base_url, &provider_row.api_key).await {
        warnings.push(format!(
            "Could not revoke BTCPay API key: {e}. \
             You may want to manually revoke it in BTCPay's account API-keys page."
        ));
    }

    crate::db::repo::delete_payment_provider(&state.db, &provider_id).await?;

    // Clear the back-compat singleton if it was holding this one.
    state.clear_payment_provider().await;

    let _ = crate::db::repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "payment_provider.disconnect",
        Some("payment_provider"),
        Some(&provider_id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "kind": "btcpay", "store_id": store_id, "webhook_id": webhook_id }),
    )
    .await;

    Ok(Json(json!({
        "ok": true,
        "noop": false,
        "provider_id": provider_id,
        "store_id": store_id,
        "webhook_id": webhook_id,
        "warnings": warnings,
    })))
}
