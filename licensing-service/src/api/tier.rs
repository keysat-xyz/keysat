//! Tier model + entitlement-cap enforcement.
//!
//! Keysat ships in three tiers. The daemon enforces caps based on the
//! entitlements baked into its own self-license (see `license_self.rs`):
//!
//!   - **Creator** (free, no self-license required): caps at 5 products,
//!     5 policies per product, 10 active discount codes. Buyers get a
//!     real Keysat brand experience for hobbyist scale. Anyone who
//!     installs Keysat is on Creator out of the box — no signup, no
//!     trial.
//!   - **Pro**: unlimited products / policies / codes. Unlocks
//!     `recurring_billing` and `zaprite_payments` (Zaprite gateway —
//!     cards, Apple Pay, bank transfers, in addition to Bitcoin). Sold
//!     at keysat.xyz for ~250,000 sats / yr.
//!   - **Patron**: same feature surface as Pro, plus a `patron`
//!     entitlement that renders a "Patron" badge in the admin topbar.
//!     Honest upsell — no fake feature gate. Sold for ~500,000 sats / yr.
//!
//! The pull from Creator to a paid tier happens organically: operators
//! hit the 5-product cap, or want recurring billing, or want to accept
//! cards via Zaprite. All three trigger a 402 with an upgrade URL.
//!
//! All tier judgments are derived from the `entitlements` array on the
//! daemon's self-license. The presence of `unlimited_products` lifts
//! the product cap; `unlimited_policies` lifts the policy-per-product
//! cap; `unlimited_codes` lifts the code cap. `recurring_billing` gates
//! creating recurring policies; `zaprite_payments` gates Connect/Activate
//! Zaprite. `patron` is purely cosmetic.
//!
//! The cap enforcement returns 402 Payment Required with an `upgrade_url`
//! pointing at the master Keysat's buy page so the admin SPA can render
//! a "Upgrade to Pro" CTA right inside the error.

use crate::api::AppState;
use crate::error::{AppError, AppResult};
use crate::license_self::Tier;

/// Tier-cap ceilings for the entry-level "Creator" tier — the default
/// state when no self-license is present and the surfaced label whenever
/// a license's entitlements don't include `unlimited_products`. Tunable
/// as we learn more from real operator usage post-launch — change the
/// constants here. Existing operators are never retroactively kicked
/// off; the cap fires at create-time only.
pub const CREATOR_PRODUCT_CAP: i64 = 5;
pub const CREATOR_POLICY_CAP_PER_PRODUCT: i64 = 5;
/// Creator-tier active-discount-code cap. Sized so a launch operator
/// can run several concurrent promo campaigns (launch week, early bird,
/// newsletter, speaker codes, etc.) without conversion-pressure that
/// doesn't actually map to scale. Disabled codes don't count.
pub const CREATOR_CODE_CAP: i64 = 10;

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
    /// Coarse label: "creator" | "pro" | "patron".
    pub label: &'static str,
    /// Display-friendly name: "Creator" | "Pro" | "Patron".
    pub display_name: &'static str,
    /// The full entitlement set baked into the self-license; empty for Creator.
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
///
/// A missing self-license surfaces as Creator (the free tier) — the daemon
/// always boots, the Creator caps apply, and the admin UI shows "Creator"
/// rather than "Unlicensed" to avoid the implication that something needs
/// to be fixed.
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
    } else {
        // No paid entitlements present (or no self-license at all) → Creator.
        label = "creator";
        display_name = "Creator";
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
        "creator" => "pro",
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

/// Refuse a new merchant profile if the operator is at the Creator-tier
/// merchant-profile cap (= 1) and lacks `unlimited_merchant_profiles`.
/// Counts every profile including the auto-created default. So Creator
/// operators have the default profile (auto-created by migration 0020)
/// and can't add more; Pro and Patron operators are unlimited.
///
/// The `unlimited_merchant_profiles` entitlement needs to be added to
/// the master Keysat's Pro and Patron policies as a separate admin
/// action — see plans/multi-provider-payment-model.md "Tier gating"
/// section.
pub async fn enforce_merchant_profile_cap(state: &AppState) -> AppResult<()> {
    let tier = current(state).await;
    if tier.has("unlimited_merchant_profiles") {
        return Ok(());
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM merchant_profiles")
        .fetch_one(&state.db)
        .await?;
    // Creator gets 1 (the default profile).
    if count >= 1 {
        return Err(AppError::PaymentRequired {
            message: format!(
                "Your {} tier allows a single merchant profile (the default). \
                 You're at {}. Upgrade to Pro to run multiple businesses \
                 from one Keysat instance.",
                tier.display_name, count
            ),
            upgrade_url: UPGRADE_URL_PRO.to_string(),
        });
    }
    Ok(())
}

/// Refuse to mark a policy as recurring unless the operator's self-tier
/// carries the `recurring_billing` entitlement. Pro and Patron tiers
/// have it; Creator does not. Called from both create-policy and
/// update-policy paths so operators can't sneak past by patching a
/// non-recurring policy to recurring after creation.
pub async fn enforce_recurring_feature(state: &AppState) -> AppResult<()> {
    let tier = current(state).await;
    if tier.has("recurring_billing") {
        return Ok(());
    }
    Err(AppError::PaymentRequired {
        message: format!(
            "Recurring subscriptions require Pro or Patron. You're on {}. \
             Upgrade to enable monthly/annual billing.",
            tier.display_name
        ),
        upgrade_url: UPGRADE_URL_PRO.to_string(),
    })
}

/// Refuse to connect or activate Zaprite unless the operator's self-tier
/// carries the `zaprite_payments` entitlement. Pro and Patron tiers have
/// it; Creator does not. Zaprite is the buyer-side optionality story —
/// cards, Apple Pay, bank transfers, plus Bitcoin — so this gate is the
/// upgrade pressure for operators who want to accept payment methods
/// beyond Bitcoin / Lightning via BTCPay. Called from both the initial
/// Connect Zaprite flow and the Activate-Zaprite switch, so an operator
/// can't sneak past by connecting on Pro and downgrading later (the
/// downgrade flow doesn't auto-disconnect Zaprite, but a switch attempt
/// after downgrade is refused).
pub async fn enforce_zaprite_feature(state: &AppState) -> AppResult<()> {
    let tier = current(state).await;
    if tier.has("zaprite_payments") {
        return Ok(());
    }
    Err(AppError::PaymentRequired {
        message: format!(
            "Zaprite payment gateway (cards, Apple Pay, bank transfers, and more) \
             requires Pro or Patron. You're on {}. BTCPay (Bitcoin / Lightning) \
             remains available on every tier.",
            tier.display_name
        ),
        upgrade_url: UPGRADE_URL_PRO.to_string(),
    })
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
