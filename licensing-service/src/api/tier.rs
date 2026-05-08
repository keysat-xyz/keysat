//! Tier model + entitlement-cap enforcement.
//!
//! Keysat ships in three tiers. The daemon enforces caps based on the
//! entitlements baked into its own self-license (see `license_self.rs`):
//!
//!   - **Creator** (default, also the unlicensed default): caps at 5
//!     products, 5 policies per product, 5 active discount codes. Buyers
//!     get a real Keysat brand experience for hobbyist scale. Sold at
//!     keysat.xyz for ~21,000 sats; also distributable via free codes.
//!   - **Pro**: unlimited products / policies / codes. Unlocks
//!     `recurring_billing` and `card_payments` (Zaprite) when those
//!     features ship in v0.3. Sold at keysat.xyz for ~250,000 sats / yr.
//!   - **Patron**: same feature surface as Pro, plus a `patron`
//!     entitlement that renders a "Patron" badge in the admin topbar.
//!     Honest upsell — no fake feature gate. Sold for ~500,000 sats / yr.
//!
//! "Unlicensed" (no self-license file present) is treated as Creator-tier
//! caps: operators can install Keysat and start shipping without paying
//! us a sat. The pull to a paid tier happens organically when they need
//! more than 5 products or want recurring billing.
//!
//! All tier judgments are derived from the `entitlements` array on the
//! daemon's self-license. The presence of `unlimited_products` lifts
//! the product cap; `unlimited_policies` lifts the policy-per-product
//! cap; `unlimited_codes` lifts the code cap. `recurring_billing` and
//! `card_payments` gate the Zaprite + recurring features (when those
//! ship). `patron` is purely cosmetic.
//!
//! The cap enforcement returns 402 Payment Required with an `upgrade_url`
//! pointing at the master Keysat's buy page so the admin SPA can render
//! a "Upgrade to Pro" CTA right inside the error.

use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::license_self::Tier;

/// Tier-cap ceilings for the entry-level "Creator" tier (and unlicensed
/// installs, which inherit the same caps). Tunable as we learn more from
/// real operator usage post-launch — change the constants here. Existing
/// operators are never retroactively kicked off; the cap fires at
/// create-time only.
pub const CREATOR_PRODUCT_CAP: i64 = 5;
pub const CREATOR_POLICY_CAP_PER_PRODUCT: i64 = 5;
pub const CREATOR_CODE_CAP: i64 = 5;

/// Where the upgrade banner / 402 error sends an operator to buy a
/// higher tier. Hard-coded to the canonical master Keysat. Eventually
/// this becomes configurable for partners who run their own master
/// Keysat (resellers); for v0.1 it's fixed.
pub const UPGRADE_URL_PRO: &str = "https://licensing.keysat.xyz/buy/keysat?policy=pro";
pub const UPGRADE_URL_PATRON: &str = "https://licensing.keysat.xyz/buy/keysat?policy=patron";

/// Snapshot of the daemon's current entitlements + a coarse tier label
/// for UI consumption.
#[derive(Debug, Clone)]
pub struct TierInfo {
    /// Coarse label: "creator" | "pro" | "patron" | "unlicensed".
    pub label: &'static str,
    /// Display-friendly name: "Creator" | "Pro" | "Patron" | "Unlicensed".
    pub display_name: &'static str,
    /// The full entitlement set baked into the self-license, or empty if unlicensed.
    pub entitlements: Vec<String>,
}

impl TierInfo {
    pub fn has(&self, name: &str) -> bool {
        self.entitlements.iter().any(|e| e == name)
    }
    pub fn is_at_least_pro(&self) -> bool {
        // Anything with unlimited_products is Pro or above. Patron is the
        // top tier; the `patron` entitlement is purely a badge and doesn't
        // grant anything Pro doesn't.
        self.has("unlimited_products")
    }
}

/// Read the daemon's self-tier and project to a TierInfo for tier-aware
/// code paths. Async because state.self_tier is wrapped in a tokio RwLock
/// (allows `Activate Keysat license` to swap it without a daemon restart).
pub async fn current(state: &AppState) -> TierInfo {
    let tier = state.self_tier.read().await;
    let entitlements = match &*tier {
        Tier::Licensed { entitlements, .. } => entitlements.clone(),
        Tier::Unlicensed { .. } => Vec::new(),
    };
    drop(tier);

    let label: &'static str;
    let display_name: &'static str;
    if entitlements.iter().any(|e| e == "patron") {
        label = "patron";
        display_name = "Patron";
    } else if entitlements.iter().any(|e| e == "unlimited_products") {
        label = "pro";
        display_name = "Pro";
    } else if entitlements.iter().any(|e| e == "self_host") {
        label = "creator";
        display_name = "Creator";
    } else {
        label = "unlicensed";
        display_name = "Unlicensed";
    }
    TierInfo {
        label,
        display_name,
        entitlements,
    }
}

