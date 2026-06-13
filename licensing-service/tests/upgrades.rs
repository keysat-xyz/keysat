//! Integration tests for tier upgrades — the quote logic + apply
//! step that lives in `src/upgrades.rs`. Phase 2 of
//! TIER_UPGRADES_DESIGN.md. No HTTP layer yet (Phase 3); these
//! tests exercise the pure module API.

use anyhow::Result;
use chrono::Utc;
use keysat::api::AppState;
use keysat::config::Config;
use keysat::db::repo;
use keysat::license_self::Tier;
use keysat::upgrades::{
    apply_tier_change, compute_upgrade_quote, list_tier_changes_for_license,
    record_tier_change, EffectiveAt, QuoteMode, TierDirection,
};
use serde_json::json;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::sync::RwLock;
use uuid::Uuid;

const TEST_ADMIN_KEY: &str = "test_admin_api_key_with_at_least_32_chars_present";

async fn make_state() -> (AppState, NamedTempFile) {
    let tmp = NamedTempFile::new().expect("tempfile");
    let url = format!("sqlite://{}", tmp.path().display());
    let opts = SqliteConnectOptions::from_str(&url)
        .expect("parse url")
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(opts)
        .await
        .expect("connect");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrations");
    let keypair = keysat::crypto::keys::load_or_generate(&pool)
        .await
        .expect("keypair");
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        db_path: PathBuf::from(":memory:"),
        admin_api_key: TEST_ADMIN_KEY.to_string(),
        btcpay_url: "http://btcpay.test".to_string(),
        btcpay_browser_url: None,
        btcpay_public_url: None,
        btcpay_api_key: None,
        btcpay_store_id: None,
        btcpay_webhook_secret: None,
        public_base_url: "http://keysat.test".to_string(),
        operator_name: None,
    };
    let state = AppState {
        db: pool,
        keypair: Arc::new(keypair),
        payment: Arc::new(RwLock::new(None)),
        provider_override: None,
        config: Arc::new(cfg),
        self_tier: Arc::new(RwLock::new(Tier::Unlicensed {
            reason: "test".into(),
        })),
        rates: keysat::rates::RateCache::new(),
    };
    (state, tmp)
}

/// Seed a USD-priced product, two perpetual policies (Standard
/// rank 1 / $25, Pro rank 2 / $75), a license currently on
/// Standard. Returns (license_id, standard_policy_id, pro_policy_id).
async fn seed_perpetual_ladder(state: &AppState) -> (String, String, String) {
    let product = repo::create_product(
        &state.db,
        "perp-ladder",
        "Perpetual Ladder",
        "",
        25_00, // $25.00 (cents); price_sats backfill from product create
        &json!({}),
    )
    .await
    .expect("create_product");
    // Update product to USD currency. create_product hits the SAT
    // default; bump it via a direct SQL UPDATE so the test setup
    // doesn't require going through the multi-currency admin path.
    sqlx::query(
        "UPDATE products SET price_currency = 'USD', price_value = 2500 WHERE id = ?",
    )
    .bind(&product.id)
    .execute(&state.db)
    .await
    .unwrap();

    // Standard tier: $25 perpetual, rank 1.
    let standard = repo::create_policy(
        &state.db,
        &product.id,
        "Standard",
        "standard",
        0,         // perpetual
        0,
        1,
        false,
        Some(2500), // $25.00 in cents
        &["core".into()],
        &json!({}),
        None,
        0,
        None,
        repo::RecurringConfig::off(),
        Some(1),
    )
    .await
    .expect("create standard");

    // Pro tier: $75 perpetual, rank 2, more entitlements.
    let pro = repo::create_policy(
        &state.db,
        &product.id,
        "Pro",
        "pro",
        0,
        0,
        3,
        false,
        Some(7500), // $75.00 in cents
        &["core".into(), "ai_summaries".into(), "export".into()],
        &json!({}),
        None,
        0,
        None,
        repo::RecurringConfig::off(),
        Some(2),
    )
    .await
    .expect("create pro");

    // Issue a license under Standard.
    let license_id = Uuid::new_v4().to_string();
    repo::create_license(
        &state.db,
        &license_id,
        &product.id,
        None,
        &Utc::now().to_rfc3339(),
        &json!({}),
        Some(&standard.id),
        None,                     // perpetual
        0,
        1,
        &["core".to_string()],
        false,
        None,
        None,
    )
    .await
    .expect("create_license");

    (license_id, standard.id, pro.id)
}

