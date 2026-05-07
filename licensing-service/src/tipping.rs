//! Tip-recipient-on-policy: fire a Lightning tip after every successful
//! license issuance under a tip-enabled policy.
//!
//! Flow:
//!   1. License is issued (existing path; this module is called from the
//!      reconcile/webhook layer once that completes).
//!   2. Look up the policy. If `tip_recipient` is set and `tip_pct_bps > 0`,
//!      compute `amount_sats = paid_sats * tip_pct_bps / 10000`.
//!   3. Resolve the Lightning Address. We support exactly the Lightning
//!      Address scheme `user@domain`, which maps to
//!      `https://domain/.well-known/lnurlp/user`. Plain LNURL-pay bech32
//!      strings are not supported in v0.1; can add later.
//!   4. Fetch the LNURL-pay metadata, verify the amount fits in
//!      `[minSendable, maxSendable]`, request a BOLT11 invoice for our
//!      amount via the `callback` URL.
//!   5. Pay the BOLT11 via the operator's BTCPay Lightning node.
//!   6. Record success/failure in the `tip_attempts` audit table.
//!
//! Failure semantics: this module **never** propagates errors back to the
//! issuance path. A tip failing is a logged + audited concern, not a reason
//! to fail a customer's purchase. Operators set up tipping voluntarily;
//! they accept the trade-off that an occasional tip will fail and can be
//! retried manually.

use crate::api::AppState;
use crate::db::repo;
use crate::models::Policy;
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

/// Maximum amount in millisats we'll send via a single tip. Defense in
/// depth — a misconfigured `tip_pct_bps` shouldn't be able to drain the
/// wallet on a single sale.
const MAX_TIP_MSAT: u64 = 5_000_000_000; // 50,000,000 sats; 0.5 BTC

#[derive(Debug, Deserialize)]
struct LnurlPayMetadata {
    callback: String,
    #[serde(rename = "minSendable")]
    min_sendable: u64,
    #[serde(rename = "maxSendable")]
    max_sendable: u64,
    #[serde(default)]
    tag: String,
}

#[derive(Debug, Deserialize)]
struct LnurlPayInvoice {
    pr: String, // BOLT11
}

/// Spawn a tip in the background. Caller fires this after issuance and
/// returns immediately — the customer's purchase response doesn't wait for
/// the tip to complete.
pub fn spawn_tip(
    state: AppState,
    license_id: String,
    policy: Policy,
    paid_sats: i64,
) {
    tokio::spawn(async move {
        if let Err(e) = run_tip(&state, &license_id, &policy, paid_sats).await {
            tracing::warn!(
                license = %license_id,
                policy = %policy.id,
                "tip flow ended with error: {e:#}"
            );
            // run_tip records its own audit entries; this is just the catch-all log.
        }
    });
}