/// Admin endpoint: GET /v1/admin/tier — used by the SPA's persistent
/// upgrade banner to know which tier message to show. Returns current
/// tier label, full entitlement list, current usage counts, and the
/// caps that apply (or null for unlimited).
pub async fn admin_status(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: axum::http::HeaderMap,
) -> AppResult<axum::Json<serde_json::Value>> {
    crate::api::admin::require_admin(&state, &headers)?;
    let tier = current(&state).await;
    let product_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM products")
        .fetch_one(&state.db)
        .await?;
    let active_code_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM discount_codes WHERE active = 1")
            .fetch_one(&state.db)
            .await?;
    let serde_json_caps = serde_json::json!({
        "products": if tier.has("unlimited_products") {
            serde_json::Value::Null
        } else {
            serde_json::Value::from(CREATOR_PRODUCT_CAP)
        },
        "policies_per_product": if tier.has("unlimited_policies") {
            serde_json::Value::Null
        } else {
            serde_json::Value::from(CREATOR_POLICY_CAP_PER_PRODUCT)
        },
        "active_codes": if tier.has("unlimited_codes") {
            serde_json::Value::Null
        } else {
            serde_json::Value::from(CREATOR_CODE_CAP)
        },
    });
    let next_tier = match tier.label {
        "creator" | "unlicensed" => "pro",
        "pro" => "patron",
        _ => "patron",
    };
    let upgrade_url = match next_tier {
        "pro" => UPGRADE_URL_PRO,
        _ => UPGRADE_URL_PATRON,
    };
    Ok(axum::Json(serde_json::json!({
        "tier": tier.label,
        "tier_name": tier.display_name,
        "entitlements": tier.entitlements,
        "usage": {
            "products": product_count,
            "active_codes": active_code_count,
        },
        "caps": serde_json_caps,
        "next_tier": if tier.label == "patron" { serde_json::Value::Null } else { serde_json::Value::from(next_tier) },
        "upgrade_url": if tier.label == "patron" { serde_json::Value::Null } else { serde_json::Value::from(upgrade_url) },
    })))
}

/// Refuse a new product if the operator is at the Creator-tier product
/// cap and lacks `unlimited_products`. Counts ALL products including
/// inactive ones — operators don't get to evade the cap by toggling
/// active=false on old rows.
pub async fn enforce_product_cap(state: &AppState) -> AppResult<()> {
    let tier = current(state).await;
    if tier.has("unlimited_products") {
        return Ok(());
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM products")
        .fetch_one(&state.db)
        .await?;
    if count >= CREATOR_PRODUCT_CAP {
        return Err(AppError::PaymentRequired {
            message: format!(
                "Your {} tier allows up to {} products. You're at {}. Upgrade to Pro for unlimited products.",
                tier.display_name, CREATOR_PRODUCT_CAP, count
            ),
            upgrade_url: UPGRADE_URL_PRO.to_string(),
        });
    }
    Ok(())
}

/// Refuse a new policy on `product_id` if the operator is at the
/// Creator-tier per-product policy cap and lacks `unlimited_policies`.
pub async fn enforce_policy_cap(state: &AppState, product_id: &str) -> AppResult<()> {
    let tier = current(state).await;
    if tier.has("unlimited_policies") {
        return Ok(());
    }
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM policies WHERE product_id = ?")
            .bind(product_id)
            .fetch_one(&state.db)
            .await?;
    if count >= CREATOR_POLICY_CAP_PER_PRODUCT {
        return Err(AppError::PaymentRequired {
            message: format!(
                "Your {} tier allows up to {} policies per product. You're at {}. Upgrade to Pro for unlimited.",
                tier.display_name, CREATOR_POLICY_CAP_PER_PRODUCT, count
            ),
            upgrade_url: UPGRADE_URL_PRO.to_string(),
        });
    }
    Ok(())
}

/// Refuse a new discount code if the operator is at the Creator-tier
/// active-codes cap and lacks `unlimited_codes`. Counts only ACTIVE
/// codes — operators can disable old codes to free up slots, which is
/// the right behavior because disabled codes don't function.
pub async fn enforce_code_cap(state: &AppState) -> AppResult<()> {
    let tier = current(state).await;
    if tier.has("unlimited_codes") {
        return Ok(());
    }
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM discount_codes WHERE active = 1")
            .fetch_one(&state.db)
            .await?;
    if count >= CREATOR_CODE_CAP {
        return Err(AppError::PaymentRequired {
            message: format!(
                "Your {} tier allows up to {} active discount codes. You're at {}. Disable an old code to free up a slot, or upgrade to Pro for unlimited.",
                tier.display_name, CREATOR_CODE_CAP, count
            ),
            upgrade_url: UPGRADE_URL_PRO.to_string(),
        });
    }
    Ok(())
}