#[tokio::test]
async fn perpetual_upgrade_quote_returns_flat_price_difference() {
    let (state, _tmp) = make_state().await;
    let (license_id, _standard_id, pro_id) = seed_perpetual_ladder(&state).await;

    let license = repo::get_license_by_id(&state.db, &license_id)
        .await
        .unwrap()
        .unwrap();
    let pro = repo::get_policy_by_id(&state.db, &pro_id).await.unwrap().unwrap();

    let quote = compute_upgrade_quote(&state, &license, &pro, QuoteMode::Buyer).await.unwrap();

    assert_eq!(quote.direction, TierDirection::Upgrade);
    assert_eq!(quote.listed_currency, "USD");
    // Pro $75 - Standard $25 = $50 = 5000 cents.
    assert_eq!(quote.proration_charge_value, 5000);
    assert_eq!(quote.effective_at, EffectiveAt::Immediate);
    // Perpetual: no next-cycle charge.
    assert_eq!(quote.next_renewal_charge, None);
    assert_eq!(quote.next_renewal_period_days, None);
}

#[tokio::test]
async fn perpetual_downgrade_is_admin_only() {
    let (state, _tmp) = make_state().await;
    let (_lic, standard_id, pro_id) = seed_perpetual_ladder(&state).await;

    // Re-issue a license, but on Pro this time, so we can attempt
    // a Pro → Standard downgrade (which should be rejected).
    let license_id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let pro = repo::get_policy_by_id(&state.db, &pro_id).await.unwrap().unwrap();
    repo::create_license(
        &state.db,
        &license_id,
        &pro.product_id,
        None,
        &now,
        &json!({}),
        Some(&pro.id),
        None,
        0,
        3,
        &["core".to_string()],
        false,
        None,
        None,
    )
    .await
    .unwrap();

    let license = repo::get_license_by_id(&state.db, &license_id)
        .await
        .unwrap()
        .unwrap();
    let standard = repo::get_policy_by_id(&state.db, &standard_id)
        .await
        .unwrap()
        .unwrap();

    let err = compute_upgrade_quote(&state, &license, &standard, QuoteMode::Buyer)
        .await
        .expect_err("perpetual downgrade should be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("admin-only"),
        "perpetual downgrade error must mention admin-only path: {msg}"
    );
}

#[tokio::test]
async fn quote_rejects_target_with_null_tier_rank() {
    let (state, _tmp) = make_state().await;
    let (license_id, _, _) = seed_perpetual_ladder(&state).await;

    // Make a target policy that DELIBERATELY has tier_rank = NULL.
    let license = repo::get_license_by_id(&state.db, &license_id)
        .await
        .unwrap()
        .unwrap();
    let unlisted = repo::create_policy(
        &state.db,
        &license.product_id,
        "Promo",
        "promo",
        0,
        0,
        1,
        false,
        Some(5000),
        &["core".into()],
        &json!({}),
        None,
        0,
        None,
        repo::RecurringConfig::off(),
        None, // out of ladder
    )
    .await
    .unwrap();

    let err = compute_upgrade_quote(&state, &license, &unlisted, QuoteMode::Buyer)
        .await
        .expect_err("unlisted target should be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("not in any tier ladder"),
        "expected ladder rejection; got: {msg}"
    );
}

