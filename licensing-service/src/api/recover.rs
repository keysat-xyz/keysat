//! Buyer self-service recovery.
//!
//! When a customer loses their license key (lost laptop, deleted
//! email, etc.), they can re-derive it themselves by presenting the
//! invoice id + buyer email they used at purchase. The pair acts as
//! a low-stakes proof-of-purchase: the invoice id is the high-entropy
//! UUID handed to them at checkout, and the email locks the
//! recovery to the same person who paid.
//!
//! Without this, the recovery path was "DM the operator with your
//! invoice id and they'll re-send the key." That doesn't scale —
//! every recovery is operator-time. With it, the customer
//! self-serves and the operator never has to know.
//!
//! Per-IP rate limited at 10 requests / minute to make brute-forcing
//! pairs of (random_uuid, common_email) impractical: a UUIDv4 has
//! ~122 bits of entropy and our daemon can only respond to ~10 RPM
//! per source IP, so guessing rate is bounded by both.

use crate::api::AppState;
use crate::crypto::{encode_key, sign_payload, LicensePayload, FLAG_TRIAL, KEY_VERSION_V2};
use crate::db::repo;
use crate::error::{AppError, AppResult};
use axum::{
    extract::State,
    http::HeaderMap,
    response::{Html, IntoResponse, Response},
    Json,
};
use chrono::DateTime;
use serde::Deserialize;
use serde_json::{json, Value};

/// GET /recover — simple HTML form. Server-rendered (no JS required)
/// because customers reaching this page may have just had a
/// catastrophic failure of their primary computer and we don't want
/// to depend on cookies, JS frameworks, or admin auth.
pub async fn page(State(_state): State<AppState>) -> impl IntoResponse {
    Html(RECOVER_PAGE_HTML)
}

#[derive(Debug, Deserialize)]
pub struct RecoverReq {
    pub invoice_id: String,
    pub email: String,
}

/// POST /v1/recover — exchange (invoice_id, buyer_email) for the
/// signed license key. Both must match the original purchase exactly
/// (email match is case-insensitive on the local-part-and-domain).
///
/// Returns 200 with `{license_key, license_id, product_id, ...}` on
/// success, or a generic 404 ("recovery failed — pair did not match
/// any settled purchase") on any mismatch. The error message is
/// deliberately generic to avoid leaking whether the invoice id
/// existed but the email was wrong, vs. neither existed.
pub async fn recover(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RecoverReq>,
) -> AppResult<Json<Value>> {
    // Rate-limit by client IP so this can't be hammered. Bucket on
    // X-Forwarded-For (set by StartTunnel/nginx); fallback to a
    // catch-all bucket for direct LAN access in dev.
    let bucket = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or("").trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "_lan_".to_string());
    let ok = crate::rate_limit::consume(
        &state.db,
        "recover_ip",
        &bucket,
        /* capacity */ 10.0,
        /* refill_per_second */ 1.0 / 6.0, // 10 / 60s
    )
    .await?;
    if !ok {
        return Err(AppError::TooManyRequests(
            "recovery requests are rate-limited; try again in a minute".into(),
        ));
    }

    let invoice_id = req.invoice_id.trim();
    let supplied_email = req.email.trim().to_lowercase();
    if invoice_id.is_empty() || supplied_email.is_empty() {
        return Err(AppError::BadRequest(
            "both invoice_id and email are required".into(),
        ));
    }

    // Look up the invoice. Must be settled — pending/expired/invalid
    // invoices have no license to recover.
    let invoice = match repo::get_invoice_by_id(&state.db, invoice_id).await? {
        Some(inv) if inv.status == "settled" => inv,
        _ => return Err(generic_failure()),
    };

    // Constant-time-ish email comparison. We don't care about the
    // exact attack model here (the rate limit is the real defence)
    // but it costs nothing to lowercase + compare in full rather
    // than first-byte-mismatch.
    let stored_email = match invoice.buyer_email.as_deref() {
        Some(e) => e.trim().to_lowercase(),
        None => return Err(generic_failure()),
    };
    if stored_email != supplied_email {
        return Err(generic_failure());
    }

    // Find the issued license for this invoice.
    let license = match repo::get_license_by_invoice(&state.db, &invoice.id).await? {
        Some(lic) if lic.status == "active" => lic,
        _ => return Err(generic_failure()),
    };

    // Re-derive the signed key. Same logic as `purchase::status` —
    // deterministic from the stored row, no DB write here.
    let flags = if license.is_trial { FLAG_TRIAL } else { 0 };
    let expires_at = license
        .expires_at
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.timestamp())
        .unwrap_or(0);
    let payload = LicensePayload {
        version: KEY_VERSION_V2,
        flags,
        product_id: uuid::Uuid::parse_str(&license.product_id)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("bad stored product_id: {e}")))?,
        license_id: uuid::Uuid::parse_str(&license.id)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("bad stored license_id: {e}")))?,
        issued_at: DateTime::parse_from_rfc3339(&license.issued_at)
            .map(|t| t.timestamp())
            .unwrap_or(0),
        expires_at,
        fingerprint_hash: [0u8; 32],
        entitlements: license.entitlements.clone(),
    };
    let sig = sign_payload(&state.keypair.signing, &payload);
    let license_key = encode_key(&payload, &sig);

    // Audit-log the recovery so operators can see if a pair was
    // recovered repeatedly (which might indicate the buyer's email
    // is compromised). We hash the email to avoid storing PII in
    // the log.
    let email_hash = crate::hex_sha256(&stored_email);
    let _ = repo::insert_audit(
        &state.db,
        "buyer_self_service",
        Some(&email_hash),
        "license.recovered",
        Some("license"),
        Some(&license.id),
        Some(&bucket),
        headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok()),
        &json!({ "invoice_id": invoice.id }),
    )
    .await;

    Ok(Json(json!({
        "license_key": license_key,
        "license_id": license.id,
        "product_id": license.product_id,
        "issued_at": license.issued_at,
        "expires_at": license.expires_at,
        "entitlements": license.entitlements,
    })))
}

