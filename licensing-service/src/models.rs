//! Domain models — shared types used by DB, API, and BTCPay layers.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Product {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub description: String,
    /// Sat-denominated price. For SAT-currency products this equals
    /// `price_value`. For fiat-priced products (USD, EUR, etc.) this
    /// is a snapshot from the most recent invoice creation against
    /// the product, kept for back-compat with v0.1 SDKs and admin UI
    /// that haven't migrated to the typed currency view yet. The
    /// canonical price is `price_currency` + `price_value`.
    pub price_sats: i64,
    /// Operator-facing currency: 'SAT', 'BTC', 'USD', 'EUR' (and
    /// future ISO 4217 codes). Defaults to 'SAT' for products
    /// created before v0.1.0:48 / migration 0010.
    #[serde(default = "default_currency")]
    pub price_currency: String,
    /// Price in the smallest indivisible unit of `price_currency`:
    /// sats for SAT/BTC, cents for USD/EUR.
    #[serde(default)]
    pub price_value: i64,
    pub active: bool,
    /// Arbitrary JSON metadata the developer can attach.
    pub metadata: serde_json::Value,
    /// Per-product entitlements catalog (migration 0014). Defines the
    /// closed list of entitlement slugs the product offers, with
    /// human-readable display names + descriptions used by the buy
    /// page and SDK consumers. None = "free-text mode" (legacy
    /// behavior); operators can opt-in by adding rows.
    #[serde(default)]
    pub entitlements_catalog: Option<Vec<EntitlementDef>>,
    pub created_at: String,
    pub updated_at: String,
}

/// One entry in a product's entitlements catalog. Operator defines
/// these once per product; policies reference them by slug.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntitlementDef {
    /// Stable identifier — what gets baked into the signed license
    /// payload + checked by the SDK's `hasEntitlement(slug)` calls.
    /// Must be ASCII, lowercase, no spaces (operator's responsibility).
    pub slug: String,
    /// Human-readable label rendered on the buy page tier cards
    /// (e.g. "AI summaries"). Falls back to the slug if empty.
    pub name: String,
    /// Optional one-sentence description shown as a tooltip / sub-line
    /// on the buy page. Empty when operator hasn't filled it in.
    #[serde(default)]
    pub description: String,
}

fn default_currency() -> String {
    "SAT".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InvoiceStatus {
    Pending,
    Settled,
    Expired,
    Invalid,
}

impl InvoiceStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            InvoiceStatus::Pending => "pending",
            InvoiceStatus::Settled => "settled",
            InvoiceStatus::Expired => "expired",
            InvoiceStatus::Invalid => "invalid",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "settled" => InvoiceStatus::Settled,
            "expired" => InvoiceStatus::Expired,
            "invalid" => InvoiceStatus::Invalid,
            _ => InvoiceStatus::Pending,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invoice {
    pub id: String,
    pub btcpay_invoice_id: String,
    pub product_id: String,
    pub status: String,
    pub buyer_email: Option<String>,
    pub buyer_note: Option<String>,
    pub amount_sats: i64,
    pub checkout_url: String,
    pub created_at: String,
    pub updated_at: String,
    /// Policy chosen by the buyer at purchase time. NULL on pre-:27 invoices,
    /// in which case `issue_license_for_invoice` falls back to picking the
    /// product's default policy. Migration 0007 adds the column.
    #[serde(default)]
    pub policy_id: Option<String>,
    /// Listed currency the invoice was priced in. NULL on pre-multi-currency
    /// invoices (migration 0010+); fall back to "SAT" in that case.
    #[serde(default)]
    pub listed_currency: Option<String>,
    /// Price in the listed currency's smallest unit (sats for SAT, cents
    /// for USD/EUR). NULL on pre-multi-currency invoices; fall back to
    /// `amount_sats` (which is correct for SAT-priced products).
    #[serde(default)]
    pub listed_value: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LicenseStatus {
    Active,
    Revoked,
    /// Temporarily disabled but recoverable — distinct from revocation, which
    /// is terminal. Suspended licenses fail `/v1/validate` with reason
    /// `suspended` until an admin un-suspends them.
    Suspended,
}

impl LicenseStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            LicenseStatus::Active => "active",
            LicenseStatus::Revoked => "revoked",
            LicenseStatus::Suspended => "suspended",
        }
    }
}

