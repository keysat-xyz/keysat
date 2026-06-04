//! Payment-provider status endpoint (multi-merchant-profile model).
//!
//! Pre-:52 this module held two endpoints:
//!   - `GET /v1/admin/payment-provider/status` — which provider was
//!     active, plus configured flags for BTCPay + Zaprite.
//!   - `POST /v1/admin/payment-provider/activate` — flip the singleton
//!     active-provider preference between two configured ones.
//!
//! Both became meaningless in the merchant-profile model — providers
//! aren't "active," they attach to profiles, and products pick a profile
//! at the resolution layer. The activate endpoint is removed. The status
//! endpoint stays as a back-compat shim so the existing admin UI's
//! payment-providers card keeps rendering until the new Merchant
//! Profiles UI replaces it: it now reports against the DEFAULT profile
//! (single-profile operators see no change). Multi-profile operators
//! should use the new `/v1/admin/merchant-profiles` endpoints to see
//! all providers across all profiles.

use crate::api::admin::require_admin;
use crate::api::AppState;
use crate::error::AppResult;
use axum::{extract::State, http::HeaderMap, Json};
use serde_json::{json, Value};

/// `GET /v1/admin/payment-provider/status` — back-compat snapshot of
/// providers attached to the default merchant profile. Returns the same
/// shape as pre-:52 with `btcpay_configured` / `zaprite_configured` /
/// `active` for compatibility with the existing admin UI; new code
/// should use `/v1/admin/merchant-profiles/{id}` instead.
pub async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let default = crate::merchant_profiles::get_default(&state.db).await?;
    let providers = match &default {
        Some(p) => crate::db::repo::list_payment_providers_for_profile(&state.db, &p.id).await?,
        None => Vec::new(),
    };
    let btcpay_row = providers.iter().find(|p| p.kind == "btcpay").cloned();
    let zaprite_row = providers.iter().find(|p| p.kind == "zaprite").cloned();
    // "active" used to mean "the singleton active-provider preference."
    // In the new model there isn't one. For back-compat we report the
    // FIRST provider on the default profile (which is what the legacy
    // boot-loader semantics would have picked) so the existing admin UI
    // shows a sensible active badge. Multi-rail operators get the full
    // picture from the new merchant-profile endpoints.
    let active_runtime = providers.first().map(|p| p.kind.clone());
    Ok(Json(json!({
        "btcpay_configured": btcpay_row.is_some(),
        "zaprite_configured": zaprite_row.is_some(),
        "preferred": active_runtime.clone(),
        "active": active_runtime,
        "merchant_profile_id": default.as_ref().map(|p| p.id.clone()),
        "merchant_profile_name": default.as_ref().map(|p| p.name.clone()),
        "providers": providers.iter().map(|p| json!({
            "id": p.id,
            "kind": p.kind,
            "label": p.label,
            "base_url": p.base_url,
            "store_id": p.store_id,
        })).collect::<Vec<_>>(),
    })))
}
