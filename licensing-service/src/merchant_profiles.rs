//! Merchant profile layer.
//!
//! A merchant profile represents one "business" the operator is running
//! on a Keysat instance. Owns business identity (brand, support contact,
//! redirect URL, optional SMTP) and a set of payment providers attached
//! to it (BTCPay + Zaprite + future kinds). Products attach to a
//! merchant profile, not directly to a provider.
//!
//! Tier gating:
//! - **Creator (free)**: exactly 1 profile (the auto-created default).
//! - **Pro / Patron**: unlimited profiles.
//!
//! The schema lives in `migrations/0020_merchant_profiles.sql`. Repo
//! helpers (raw SQL) live in `db::repo`; this module wraps them with
//! business-logic guards (tier check, single-default enforcement, etc.).
//!
//! See `plans/multi-provider-payment-model.md` for the design rationale.

use crate::api::AppState;
use crate::db::repo;
use crate::error::{AppError, AppResult};
use anyhow::Context;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use uuid::Uuid;

/// A merchant profile row. Mirrors the `merchant_profiles` table.
///
/// NOTE: the `smtp_*` fields are DORMANT and not consumed by anything.
/// They were laid down in migration 0020 ahead of the keysat-smtp-emails
/// plan, which was SUPERSEDED 2026-06-18: Keysat will never send buyer
/// email itself (operators own that via their own app + the existing
/// webhooks). The columns are left in place because a removal migration
/// isn't worth it — do not build a send path against them. See
/// `plans/keysat-smtp-emails.md` (superseded banner) and the
/// "Operability & alerts" ROADMAP item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MerchantProfile {
    pub id: String,
    pub name: String,
    pub legal_name: Option<String>,
    pub support_url: Option<String>,
    pub support_email: Option<String>,
    pub brand_color: Option<String>,
    pub post_purchase_redirect_url: Option<String>,
    pub is_default: bool,

    // Dormant SMTP-override columns (see struct doc) — stored/returned
    // but never read to send mail; no send path exists or is planned.
    pub smtp_host: Option<String>,
    pub smtp_port: Option<i64>,
    pub smtp_username: Option<String>,
    pub smtp_password: Option<String>,
    pub smtp_from_address: Option<String>,
    pub smtp_from_name: Option<String>,
    pub smtp_use_starttls: bool,

    pub created_at: String,
    pub updated_at: String,
}

/// Input for `create` — only the operator-set fields. id, is_default,
/// created_at, updated_at are filled in by this layer.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewMerchantProfile {
    pub name: String,
    pub legal_name: Option<String>,
    pub support_url: Option<String>,
    pub support_email: Option<String>,
    pub brand_color: Option<String>,
    pub post_purchase_redirect_url: Option<String>,
}

/// Input for `update` — every field optional. None means "leave unchanged."
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MerchantProfileUpdate {
    pub name: Option<String>,
    pub legal_name: Option<Option<String>>,
    pub support_url: Option<Option<String>>,
    pub support_email: Option<Option<String>>,
    pub brand_color: Option<Option<String>>,
    pub post_purchase_redirect_url: Option<Option<String>>,

    pub smtp_host: Option<Option<String>>,
    pub smtp_port: Option<Option<i64>>,
    pub smtp_username: Option<Option<String>>,
    pub smtp_password: Option<Option<String>>,
    pub smtp_from_address: Option<Option<String>>,
    pub smtp_from_name: Option<Option<String>>,
    pub smtp_use_starttls: Option<bool>,
}

/// Look up a profile by id. Returns `Ok(None)` if not found.
pub async fn get(pool: &SqlitePool, id: &str) -> AppResult<Option<MerchantProfile>> {
    repo::get_merchant_profile_by_id(pool, id).await
}

/// Return the default profile. Migration 0020 guarantees exactly one
/// exists post-migration, so this returning None at runtime is an
/// invariant violation and the caller should treat it as fatal.
pub async fn get_default(pool: &SqlitePool) -> AppResult<Option<MerchantProfile>> {
    repo::get_default_merchant_profile(pool).await
}

/// Required default profile lookup. Returns AppError::Internal if no
/// default exists (which would mean the migration was skipped or the
/// row was somehow deleted — neither should happen in normal operation).
pub async fn require_default(pool: &SqlitePool) -> AppResult<MerchantProfile> {
    get_default(pool).await?.ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!(
            "no default merchant profile — migration 0020 may not have run"
        ))
    })
}