/// Full license row. Older fields are unchanged; v2 columns live behind
/// `Option`s since they were introduced in migration 0003.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct License {
    pub id: String,
    pub product_id: String,
    pub invoice_id: Option<String>,
    pub status: String,
    pub fingerprint: Option<String>,
    pub bound_identity: Option<String>,
    pub issued_at: String,
    pub revoked_at: Option<String>,
    pub revocation_reason: Option<String>,
    pub metadata: serde_json::Value,

    // v2 / migration 0003 fields
    pub policy_id: Option<String>,
    pub expires_at: Option<String>,
    pub grace_seconds: i64,
    pub max_machines: i64,
    pub suspended_at: Option<String>,
    pub suspension_reason: Option<String>,
    pub entitlements: Vec<String>,
    pub is_trial: bool,
    pub nostr_npub: Option<String>,
    pub buyer_email: Option<String>,
}

/// Reusable license template. A policy says "when we issue a license under
/// this slug, set these defaults" (duration, grace, entitlements, machine
/// cap, trial flag, price override, optional tip recipient).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    pub id: String,
    pub product_id: String,
    pub name: String,
    pub slug: String,
    pub duration_seconds: i64,
    pub grace_seconds: i64,
    pub max_machines: i64,
    pub is_trial: bool,
    pub price_sats_override: Option<i64>,
    pub entitlements: Vec<String>,
    pub metadata: serde_json::Value,
    pub active: bool,
    /// Lightning Address (user@domain) the daemon tips a percentage of
    /// each successful issuance to. None = no tipping. The amount is
    /// `license_price_sats * tip_pct_bps / 10000`. Tip failures never
    /// block license issuance.
    pub tip_recipient: Option<String>,
    /// Percentage in basis points (1bps = 0.01%; 100bps = 1%; 10000bps = 100%).
    /// 0 = no tipping. Capped at 10000 server-side.
    pub tip_pct_bps: i64,
    /// Free-form label for the tip recipient — surfaced in the audit log.
    pub tip_label: Option<String>,
    /// When true, the policy is rendered on /buy/<product-slug> as a
    /// selectable tier card. Operators can mark "Comp / press" or
    /// "Internal team seat" policies as private to keep them off the
    /// public buy page while still issuing them via admin tooling.
    /// Defaults to true; migration 0007 adds this column.
    #[serde(default = "default_true")]
    pub public: bool,
    /// Recurring subscription cadence (migration 0011). When `is_recurring`
    /// is true, the renewal worker will create a fresh invoice every
    /// `renewal_period_days` and the buy page renders the price as
    /// "every N days" / "monthly".
    #[serde(default)]
    pub is_recurring: bool,
    /// Days between renewal cycles. Ignored when `is_recurring = false`.
    /// Common values: 30 (monthly), 365 (annual).
    #[serde(default)]
    pub renewal_period_days: i64,
    /// Days the subscription stays in `past_due` before transitioning to
    /// `lapsed`. Migration default is 7.
    #[serde(default = "default_grace_period_days")]
    pub grace_period_days: i64,
    /// Free-trial length at first cycle. 0 = no trial. The first invoice
    /// is still issued (for $0 / 1-sat) so the buyer email + license
    /// flow is consistent; renewal worker charges the real price after
    /// `trial_days`.
    #[serde(default)]
    pub trial_days: i64,
    /// Operator-defined ladder ordering for in-place tier upgrades
    /// (migration 0013). Higher rank = better tier. Per-product space:
    /// "free" → 0, "standard" → 1, "pro" → 2, "patron" → 3 etc.
    /// `None` means the policy isn't part of any ladder — buyer-facing
    /// upgrade endpoints reject changes that touch a NULL-rank policy
    /// on either side. Admin endpoints can force-change to/from any
    /// policy. See TIER_UPGRADES_DESIGN.md for the full semantics.
    #[serde(default)]
    pub tier_rank: Option<i64>,
    /// Soft-archive timestamp (migration 0015). `None` = live. `Some(ts)` =
    /// archived: hidden from admin grid by default, hidden from /buy/<slug>,
    /// renewal worker refuses to renew. Existing licenses keep validating
    /// regardless (entitlements are signed into the key).
    #[serde(default)]
    pub archived_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn default_grace_period_days() -> i64 { 7 }