#[tokio::test]
async fn quote_rejects_same_policy() {
    let (state, _tmp) = make_state().await;
    let (license_id, standard_id, _) = seed_perpetual_ladder(&state).await;

    let license = repo::get_license_by_id(&state.db, &license_id)
        .await
        .unwrap()
        .unwrap();
    let same = repo::get_policy_by_id(&state.db, &standard_id)
        .await
        .unwrap()
        .unwrap();
    let err = compute_upgrade_quote(&state, &license, &same, QuoteMode::Buyer)
        .await
        .expect_err("same-policy target should be rejected");
    assert!(format!("{err}").contains("same as current"));
}

/// Recurring upgrade with the buyer halfway through a 30-day cycle.
/// The quote should bill ~half of the price diff. We assert a
/// tolerance window since "now" depends on test execution time.
#[tokio::test]
async fn recurring_upgrade_prorates_against_time_remaining() {
    let (state, _tmp) = make_state().await;
    let now = Utc::now();
    let now_str = now.to_rfc3339();

    // USD-priced product.
    let product = repo::create_product(
        &state.db,
        "rec-ladder",
        "Recurring Ladder",
        "",
        2500,
        &json!({}),
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE products SET price_currency = 'USD', price_value = 2500 WHERE id = ?",
    )
    .bind(&product.id)
    .execute(&state.db)
    .await
    .unwrap();

    // Standard $25/mo monthly recurring, rank 1.
    let standard = repo::create_policy(
        &state.db,
        &product.id,
        "Standard",
        "standard",
        30 * 86_400,
        0,
        1,
        false,
        Some(2500),
        &["core".into()],
        &json!({}),
        None,
        0,
        None,
        repo::RecurringConfig {
            is_recurring: true,
            renewal_period_days: 30,
            grace_period_days: 7,
            trial_days: 0,
        },
        Some(1),
    )
    .await
    .unwrap();

    // Pro $75/mo monthly recurring, rank 2.
    let pro = repo::create_policy(
        &state.db,
        &product.id,
        "Pro",
        "pro",
        30 * 86_400,
        0,
        3,
        false,
        Some(7500),
        &["core".into(), "ai_summaries".into()],
        &json!({}),
        None,
        0,
        None,
        repo::RecurringConfig {
            is_recurring: true,
            renewal_period_days: 30,
            grace_period_days: 7,
            trial_days: 0,
        },
        Some(2),
    )
    .await
    .unwrap();

    // License + active subscription on Standard, ~15 days into a
    // 30-day cycle.
    let license_id = Uuid::new_v4().to_string();
    repo::create_license(
        &state.db,
        &license_id,
        &product.id,
        None,
        &now_str,
        &json!({}),
        Some(&standard.id),
        Some(&(now + chrono::Duration::days(30)).to_rfc3339()),
        0,
        1,
        &["core".to_string()],
        false,
        None,
        None,
    )
    .await
    .unwrap();

    let next_renewal = (now + chrono::Duration::days(15)).to_rfc3339();
    sqlx::query(
        "INSERT INTO subscriptions(id, license_id, policy_id, product_id, period_days, \
         listed_currency, listed_value, status, started_at, next_renewal_at, \
         consecutive_failures, created_at, updated_at) \
         VALUES('sub1', ?, ?, ?, 30, 'USD', 2500, 'active', ?, ?, 0, ?, ?)",
    )
    .bind(&license_id)
    .bind(&standard.id)
    .bind(&product.id)
    .bind(&now_str)
    .bind(&next_renewal)
    .bind(&now_str)
    .bind(&now_str)
    .execute(&state.db)
    .await
    .unwrap();

    let license = repo::get_license_by_id(&state.db, &license_id)
        .await
        .unwrap()
        .unwrap();
    let quote = compute_upgrade_quote(&state, &license, &pro, QuoteMode::Buyer).await.unwrap();

    assert_eq!(quote.direction, TierDirection::Upgrade);
    assert_eq!(quote.listed_currency, "USD");
    assert_eq!(quote.next_renewal_charge, Some(7500));
    assert_eq!(quote.next_renewal_period_days, Some(30));
    assert_eq!(quote.effective_at, EffectiveAt::Immediate);

    // Diff is $50 (5000 cents). 15 days remaining out of 30, so
    // ~$25 (2500 cents). num_days() floors, so we expect 14 or 15
    // days remaining depending on test-execution timing. Tolerance
    // window: 2300..=2600.
    assert!(
        (2300..=2600).contains(&quote.proration_charge_value),
        "proration should be ~half of $50 diff for ~15 days remaining; got {}",
        quote.proration_charge_value
    );
}

