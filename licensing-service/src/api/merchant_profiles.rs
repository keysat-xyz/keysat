//! Admin CRUD endpoints for merchant profiles + rail preferences.
//!
//! Thin Axum handlers wrapping the business-logic helpers in
//! `crate::merchant_profiles` and the rail-preference repo helpers.
//! Consumed by the new Merchant Profiles section of the admin UI.

use crate::api::admin::{request_context, require_admin};
use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::merchant_profiles::{
    self, MerchantProfile, MerchantProfileUpdate, NewMerchantProfile,
};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

fn profile_to_json(p: &MerchantProfile, with_providers: Option<&[crate::db::repo::PaymentProviderRow]>) -> Value {
    let mut obj = json!({
        "id": p.id,
        "name": p.name,
        "legal_name": p.legal_name,
        "support_url": p.support_url,
        "support_email": p.support_email,
        "brand_color": p.brand_color,
        "post_purchase_redirect_url": p.post_purchase_redirect_url,
        "is_default": p.is_default,
        // SMTP credentials are redacted in list/get responses — operators
        // see whether they're set, not the password itself. The edit
        // form submits new credentials only when the operator explicitly
        // wants to rotate them.
        "smtp_configured": p.smtp_host.is_some(),
        "smtp_host": p.smtp_host,
        "smtp_port": p.smtp_port,
        "smtp_username": p.smtp_username,
        "smtp_from_address": p.smtp_from_address,
        "smtp_from_name": p.smtp_from_name,
        "smtp_use_starttls": p.smtp_use_starttls,
        "created_at": p.created_at,
        "updated_at": p.updated_at,
    });
    if let Some(providers) = with_providers {
        let arr: Vec<Value> = providers
            .iter()
            .map(|row| {
                let rails: Vec<&'static str> = crate::payment::ProviderKind::parse(&row.kind)
                    .map(|kind| {
                        crate::payment::rails_for_kind(kind)
                            .into_iter()
                            .map(|r| r.as_str())
                            .collect()
                    })
                    .unwrap_or_default();
                json!({
                    "id": row.id,
                    "kind": row.kind,
                    "label": row.label,
                    "base_url": row.base_url,
                    "store_id": row.store_id,
                    "webhook_id": row.webhook_id,
                    "connected_at": row.connected_at,
                    "served_rails": rails,
                })
            })
            .collect();
        obj["providers"] = json!(arr);
    }
    obj
}

/// `GET /v1/admin/merchant-profiles` — list every profile + a brief
/// summary of attached providers per profile.
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let profiles = merchant_profiles::list(&state.db).await?;
    let mut out: Vec<Value> = Vec::with_capacity(profiles.len());
    for p in &profiles {
        let providers = crate::db::repo::list_payment_providers_for_profile(&state.db, &p.id).await?;
        out.push(profile_to_json(p, Some(&providers)));
    }
    Ok(Json(json!({ "profiles": out })))
}

/// `GET /v1/admin/merchant-profiles/:id` — full detail for a profile,
/// including providers + rail preferences.
pub async fn get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    require_admin(&state, &headers)?;
    let profile = merchant_profiles::get(&state.db, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("merchant profile {id}")))?;
    let providers = crate::db::repo::list_payment_providers_for_profile(&state.db, &id).await?;
    let rail_prefs = crate::db::repo::list_rail_preferences_for_profile(&state.db, &id).await?;
    let mut obj = profile_to_json(&profile, Some(&providers));
    obj["rail_preferences"] = json!(rail_prefs
        .into_iter()
        .map(|p| json!({ "rail": p.rail, "payment_provider_id": p.payment_provider_id }))
        .collect::<Vec<_>>());
    let product_count =
        crate::db::repo::count_products_for_profile(&state.db, &id)
            .await
            .map_err(AppError::Internal)?;
    let active_subscription_count =
        crate::db::repo::count_active_subscriptions_for_profile(&state.db, &id)
            .await
            .map_err(AppError::Internal)?;
    obj["product_count"] = json!(product_count);
    obj["active_subscription_count"] = json!(active_subscription_count);
    Ok(Json(obj))
}

/// `POST /v1/admin/merchant-profiles` — create a new profile.
/// Tier-gated: Creator hits cap on the second profile.
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<NewMerchantProfile>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    let created = merchant_profiles::create(&state, req).await?;
    let _ = crate::db::repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "merchant_profile.create",
        Some("merchant_profile"),
        Some(&created.id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "name": created.name }),
    )
    .await;
    Ok(Json(profile_to_json(&created, None)))
}

