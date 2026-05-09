//! Entry point. Wires config → logging → DB → keypair → HTTP server.
//!
//! The actual modules (api, btcpay, db, etc.) live in `src/lib.rs` so that
//! integration tests under `tests/` can also reach them. Both the binary
//! and the library compile from the same source files; nothing here
//! changes between targets.

use anyhow::Context;
use keysat::{
    analytics, api, btcpay, config, crypto, db, license_self, payment, reconcile, subscriptions,
    webhooks,
};
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // --- logging ---
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn,hyper=warn")),
        )
        .with(fmt::layer().with_target(false))
        .init();

    // --- config ---
    let cfg = config::Config::from_env().context("loading configuration")?;
    tracing::info!(
        bind = %cfg.bind,
        db = %cfg.db_path.display(),
        btcpay_url = %cfg.btcpay_url,
        btcpay_browser_url = ?cfg.btcpay_browser_url,
        btcpay_public_url = ?cfg.btcpay_public_url,
        "starting keysat v{}",
        env!("CARGO_PKG_VERSION")
    );

    // --- self-license tier (Keysat-licenses-Keysat) ---
    // Verifies any /data/keysat-license.txt against the embedded master
    // pubkey. In permissive builds (default) a missing/invalid license
    // logs a warning and we continue. In enforce builds (compiled with
    // KEYSAT_LICENSE_ENFORCE=1) a missing/invalid license refuses to
    // start. Result is held in app state so the admin UI can surface it.
    let self_tier = Arc::new(tokio::sync::RwLock::new(
        license_self::check_at_boot()
            .context("Keysat self-license check failed (enforce mode)")?,
    ));

    // --- database ---
    let pool = db::init(&cfg.db_path).await?;

    // --- signing key ---
    let keypair = crypto::keys::load_or_generate(&pool).await?;
    tracing::info!(
        "signing key ready; public key:\n{}",
        keypair.public_key_pem.trim()
    );

    // --- payment provider (may be None until operator connects) ---
    // Resolution order:
    //   1. operator's explicit preference from the
    //      active_payment_provider setting (set by the most recent
    //      Connect or Activate action),
    //   2. fallback for legacy installs without the setting:
    //      BTCPay first, Zaprite second. Once we ship v0.3 with the
    //      multi-provider routing layer this fallback retires.
    let preferred = payment::read_active_provider_preference(&pool).await;
    let provider: Option<Arc<dyn payment::PaymentProvider>> = match preferred {
        Some(payment::ProviderKind::Zaprite) => {
            // Operator explicitly chose Zaprite. Try Zaprite; if it
            // can't be loaded (e.g., the row was deleted out from
            // under the setting), fall through to BTCPay rather
            // than booting unconfigured.
            load_zaprite_provider(&pool)
                .await
                .map(|p| Arc::new(p) as Arc<dyn payment::PaymentProvider>)
                .or_else(|| {
                    tracing::warn!(
                        "active_payment_provider=zaprite but zaprite_config is missing; \
                         falling back to BTCPay"
                    );
                    None
                })
                .or(load_btcpay_provider(&pool, &cfg)
                    .await
                    .map(|p| Arc::new(p) as Arc<dyn payment::PaymentProvider>))
        }
        Some(payment::ProviderKind::Btcpay) | None => {
            // Either operator chose BTCPay, or no preference recorded
            // yet (legacy install). Either way, BTCPay wins if
            // configured; Zaprite as fallback.
            load_btcpay_provider(&pool, &cfg)
                .await
                .map(|p| Arc::new(p) as Arc<dyn payment::PaymentProvider>)
                .or_else(|| {
                    if preferred == Some(payment::ProviderKind::Btcpay) {
                        tracing::warn!(
                            "active_payment_provider=btcpay but btcpay_config is missing; \
                             falling back to Zaprite"
                        );
                    }
                    None
                })
                .or(load_zaprite_provider(&pool)
                    .await
                    .map(|p| Arc::new(p) as Arc<dyn payment::PaymentProvider>))
        }
    };
    match &provider {
        Some(p) => tracing::info!(provider = p.kind().as_str(), "payment provider connected"),
        None => tracing::warn!(
            "no payment provider yet configured — purchases will return 503 until the \
             operator completes the 'Connect BTCPay' or 'Connect Zaprite' flow"
        ),
    }

    let state = api::AppState {
        db: pool,
        keypair: Arc::new(keypair),
        payment: Arc::new(tokio::sync::RwLock::new(provider)),
        config: Arc::new(cfg.clone()),
        self_tier,
        rates: keysat::rates::RateCache::new(),
    };

    // Spawn background loops before handing state to the router.
    reconcile::spawn(state.clone());
    webhooks::spawn_delivery_worker(state.clone());
    // Opt-in community analytics — every tick checks the toggle
    // and short-circuits if disabled (default), so spawning is safe
    // unconditionally.
    analytics::spawn(state.clone());
    // Recurring subscriptions renewal worker. Picks up subs whose
    // next_renewal_at has passed, creates fresh invoices via the
    // active provider, transitions state. No-op if no recurring
    // subscriptions exist; safe to spawn unconditionally.
    subscriptions::spawn(state.clone());

    // Hourly session reaper — drops sessions whose expires_at < now.
    {
        let pool = state.db.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                match db::repo::reap_expired_sessions(&pool).await {
                    Ok(n) if n > 0 => tracing::info!("reaped {n} expired session(s)"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!("session reaper: {e}"),
                }
            }
        });
    }

    // Self-tier live refresher — re-reads the daemon's own license
    // row from the local DB every hour and updates `state.self_tier`
    // with the live entitlements. Without this, an admin Change Tier
    // on the daemon's own license never propagates to the running
    // process (boot-time check trusts the signed payload's
    // entitlements, which are immutable). See
    // `license_self::refresh_self_tier_from_db` for the rationale.
    {
        // Initial refresh — fires once now so an operator who changed
        // their tier between two daemon runs sees the new tier before
        // the first interval tick.
        let current = state.self_tier.read().await.clone();
        let next = license_self::refresh_self_tier_from_db(&state.db, &current).await;
        *state.self_tier.write().await = next;

        // Periodic refresh thereafter.
        let state2 = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Burn the immediate-fire tick so the first real tick is
            // 1h from spawn (we already ran the initial refresh above).
            interval.tick().await;
            loop {
                interval.tick().await;
                let current = state2.self_tier.read().await.clone();
                let next =
                    license_self::refresh_self_tier_from_db(&state2.db, &current).await;
                *state2.self_tier.write().await = next;
            }
        });
    }

    let app = api::router(state).layer(TraceLayer::new_for_http());

    // --- serve ---
    let listener = tokio::net::TcpListener::bind(cfg.bind)
        .await
        .with_context(|| format!("binding to {}", cfg.bind))?;
    tracing::info!("listening on http://{}", cfg.bind);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("shutdown complete");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}