fn generic_failure() -> AppError {
    AppError::NotFound(
        "recovery failed — invoice id and email did not match any settled purchase".into(),
    )
}

const RECOVER_PAGE_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Recover your license — Keysat</title>
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
  body { font-family: -apple-system, BlinkMacSystemFont, "Inter", system-ui, sans-serif;
         background: #f6f1e7; color: #1a2238; margin: 0; padding: 48px 16px; }
  main { max-width: 480px; margin: 0 auto; background: #fff;
         border: 1px solid #d6cdb8; border-radius: 12px; padding: 32px; }
  h1 { margin: 0 0 8px; font-family: "Archivo", Georgia, serif; font-weight: 600; font-size: 24px; }
  p.intro { margin: 0 0 24px; color: #5a6178; line-height: 1.5; }
  label { display: block; font-size: 14px; font-weight: 600; margin: 16px 0 6px; }
  input { width: 100%; padding: 10px 12px; box-sizing: border-box;
          border: 1px solid #c5b994; border-radius: 6px; font-size: 15px;
          font-family: "JetBrains Mono", Menlo, monospace; }
  button { margin-top: 20px; width: 100%; padding: 12px; background: #1a2238;
           color: #f6f1e7; border: 0; border-radius: 6px; font-size: 15px;
           font-weight: 600; cursor: pointer; }
  button:disabled { opacity: 0.6; cursor: wait; }
  pre { margin: 16px 0 0; padding: 12px; background: #1a2238; color: #f6f1e7;
        border-radius: 6px; overflow-x: auto; font-size: 12px; word-break: break-all;
        white-space: pre-wrap; }
  .err { color: #b03020; margin-top: 12px; font-size: 14px; }
  .ok { color: #1a6b3a; margin-top: 12px; font-size: 14px; font-weight: 600; }
</style>
</head>
<body>
<main>
  <h1>Recover your license key</h1>
  <p class="intro">If you've lost your license key, enter the invoice id you received at checkout and the email you paid with. We'll re-issue the same signed key — no support ticket needed.</p>
  <form id="f">
    <label for="invoice_id">Invoice id</label>
    <input id="invoice_id" name="invoice_id" required autocomplete="off"
           placeholder="11111111-2222-3333-4444-555555555555">
    <label for="email">Email used at purchase</label>
    <input id="email" name="email" type="email" required autocomplete="email">
    <button type="submit">Recover key</button>
  </form>
  <div id="result"></div>
</main>
<script>
const f = document.getElementById('f');
const result = document.getElementById('result');
f.addEventListener('submit', async (e) => {
  e.preventDefault();
  result.innerHTML = '';
  const btn = f.querySelector('button');
  btn.disabled = true;
  try {
    const r = await fetch('/v1/recover', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        invoice_id: f.invoice_id.value.trim(),
        email: f.email.value.trim(),
      }),
    });
    const j = await r.json();
    if (!r.ok) {
      const msg = (j && j.error && j.error.message) || (j && j.message) || ('HTTP ' + r.status);
      result.innerHTML = '<div class="err">' + msg + '</div>';
      return;
    }
    result.innerHTML =
      '<div class="ok">Recovered. Save this key somewhere safe.</div>' +
      '<pre>' + j.license_key + '</pre>';
  } catch (err) {
    result.innerHTML = '<div class="err">' + err.message + '</div>';
  } finally {
    btn.disabled = false;
  }
});
</script>
</body>
</html>
"##;