#[tokio::test]
async fn recurring_downgrade_is_zero_charge_at_next_cycle() {
    let (state, _tmp) = make_state().await;
    let now = Utc::now();
    let now_str = now.to_rfc3339();

    let product = repo::create_product(
        &state.db,
        "rec-down",
        "Down",
        "",
        2500,
        &json!({}),
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE products SET price_currency = 'USD', price_value = 2500 WHERE id = ?",
    )
    .bind(&product.id)
    .execute(&state.db)
    .await
    .unwrap();

    let standard = repo::create_policy(
        &state.db,
        &product.id,
        "Standard",
        "standard",
        30 * 86_400,
        0,
        1,
        false,
        Some(2500),
        &["core".into()],
        &json!({}),
        None,
        0,
        None,
        repo::RecurringConfig {
            is_recurring: true,
            renewal_period_days: 30,
            grace_period_days: 7,
            trial_days: 0,
        },
        Some(1),
    )
    .await
    .unwrap();
    let pro = repo::create_policy(
        &state.db,
        &product.id,
        "Pro",
        "pro",
        30 * 86_400,
        0,
        3,
        false,
        Some(7500),
        &["core".into(), "ai_summaries".into()],
        &json!({}),
        None,
        0,
        None,
        repo::RecurringConfig {
            is_recurring: true,
            renewal_period_days: 30,
            grace_period_days: 7,
            trial_days: 0,
        },
        Some(2),
    )
    .await
    .unwrap();

    // License on Pro, with a sub. Buyer wants to downgrade to Standard.
    let license_id = Uuid::new_v4().to_string();
    repo::create_license(
        &state.db,
        &license_id,
        &product.id,
        None,
        &now_str,
        &json!({}),
        Some(&pro.id),
        Some(&(now + chrono::Duration::days(30)).to_rfc3339()),
        0,
        3,
        &["core".to_string()],
        false,
        None,
        None,
    )
    .await
    .unwrap();
    let next_renewal = (now + chrono::Duration::days(20)).to_rfc3339();
    sqlx::query(
        "INSERT INTO subscriptions(id, license_id, policy_id, product_id, period_days, \
         listed_currency, listed_value, status, started_at, next_renewal_at, \
         consecutive_failures, created_at, updated_at) \
         VALUES('sub2', ?, ?, ?, 30, 'USD', 7500, 'active', ?, ?, 0, ?, ?)",
    )
    .bind(&license_id)
    .bind(&pro.id)
    .bind(&product.id)
    .bind(&now_str)
    .bind(&next_renewal)
    .bind(&now_str)
    .bind(&now_str)
    .execute(&state.db)
    .await
    .unwrap();

    let license = repo::get_license_by_id(&state.db, &license_id)
        .await
        .unwrap()
        .unwrap();
    let quote = compute_upgrade_quote(&state, &license, &standard, QuoteMode::Buyer).await.unwrap();

    assert_eq!(quote.direction, TierDirection::Downgrade);
    assert_eq!(quote.proration_charge_value, 0,
        "recurring downgrade should be zero-charge today");
    // Effective at next renewal — full Pro through current cycle.
    match quote.effective_at {
        EffectiveAt::At(ref s) => assert_eq!(s, &next_renewal),
        EffectiveAt::Immediate => panic!("recurring downgrade should defer to next cycle"),
    }
    assert_eq!(quote.next_renewal_charge, Some(2500));
}