/// Load a BtcpayProvider from (in order): DB, then env var seed, then None.
/// Never fails — an unconfigured service simply returns 503 on purchase paths
/// until the operator completes the connect flow. Returns the concrete
/// `BtcpayProvider` so the caller can decide how to wrap it (we wrap as
/// `Arc<dyn PaymentProvider>` in `main`).
async fn load_btcpay_provider(
    pool: &sqlx::SqlitePool,
    cfg: &config::Config,
) -> Option<payment::btcpay::BtcpayProvider> {
    // DB first.
    if let Ok(Some(saved)) = btcpay::config::load(pool).await {
        let client = btcpay::client::BtcpayClient::new(
            &saved.base_url,
            &saved.api_key,
            &saved.store_id,
        );
        return Some(
            payment::btcpay::BtcpayProvider::new(client, saved.webhook_secret)
                .with_public_base(cfg.btcpay_public_url.clone()),
        );
    }
    // Fall back to env seed (useful for dev / legacy installs).
    if let (Some(api_key), Some(store_id), Some(secret)) = (
        cfg.btcpay_api_key.as_deref(),
        cfg.btcpay_store_id.as_deref(),
        cfg.btcpay_webhook_secret.as_deref(),
    ) {
        let client =
            btcpay::client::BtcpayClient::new(&cfg.btcpay_url, api_key, store_id);
        // Persist the seed into DB so it survives env changes.
        let _ = btcpay::config::save(
            pool,
            &btcpay::config::BtcpayConfig {
                base_url: cfg.btcpay_url.clone(),
                api_key: api_key.to_string(),
                store_id: store_id.to_string(),
                webhook_id: None,
                webhook_secret: secret.to_string(),
            },
        )
        .await;
        return Some(
            payment::btcpay::BtcpayProvider::new(client, secret.to_string())
                .with_public_base(cfg.btcpay_public_url.clone()),
        );
    }
    None
}

/// Load a ZapriteProvider from the DB, if the operator has previously
/// completed the Connect Zaprite flow. No env-var fallback because
/// Zaprite is brand new in this codebase — operators who want it
/// configure it via the admin UI / StartOS Action, not env vars.
async fn load_zaprite_provider(
    pool: &sqlx::SqlitePool,
) -> Option<payment::zaprite::ZapriteProvider> {
    if let Ok(Some(saved)) = payment::zaprite::config::load(pool).await {
        let client =
            payment::zaprite::ZapriteClient::new(&saved.base_url, &saved.api_key);
        return Some(payment::zaprite::ZapriteProvider::new(client));
    }
    None
}