/// List all merchant profiles, newest-first.
pub async fn list(pool: &SqlitePool) -> AppResult<Vec<MerchantProfile>> {
    repo::list_merchant_profiles(pool).await
}

/// Look up the merchant profile a product belongs to. Resolves via
/// `products.merchant_profile_id`. Returns the DEFAULT profile if the
/// product has no profile id set (back-compat for any rows that slipped
/// through the migration with NULL — shouldn't happen but defensive).
pub async fn for_product(state: &AppState, product_id: &str) -> AppResult<MerchantProfile> {
    if let Some(p) = repo::get_merchant_profile_for_product(&state.db, product_id).await? {
        return Ok(p);
    }
    require_default(&state.db).await
}

/// Create a new merchant profile. Enforces the Creator tier cap: if the
/// operator's current tier returns a `merchant_profile` cap of 1 and
/// at least one profile already exists, returns `AppError::TierCap`
/// pointing at the upgrade URL.
///
/// New profiles default to `is_default = 0`. Use `set_default` to flip
/// the default flag explicitly — the auto-created post-migration profile
/// is always the default; subsequent profiles never become default by
/// creation alone.
pub async fn create(
    state: &AppState,
    input: NewMerchantProfile,
) -> AppResult<MerchantProfile> {
    // Tier gate: Creator gets 1 profile (the auto-created default).
    // Pro / Patron with `unlimited_merchant_profiles` get N. Returns
    // AppError::PaymentRequired (HTTP 402) with the upgrade URL so the
    // admin UI can render the existing tier-cap modal.
    crate::api::tier::enforce_merchant_profile_cap(state).await?;

    if input.name.trim().is_empty() {
        return Err(AppError::BadRequest("merchant profile name required".into()));
    }

    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    repo::create_merchant_profile(
        &state.db,
        &id,
        &input.name,
        input.legal_name.as_deref(),
        input.support_url.as_deref(),
        input.support_email.as_deref(),
        input.brand_color.as_deref(),
        input.post_purchase_redirect_url.as_deref(),
        false, // is_default
        &now,
    )
    .await?;
    get(&state.db, &id)
        .await?
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("created profile not found")))
}

/// Update a profile. Only fields with `Some(...)` are written;
/// double-Option wraps nullable fields so callers can distinguish
/// "leave unchanged" (`None`) from "set to NULL" (`Some(None)`).
pub async fn update(
    pool: &SqlitePool,
    id: &str,
    patch: MerchantProfileUpdate,
) -> AppResult<MerchantProfile> {
    repo::update_merchant_profile(pool, id, &patch).await?;
    get(pool, id)
        .await?
        .ok_or_else(|| AppError::BadRequest(format!("merchant profile {id} not found")))
}

/// Flip a profile to be the default. Atomic: clears the previous
/// default in the same transaction so the partial unique index holds.
pub async fn set_default(pool: &SqlitePool, id: &str) -> AppResult<()> {
    repo::set_default_merchant_profile(pool, id).await
}

/// Delete a profile. Refuses if any product OR active subscription
/// is still attached. Refuses if it's the default profile (operator
/// must set another profile as default first).
pub async fn delete(pool: &SqlitePool, id: &str) -> AppResult<()> {
    let profile = get(pool, id).await?.ok_or_else(|| {
        AppError::BadRequest(format!("merchant profile {id} not found"))
    })?;
    if profile.is_default {
        return Err(AppError::BadRequest(
            "cannot delete the default merchant profile — set another profile as default first"
                .into(),
        ));
    }
    let product_count = repo::count_products_for_profile(pool, id)
        .await
        .context("count_products_for_profile")
        .map_err(AppError::Internal)?;
    if product_count > 0 {
        return Err(AppError::BadRequest(format!(
            "cannot delete merchant profile: {product_count} products still attached. \
             Move or delete the products first."
        )));
    }
    let active_sub_count = repo::count_active_subscriptions_for_profile(pool, id)
        .await
        .context("count_active_subscriptions_for_profile")
        .map_err(AppError::Internal)?;
    if active_sub_count > 0 {
        return Err(AppError::BadRequest(format!(
            "cannot delete merchant profile: {active_sub_count} active subscriptions \
             still attached. Cancel them first or migrate them to another profile."
        )));
    }
    repo::delete_merchant_profile(pool, id).await
}