fn default_true() -> bool { true }

/// A machine activated under a license. One row per active install.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Machine {
    pub id: String,
    pub license_id: String,
    pub fingerprint: String,
    pub fingerprint_hash: String,
    pub hostname: Option<String>,
    pub platform: Option<String>,
    pub ip_last_seen: Option<String>,
    pub activated_at: String,
    pub last_heartbeat_at: Option<String>,
    pub deactivated_at: Option<String>,
    pub deactivation_reason: Option<String>,
}

impl Machine {
    pub fn is_active(&self) -> bool {
        self.deactivated_at.is_none()
    }
}

/// Outbound webhook subscription.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookEndpoint {
    pub id: String,
    pub url: String,
    /// HMAC-SHA256 secret — never returned on list endpoints after creation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    pub event_types: Vec<String>,
    pub active: bool,
    pub description: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookDelivery {
    pub id: String,
    pub endpoint_id: String,
    pub event_type: String,
    pub payload_json: String,
    pub attempt_count: i64,
    pub next_attempt_at: Option<String>,
    pub last_status_code: Option<i64>,
    pub last_error: Option<String>,
    pub delivered_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: i64,
    pub actor_kind: String,
    pub actor_hash: Option<String>,
    pub action: String,
    pub target_kind: Option<String>,
    pub target_id: Option<String>,
    pub request_ip: Option<String>,
    pub user_agent: Option<String>,
    pub details: serde_json::Value,
    pub occurred_at: String,
}

/// Discount / referral code. See `migrations/0004_discount_codes.sql`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscountCode {
    pub id: String,
    pub code: String,
    /// 'percent' | 'fixed_sats'.
    pub kind: String,
    /// Basis points if `kind == 'percent'` (0..=10000); sats if `kind == 'fixed_sats'`.
    pub amount: i64,
    pub max_uses: Option<i64>,
    pub used_count: i64,
    pub expires_at: Option<String>,
    pub applies_to_product_id: Option<String>,
    pub applies_to_policy_id: Option<String>,
    /// Multi-policy scope (migration 0018). When non-empty, the code
    /// applies only to policies in this list — the legacy singular
    /// `applies_to_policy_id` is ignored. When empty, behavior falls
    /// back to the singular column.
    #[serde(default)]
    pub applies_to_policy_ids: Vec<String>,
    pub referrer_label: Option<String>,
    pub description: String,
    pub active: bool,
    /// When `true`, the buy page renders this code as a public "launch
    /// special" — striking the original price, showing the discounted
    /// price, with a "LAUNCH SPECIAL" diagonal ribbon. The purchase
    /// endpoint auto-applies it for buyers who don't type any code.
    /// Operator-typed codes still win if the buyer manually enters one.
    #[serde(default)]
    pub featured: bool,
    pub created_at: String,
    pub updated_at: String,
}

impl DiscountCode {
    /// Effective allowed-policy set. Empty = no policy restriction (the
    /// code applies to any policy in the product/global scope). Non-
    /// empty = the buyer's chosen policy id must be in this list.
    ///
    /// Multi-policy column (0018) takes precedence over the legacy
    /// singular column; if both are absent the result is empty.
    pub fn allowed_policy_ids(&self) -> Vec<&str> {
        if !self.applies_to_policy_ids.is_empty() {
            self.applies_to_policy_ids
                .iter()
                .map(|s| s.as_str())
                .collect()
        } else if let Some(pid) = self.applies_to_policy_id.as_deref() {
            vec![pid]
        } else {
            Vec::new()
        }
    }
}

/// One row per (code, invoice) pair. Status transitions:
///   pending → redeemed   (invoice settled, license issued)
///   pending → cancelled  (invoice expired or invalidated)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscountRedemption {
    pub id: String,
    pub code_id: String,
    pub invoice_id: String,
    pub license_id: Option<String>,
    /// 'pending' | 'redeemed' | 'cancelled'.
    pub status: String,
    pub discount_applied_sats: i64,
    pub base_price_sats: i64,
    pub final_price_sats: i64,
    pub created_at: String,
    pub updated_at: String,
}