/// apply_tier_change must update licenses (policy_id +
/// entitlements + max_machines + grace + expires_at) and, if a
/// recurring sub exists, the sub's policy_id + listed_value +
/// period_days.
#[tokio::test]
async fn apply_tier_change_mutates_license_and_subscription() {
    let (state, _tmp) = make_state().await;
    let now = Utc::now();
    let now_str = now.to_rfc3339();

    // Build a USD product + Standard/Pro recurring policies + a
    // license + sub on Standard (basically the same scaffolding as
    // the recurring-upgrade quote test).
    let product = repo::create_product(
        &state.db,
        "apply-test",
        "Apply",
        "",
        2500,
        &json!({}),
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE products SET price_currency = 'USD', price_value = 2500 WHERE id = ?",
    )
    .bind(&product.id)
    .execute(&state.db)
    .await
    .unwrap();
    let standard = repo::create_policy(
        &state.db,
        &product.id,
        "Standard",
        "standard",
        30 * 86_400,
        0,
        1,
        false,
        Some(2500),
        &["core".into()],
        &json!({}),
        None,
        0,
        None,
        repo::RecurringConfig {
            is_recurring: true,
            renewal_period_days: 30,
            grace_period_days: 7,
            trial_days: 0,
        },
        Some(1),
    )
    .await
    .unwrap();
    let pro = repo::create_policy(
        &state.db,
        &product.id,
        "Pro",
        "pro",
        365 * 86_400, // annual entitlement window
        0,
        5, // bigger max_machines on Pro
        false,
        Some(75_000), // $750 / yr, paid annually
        &["core".into(), "ai_summaries".into(), "export".into()],
        &json!({}),
        None,
        0,
        None,
        repo::RecurringConfig {
            is_recurring: true,
            renewal_period_days: 365, // annual cadence
            grace_period_days: 14,
            trial_days: 0,
        },
        Some(2),
    )
    .await
    .unwrap();

    let license_id = Uuid::new_v4().to_string();
    repo::create_license(
        &state.db,
        &license_id,
        &product.id,
        None,
        &now_str,
        &json!({}),
        Some(&standard.id),
        Some(&(now + chrono::Duration::days(30)).to_rfc3339()),
        0,
        1,
        &["core".to_string()],
        false,
        None,
        None,
    )
    .await
    .unwrap();
    let next_renewal = (now + chrono::Duration::days(20)).to_rfc3339();
    sqlx::query(
        "INSERT INTO subscriptions(id, license_id, policy_id, product_id, period_days, \
         listed_currency, listed_value, status, started_at, next_renewal_at, \
         consecutive_failures, created_at, updated_at) \
         VALUES('sub-apply', ?, ?, ?, 30, 'USD', 2500, 'active', ?, ?, 0, ?, ?)",
    )
    .bind(&license_id)
    .bind(&standard.id)
    .bind(&product.id)
    .bind(&now_str)
    .bind(&next_renewal)
    .bind(&now_str)
    .bind(&now_str)
    .execute(&state.db)
    .await
    .unwrap();

    let product_full = repo::get_product_by_id(&state.db, &product.id)
        .await
        .unwrap()
        .unwrap();

    apply_tier_change(&state.db, &license_id, &pro, &product_full)
        .await
        .expect("apply_tier_change");

    // License now reflects Pro's policy_id, entitlements, max_machines.
    let license_after = repo::get_license_by_id(&state.db, &license_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(license_after.policy_id.as_deref(), Some(pro.id.as_str()));
    assert_eq!(license_after.max_machines, 5);
    assert!(license_after.entitlements.contains(&"ai_summaries".to_string()));
    assert!(license_after.entitlements.contains(&"export".to_string()));
    assert!(license_after.expires_at.is_some(), "annual Pro should set expires_at");

    // Subscription now reflects Pro's policy_id, $750 listed_value,
    // 365-day period.
    let (pol_id, val, period): (String, i64, i64) = sqlx::query_as(
        "SELECT policy_id, listed_value, period_days FROM subscriptions WHERE id = 'sub-apply'",
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(pol_id, pro.id);
    assert_eq!(val, 75_000);
    assert_eq!(period, 365);
}

/// Pending tier_changes with effective_at <= now are applied by
/// the renewal worker before pricing the next cycle. Mirrors the
/// recurring-downgrade flow that ships alongside this hook: admin
/// records "downgrade Pro → Standard at next cycle" with
/// effective_at = next_renewal_at, and the worker fires it on tick.
#[tokio::test]
async fn renewal_worker_applies_pending_tier_change_before_billing() {
    use keysat::payment::{
        CreateInvoiceParams, CreatedInvoiceHandle, PaymentProvider, ProviderInvoiceStatus,
        ProviderKind, ProviderWebhookEvent,
    };
    use std::any::Any;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Local mock provider — same shape as the renewal-worker tests'
    // mock. Captures the listed_value-derived sat amount so we can
    // assert the worker billed AT THE NEW TIER, not the old one.
    #[derive(Default)]
    struct CapturingProvider {
        next_id: AtomicU64,
        last_amount_sats: std::sync::atomic::AtomicI64,
    }
    #[async_trait::async_trait]
    impl PaymentProvider for CapturingProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::Btcpay
        }
        async fn create_invoice(
            &self,
            params: CreateInvoiceParams<'_>,
        ) -> anyhow::Result<CreatedInvoiceHandle> {
            self.last_amount_sats
                .store(params.amount.amount, Ordering::SeqCst);
            let n = self.next_id.fetch_add(1, Ordering::SeqCst);
            Ok(CreatedInvoiceHandle {
                provider_invoice_id: format!("cap-{n}"),
                checkout_url: format!("http://cap/{n}"),
            })
        }
        async fn get_invoice_status(&self, _id: &str) -> anyhow::Result<ProviderInvoiceStatus> {
            Ok(ProviderInvoiceStatus::Pending)
        }
        fn validate_webhook(
            &self,
            _h: &axum::http::HeaderMap,
            _b: &[u8],
        ) -> anyhow::Result<ProviderWebhookEvent> {
            anyhow::bail!("not exercised")
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    let (state, _tmp) = make_state().await;
    let mock = Arc::new(CapturingProvider::default());
    *state.payment.write().await = Some(mock.clone() as Arc<dyn PaymentProvider>);

    let now = Utc::now();
    let now_str = now.to_rfc3339();

    // SAT-priced product (no rate fetcher) for a clean assertion on
    // the amount billed.
    let product = repo::create_product(
        &state.db,
        "rw-pending",
        "Renewal worker pending",
        "",
        2500, // 2500 sats base
        &json!({}),
    )
    .await
    .unwrap();

    let standard = repo::create_policy(
        &state.db,
        &product.id,
        "Standard",
        "standard",
        30 * 86_400,
        0,
        1,
        false,
        Some(2500), // 2500 sats / mo
        &["core".into()],
        &json!({}),
        None,
        0,
        None,
        repo::RecurringConfig {
            is_recurring: true,
            renewal_period_days: 30,
            grace_period_days: 7,
            trial_days: 0,
        },
        Some(1),
    )
    .await
    .unwrap();
    let pro = repo::create_policy(
        &state.db,
        &product.id,
        "Pro",
        "pro",
        30 * 86_400,
        0,
        3,
        false,
        Some(7500), // 7500 sats / mo
        &["core".into(), "ai_summaries".into()],
        &json!({}),
        None,
        0,
        None,
        repo::RecurringConfig {
            is_recurring: true,
            renewal_period_days: 30,
            grace_period_days: 7,
            trial_days: 0,
        },
        Some(2),
    )
    .await
    .unwrap();

    // License + sub on Pro, due now (next_renewal_at in the past).
    let license_id = Uuid::new_v4().to_string();
    repo::create_license(
        &state.db,
        &license_id,
        &product.id,
        None,
        &now_str,
        &json!({}),
        Some(&pro.id),
        Some(&(now + chrono::Duration::days(30)).to_rfc3339()),
        0,
        3,
        &["core".to_string(), "ai_summaries".to_string()],
        false,
        None,
        None,
    )
    .await
    .unwrap();

    let past_due = (now - chrono::Duration::minutes(5)).to_rfc3339();
    sqlx::query(
        "INSERT INTO subscriptions(id, license_id, policy_id, product_id, period_days, \
         listed_currency, listed_value, status, started_at, next_renewal_at, \
         consecutive_failures, created_at, updated_at) \
         VALUES('sub-rw-pending', ?, ?, ?, 30, 'SAT', 7500, 'active', ?, ?, 0, ?, ?)",
    )
    .bind(&license_id)
    .bind(&pro.id)
    .bind(&product.id)
    .bind(&now_str)
    .bind(&past_due)
    .bind(&now_str)
    .bind(&now_str)
    .execute(&state.db)
    .await
    .unwrap();

    // Operator (or the future admin endpoint) records a downgrade
    // tier_change with effective_at = now (= already past). No
    // invoice attached (this is the comp / scheduled-downgrade
    // shape).
    record_tier_change(
        &state.db,
        &license_id,
        &pro.id,
        &standard.id,
        TierDirection::Downgrade,
        "SAT",
        0,
        None,
        &now_str,
        "admin",
        Some("scheduled downgrade for cycle boundary"),
    )
    .await
    .unwrap();

    // Tick the renewal worker.
    keysat::subscriptions::tick(&state).await.unwrap();

    // The new invoice was created at the NEW tier's price (2500
    // sats), not the old one (7500 sats). This proves the renewal
    // worker applied the pending tier change BEFORE pricing.
    let billed = mock.last_amount_sats.load(Ordering::SeqCst);
    assert_eq!(
        billed, 2500,
        "renewal must bill at the new (Standard) tier after the pending downgrade applied; got {billed} sats"
    );

    // License is now on Standard (apply_tier_change ran during the hook).
    let license_after = repo::get_license_by_id(&state.db, &license_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(license_after.policy_id.as_deref(), Some(standard.id.as_str()));
}

/// record_tier_change writes the audit row, and
/// list_tier_changes_for_license / get_tier_change_by_invoice
/// surface it back. Round-trips the data we'd write at settle time.
#[tokio::test]
async fn record_and_lookup_tier_change_round_trip() {
    let (state, _tmp) = make_state().await;
    let (license_id, standard_id, pro_id) = seed_perpetual_ladder(&state).await;

    // Seed a placeholder invoice so the FK on tier_changes.invoice_id
    // can succeed.
    let invoice_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO invoices(id, btcpay_invoice_id, product_id, amount_sats, \
         checkout_url, status, created_at, updated_at, listed_currency, \
         listed_value, policy_id) \
         VALUES(?, ?, (SELECT product_id FROM licenses WHERE id = ?), 0, \
                ?, 'pending', ?, ?, 'USD', 5000, ?)",
    )
    .bind(&invoice_id)
    .bind(format!("test-inv-{}", &invoice_id[..8]))
    .bind(&license_id)
    .bind("http://test.invalid/inv")
    .bind(Utc::now().to_rfc3339())
    .bind(Utc::now().to_rfc3339())
    .bind(&pro_id)
    .execute(&state.db)
    .await
    .unwrap();

    let id = record_tier_change(
        &state.db,
        &license_id,
        &standard_id,
        &pro_id,
        TierDirection::Upgrade,
        "USD",
        5000,
        Some(&invoice_id),
        &Utc::now().to_rfc3339(),
        "buyer",
        Some("user clicked upgrade in app"),
    )
    .await
    .expect("record_tier_change");

    // list_for_license returns the row.
    let history = list_tier_changes_for_license(&state.db, &license_id)
        .await
        .unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id, id);
    assert_eq!(history[0].direction, "upgrade");
    assert_eq!(history[0].proration_charge_value, 5000);
    assert_eq!(history[0].listed_currency, "USD");
    assert_eq!(history[0].invoice_id.as_deref(), Some(invoice_id.as_str()));

    // get_by_invoice round-trips too.
    let by_inv = keysat::upgrades::get_tier_change_by_invoice(&state.db, &invoice_id)
        .await
        .unwrap()
        .expect("found by invoice");
    assert_eq!(by_inv.id, id);
}