async fn run_tip(
    state: &AppState,
    license_id: &str,
    policy: &Policy,
    paid_sats: i64,
) -> Result<()> {
    let recipient = match &policy.tip_recipient {
        Some(r) if !r.trim().is_empty() => r.trim().to_string(),
        _ => return Ok(()), // no tip configured; not an error
    };
    let pct = policy.tip_pct_bps;
    if pct <= 0 {
        return Ok(());
    }
    let label = policy.tip_label.clone();

    // Compute tip amount. Round down (floor); we never tip more than the
    // configured percentage of what the buyer paid.
    let tip_sats = paid_sats.saturating_mul(pct) / 10_000;
    if tip_sats <= 0 {
        repo::record_tip_attempt(
            &state.db,
            license_id,
            &policy.id,
            &recipient,
            0,
            pct,
            label.as_deref(),
            "skipped",
            Some("tip_sats <= 0 after percentage applied"),
            None,
        )
        .await
        .ok();
        return Ok(());
    }

    let tip_msat = (tip_sats as u64).saturating_mul(1000);
    if tip_msat > MAX_TIP_MSAT {
        repo::record_tip_attempt(
            &state.db,
            license_id,
            &policy.id,
            &recipient,
            tip_sats,
            pct,
            label.as_deref(),
            "skipped",
            Some(&format!(
                "tip exceeds safety cap ({} msat > {} msat)",
                tip_msat, MAX_TIP_MSAT
            )),
            None,
        )
        .await
        .ok();
        return Ok(());
    }

    // Resolve Lightning Address → LNURL-pay metadata.
    let metadata = match resolve_lightning_address(&recipient).await {
        Ok(m) => m,
        Err(e) => {
            let detail = format!("address resolution failed: {e:#}");
            tracing::warn!(license = %license_id, recipient = %recipient, "{detail}");
            repo::record_tip_attempt(
                &state.db,
                license_id,
                &policy.id,
                &recipient,
                tip_sats,
                pct,
                label.as_deref(),
                "failed",
                Some(&detail),
                None,
            )
            .await
            .ok();
            return Ok(());
        }
    };

    if tip_msat < metadata.min_sendable || tip_msat > metadata.max_sendable {
        let detail = format!(
            "tip amount {tip_msat} msat outside recipient bounds [{}, {}]",
            metadata.min_sendable, metadata.max_sendable
        );
        repo::record_tip_attempt(
            &state.db,
            license_id,
            &policy.id,
            &recipient,
            tip_sats,
            pct,
            label.as_deref(),
            "failed",
            Some(&detail),
            None,
        )
        .await
        .ok();
        return Ok(());
    }

    // Request a BOLT11 invoice from the recipient for our amount.
    let invoice = match request_lnurl_invoice(&metadata.callback, tip_msat).await {
        Ok(b) => b,
        Err(e) => {
            let detail = format!("invoice request failed: {e:#}");
            tracing::warn!(license = %license_id, "{detail}");
            repo::record_tip_attempt(
                &state.db,
                license_id,
                &policy.id,
                &recipient,
                tip_sats,
                pct,
                label.as_deref(),
                "failed",
                Some(&detail),
                None,
            )
            .await
            .ok();
            return Ok(());
        }
    };

    // Pay it via the operator's BTCPay Lightning node.
    let btcpay = match state.btcpay_client().await {
        Ok(c) => c,
        Err(e) => {
            let detail = format!("BTCPay client unavailable: {e:?}");
            repo::record_tip_attempt(
                &state.db,
                license_id,
                &policy.id,
                &recipient,
                tip_sats,
                pct,
                label.as_deref(),
                "failed",
                Some(&detail),
                None,
            )
            .await
            .ok();
            return Ok(());
        }
    };

    match btcpay.pay_lightning_invoice(&invoice).await {
        Ok(payment) => {
            let payment_hash = payment
                .get("paymentHash")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            tracing::info!(
                license = %license_id,
                recipient = %recipient,
                amount_sats = tip_sats,
                payment_hash = ?payment_hash,
                "tip sent"
            );
            repo::record_tip_attempt(
                &state.db,
                license_id,
                &policy.id,
                &recipient,
                tip_sats,
                pct,
                label.as_deref(),
                "sent",
                Some(&format!("paid via BTCPay LN node ({} sats)", tip_sats)),
                payment_hash.as_deref(),
            )
            .await
            .ok();
        }
        Err(e) => {
            let detail = format!("BTCPay pay-LN-invoice failed: {e:#}");
            tracing::warn!(license = %license_id, "{detail}");
            repo::record_tip_attempt(
                &state.db,
                license_id,
                &policy.id,
                &recipient,
                tip_sats,
                pct,
                label.as_deref(),
                "failed",
                Some(&detail),
                None,
            )
            .await
            .ok();
        }
    }
    Ok(())
}

/// Parse `user@domain` and fetch the LNURL-pay metadata document at
/// `https://domain/.well-known/lnurlp/user`. Returns the parsed metadata.
async fn resolve_lightning_address(addr: &str) -> Result<LnurlPayMetadata> {
    let (user, domain) = addr
        .split_once('@')
        .ok_or_else(|| anyhow!("not a Lightning Address (expected user@domain)"))?;
    if user.is_empty() || domain.is_empty() {
        bail!("Lightning Address has empty user or domain");
    }
    // Reasonable charset check — LN addresses are user-input-safe alphanum + dash + underscore + dot.
    let charset_ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    if !user.chars().all(charset_ok) || !domain.chars().all(charset_ok) {
        bail!("Lightning Address contains disallowed characters");
    }

    let url = format!("https://{domain}/.well-known/lnurlp/{user}");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("building HTTP client")?;
    let resp = client.get(&url).send().await.context("LNURL-pay GET")?;
    if !resp.status().is_success() {
        bail!("LNURL-pay endpoint returned {}", resp.status());
    }
    let metadata: LnurlPayMetadata = resp
        .json()
        .await
        .context("parsing LNURL-pay metadata response")?;

    if !metadata.tag.is_empty() && metadata.tag != "payRequest" {
        bail!(
            "expected LNURL-pay metadata tag='payRequest', got '{}'",
            metadata.tag
        );
    }
    if !metadata.callback.starts_with("https://") {
        bail!(
            "LNURL-pay callback must be HTTPS, got: {}",
            metadata.callback
        );
    }
    Ok(metadata)
}

/// Hit the recipient's `callback` URL with `?amount=<msat>` and return the
/// resulting BOLT11 invoice string.
async fn request_lnurl_invoice(callback: &str, amount_msat: u64) -> Result<String> {
    let sep = if callback.contains('?') { '&' } else { '?' };
    let url = format!("{callback}{sep}amount={amount_msat}");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("building HTTP client")?;
    let resp = client.get(&url).send().await.context("LNURL-pay invoice GET")?;
    if !resp.status().is_success() {
        bail!(
            "LNURL-pay invoice endpoint returned {}",
            resp.status()
        );
    }

    // The response can be either { pr, ... } on success or
    // { status: "ERROR", reason: "..." } on failure.
    let body: serde_json::Value = resp
        .json()
        .await
        .context("parsing LNURL-pay invoice response")?;
    if let Some("ERROR") = body.get("status").and_then(|s| s.as_str()) {
        let reason = body
            .get("reason")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown");
        bail!("LNURL-pay invoice error: {reason}");
    }
    let parsed: LnurlPayInvoice = serde_json::from_value(body)
        .context("LNURL-pay response missing 'pr' field")?;
    Ok(parsed.pr)
}