/// `PATCH /v1/admin/merchant-profiles/:id` — partial update.
pub async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(patch): Json<MerchantProfileUpdate>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    let updated = merchant_profiles::update(&state.db, &id, patch).await?;
    let _ = crate::db::repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "merchant_profile.update",
        Some("merchant_profile"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "name": updated.name }),
    )
    .await;
    Ok(Json(profile_to_json(&updated, None)))
}

/// `DELETE /v1/admin/merchant-profiles/:id` — delete a non-default
/// profile with no attached products or active subscriptions.
pub async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    merchant_profiles::delete(&state.db, &id).await?;
    let _ = crate::db::repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "merchant_profile.delete",
        Some("merchant_profile"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({}),
    )
    .await;
    Ok(Json(json!({ "ok": true, "id": id })))
}

/// `POST /v1/admin/merchant-profiles/:id/set-default` — flip the
/// default-profile flag to this id.
pub async fn set_default(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);
    merchant_profiles::set_default(&state.db, &id).await?;
    let _ = crate::db::repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "merchant_profile.set_default",
        Some("merchant_profile"),
        Some(&id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({}),
    )
    .await;
    Ok(Json(json!({ "ok": true, "id": id })))
}

// ---------------------------------------------------------------------
// Rail preferences
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SetRailPreferenceReq {
    pub payment_provider_id: String,
}

/// `PUT /v1/admin/merchant-profiles/:id/rail-preferences/:rail` —
/// pin the provider that should serve this rail on this profile.
/// Validates that the provider belongs to the profile AND serves
/// the requested rail before persisting.
pub async fn set_rail_preference(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((profile_id, rail)): Path<(String, String)>,
    Json(req): Json<SetRailPreferenceReq>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);

    // Validate the rail name.
    let parsed_rail = crate::payment::Rail::parse(&rail).ok_or_else(|| {
        AppError::BadRequest(format!(
            "unknown rail '{rail}'; accepted: lightning, onchain, card"
        ))
    })?;

    // Validate the provider exists, belongs to THIS profile, and serves
    // THIS rail.
    let provider_row = crate::db::repo::get_payment_provider_by_id(
        &state.db,
        &req.payment_provider_id,
    )
    .await?
    .ok_or_else(|| {
        AppError::BadRequest(format!("payment provider {} not found", req.payment_provider_id))
    })?;
    if provider_row.merchant_profile_id != profile_id {
        return Err(AppError::BadRequest(format!(
            "payment provider {} is not attached to merchant profile {profile_id}",
            req.payment_provider_id
        )));
    }
    let served = crate::payment::ProviderKind::parse(&provider_row.kind)
        .map(crate::payment::rails_for_kind)
        .unwrap_or_default();
    if !served.contains(&parsed_rail) {
        return Err(AppError::BadRequest(format!(
            "payment provider {} (kind={}) does not serve the '{rail}' rail; \
             pick a provider that does, or remove this preference",
            req.payment_provider_id, provider_row.kind
        )));
    }

    crate::db::repo::set_rail_preference(
        &state.db,
        &profile_id,
        parsed_rail.as_str(),
        &req.payment_provider_id,
    )
    .await?;

    let _ = crate::db::repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "merchant_profile.rail_preference.set",
        Some("merchant_profile"),
        Some(&profile_id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "rail": parsed_rail.as_str(), "payment_provider_id": req.payment_provider_id }),
    )
    .await;

    Ok(Json(json!({
        "ok": true,
        "merchant_profile_id": profile_id,
        "rail": parsed_rail.as_str(),
        "payment_provider_id": req.payment_provider_id,
    })))
}

/// `DELETE /v1/admin/merchant-profiles/:id/rail-preferences/:rail` —
/// clear a rail preference, letting the deterministic-earliest-connected
/// fallback take over.
pub async fn clear_rail_preference(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((profile_id, rail)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    let actor_hash = require_admin(&state, &headers)?;
    let (ip, ua) = request_context(&headers);

    let parsed_rail = crate::payment::Rail::parse(&rail).ok_or_else(|| {
        AppError::BadRequest(format!(
            "unknown rail '{rail}'; accepted: lightning, onchain, card"
        ))
    })?;
    crate::db::repo::clear_rail_preference(&state.db, &profile_id, parsed_rail.as_str()).await?;

    let _ = crate::db::repo::insert_audit(
        &state.db,
        "admin_api_key",
        Some(&actor_hash),
        "merchant_profile.rail_preference.clear",
        Some("merchant_profile"),
        Some(&profile_id),
        ip.as_deref(),
        ua.as_deref(),
        &json!({ "rail": parsed_rail.as_str() }),
    )
    .await;
    Ok(Json(json!({
        "ok": true,
        "merchant_profile_id": profile_id,
        "rail": parsed_rail.as_str(),
    })))
}
