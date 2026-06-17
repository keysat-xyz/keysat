//! Live re-validation of the agent-payment-connect network detection against a
//! real BTCPay regtest box. Exercises the daemon's ACTUAL
//! `btcpay::client::fetch_onchain_network` (not a curl reimplementation), which
//! is what the scoped-connect gate calls at callback time.
//!
//! `#[ignore]` by default — it needs a running BTCPay regtest stack and reads
//! its connection params from the environment (no secrets in the tree). Bring
//! the box up and run:
//!
//! ```sh
//! cd ../onboarding-harness/stage2/btcpay-regtest && docker compose -p keysat-btcpay up -d
//! # mint a canmodifystoresettings token + a store with an on-chain wallet, then:
//! source ../onboarding-harness/stage2/btcpay-regtest/.live-env
//! cargo test --test btcpay_network_live -- --ignored --nocapture
//! ```
//!
//! Spec: `plans/agent-payment-connect-scope.md` §6.1 — "BTCPay on-chain address
//! network detection MUST be validated against a live regtest box."

use keysat::btcpay::client::fetch_onchain_network;
use keysat::btcpay::network::BitcoinNetwork;

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

#[tokio::test]
#[ignore = "needs a live BTCPay regtest box; set KEYSAT_LIVE_BTCPAY_* env"]
async fn regtest_store_resolves_to_regtest() {
    let (Some(base), Some(key), Some(store)) = (
        env("KEYSAT_LIVE_BTCPAY_URL"),
        env("KEYSAT_LIVE_BTCPAY_KEY"),
        env("KEYSAT_LIVE_BTCPAY_STORE_REGTEST"),
    ) else {
        eprintln!("SKIP: set KEYSAT_LIVE_BTCPAY_URL / _KEY / _STORE_REGTEST");
        return;
    };

    let net = fetch_onchain_network(&base, &key, &store)
        .await
        .expect("detection call should not transport-error against a live box");
    println!("regtest store {store} resolved to {net:?}");
    assert_eq!(
        net,
        Some(BitcoinNetwork::Regtest),
        "the on-chain wallet's bcrt1 address must classify as Regtest (non-mainnet → scoped connect allowed)"
    );
    assert!(!net.unwrap().is_mainnet(), "regtest must not be mainnet");
}

#[tokio::test]
#[ignore = "needs a live BTCPay regtest box; set KEYSAT_LIVE_BTCPAY_* env"]
async fn store_without_onchain_wallet_is_undetermined() {
    let (Some(base), Some(key), Some(store)) = (
        env("KEYSAT_LIVE_BTCPAY_URL"),
        env("KEYSAT_LIVE_BTCPAY_KEY"),
        env("KEYSAT_LIVE_BTCPAY_STORE_NOWALLET"),
    ) else {
        eprintln!("SKIP: set KEYSAT_LIVE_BTCPAY_URL / _KEY / _STORE_NOWALLET");
        return;
    };

    let net = fetch_onchain_network(&base, &key, &store)
        .await
        .expect("detection call should not transport-error");
    println!("no-wallet store {store} resolved to {net:?}");
    // No on-chain wallet → undetermined → caller fails closed to mainnet → deny.
    assert_eq!(
        net, None,
        "a store with no on-chain wallet must be undetermined so the gate fails closed"
    );
}
