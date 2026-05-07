//! Public buyer-facing purchase page at `GET /buy/:slug`.
//!
//! The flow is:
//!   1. Buyer hits `https://<operator-keysat>/buy/<product-slug>` in a browser.
//!   2. We look up the product, render an HTML page showing what they're
//!      buying — name, description, price — plus a small form for an
//!      optional email (for receipt + license delivery) and an optional
//!      discount code.
//!   3. They click "Pay with Bitcoin." Inline JS POSTs to `/v1/purchase`,
//!      gets back a BTCPay checkout URL, redirects the browser there.
//!   4. After payment, BTCPay redirects to `/thank-you` (existing handler).
//!
//! Visual language matches the rest of the Keysat design system: navy
//! topbar, cream paper-textured background, gold accent on the price and
//! the CTA, classical type. Inlined CSS so this single file is the whole
//! buyer-facing surface — easy to deploy, no asset hosting required.

use crate::api::AppState;
use crate::db::repo;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Html,
};

pub async fn render(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Html<String>, (StatusCode, Html<String>)> {
    // Look up the product. Inactive or missing → 404 with a friendly page.
    let product = match repo::get_product_by_slug(&state.db, &slug).await {
        Ok(Some(p)) if p.active => p,
        _ => return Err((StatusCode::NOT_FOUND, Html(not_found_html(&slug)))),
    };

    // Live-read operator name (same pattern as thank-you / root).
    let live = repo::settings_get(&state.db, crate::api::admin::SETTING_OPERATOR_NAME)
        .await
        .ok()
        .flatten();
    let operator_str = live
        .as_deref()
        .or(state.config.operator_name.as_deref())
        .unwrap_or("Keysat");
    let operator = html_escape(operator_str);

    let product_name = html_escape(&product.name);
    let product_slug = html_escape(&product.slug);
    let product_description = html_escape(&product.description);
    let price_sats_fmt = format_thousands(product.price_sats);

    let body = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Buy {product_name} — {operator}</title>
<link href="https://fonts.googleapis.com/css2?family=Manrope:wght@400;500;600;700&family=Inter:wght@400;500;600;700&family=JetBrains+Mono:wght@400;500;600&display=swap" rel="stylesheet">
<style>
:root {{
  --navy-950:#0E1F33; --navy-900:#142A47; --navy-800:#1E3A5F;
  --cream-50:#FBF9F2; --cream-100:#F5F1E8; --cream-200:#EDE7D7;
  --gold-700:#8A6F3D; --gold-500:#BFA068; --gold-400:#D4B985;
  --ink-900:#0E1F33; --ink-700:#2C3E54; --ink-500:#5A6B7F;
  --success:#2D7A5F; --success-bg:#E3F0EA;
  --danger:#B23A3A; --danger-bg:#F4E0E0;
  --border-1:rgba(14,31,51,0.12);
  --border-2:rgba(14,31,51,0.20);
  --font-display:'Manrope','Helvetica Neue',Arial,sans-serif;
  --font-body:'Inter','Helvetica Neue',Arial,sans-serif;
  --font-mono:'JetBrains Mono',ui-monospace,'SF Mono',Menlo,monospace;
  --shadow-md:0 2px 4px rgba(14,31,51,0.06),0 4px 12px rgba(14,31,51,0.06);
}}
*{{box-sizing:border-box}} html,body{{margin:0;padding:0}}
body {{
  font-family:var(--font-body); color:var(--ink-900);
  background:var(--cream-100);
  background-image:
    radial-gradient(rgba(14,31,51,0.025) 1px, transparent 1px),
    radial-gradient(rgba(138,111,61,0.022) 1px, transparent 1px);
  background-size:3px 3px, 7px 7px;
  -webkit-font-smoothing:antialiased; min-height:100vh;
}}
.topbar {{
  background:rgba(245,241,232,0.85); backdrop-filter:blur(10px);
  border-bottom:1px solid var(--border-1);
  padding:14px 24px;
}}
.topbar .inner {{
  max-width:680px; margin:0 auto;
  display:flex; align-items:center; gap:12px;
  font-family:var(--font-display); font-weight:500; font-size:14px;
  letter-spacing:0.28em; text-transform:uppercase; color:var(--navy-900);
}}
.topbar .operator {{
  font-family:var(--font-body); font-size:12px;
  letter-spacing:0.04em; text-transform:none;
  color:var(--ink-500);
  margin-left:auto;
}}
.wrap {{ max-width:560px; margin:48px auto; padding:0 24px; }}
.eyebrow {{
  font-size:11.5px; font-weight:700; letter-spacing:0.18em;
  text-transform:uppercase; color:var(--gold-700); margin-bottom:14px;
  display:inline-flex; align-items:center; gap:10px;
}}
.eyebrow::before {{ content:''; display:inline-block; width:28px; height:1px; background:var(--gold-500); }}
h1 {{
  font-family:var(--font-display); font-weight:500; font-size:42px;
  line-height:1.05; letter-spacing:-0.022em; color:var(--navy-950);
  margin:0 0 12px;
}}
.product-slug {{
  font-family:var(--font-mono); font-size:12.5px; color:var(--ink-500);
  margin:0 0 18px;
}}
.description {{
  font-size:16px; line-height:1.55; color:var(--ink-700);
  margin:0 0 32px;
}}
.cert {{
  background:var(--cream-50); border:1px solid var(--border-1);
  border-radius:14px;
  box-shadow:0 0 0 1px var(--gold-500) inset, var(--shadow-md);
  padding:32px 32px 28px;
  position:relative;
  margin-bottom:24px;
}}
.cert::before, .cert::after {{
  content:''; position:absolute; left:14px; right:14px;
  height:1px; background:var(--gold-500); opacity:0.5;
}}
.cert::before {{ top:14px; }} .cert::after {{ bottom:14px; }}
.price {{
  font-family:var(--font-display); font-weight:700; font-size:36px;
  color:var(--navy-950); letter-spacing:-0.025em; margin:8px 0 0;
}}
.price .unit {{
  font-family:var(--font-body); font-size:15px; font-weight:600;
  color:var(--ink-500); margin-left:8px;
}}
.price-label {{
  font-size:11.5px; font-weight:700; letter-spacing:0.14em;
  text-transform:uppercase; color:var(--ink-500);
}}
.field {{ margin-bottom:14px; }}
.field label {{
  display:block; font-size:12.5px; font-weight:600;
  color:var(--ink-700); margin-bottom:6px;
}}
.field input {{
  width:100%; padding:11px 13px;
  font-family:var(--font-body); font-size:14px;
  border:1px solid var(--border-2); border-radius:8px;
  background:#fff; color:var(--ink-900);
}}
.field input:focus {{
  outline:none; border-color:var(--navy-700);
  box-shadow:0 0 0 3px rgba(30,58,95,0.18);
}}
.field .hint {{ font-size:12px; color:var(--ink-500); margin-top:5px; }}

/* Apply-discount cluster: input + button on one row */
.code-row {{ display:flex; gap:8px; align-items:stretch; }}
.code-row input {{ flex:1; }}
.btn-apply {{
  background:transparent; color:var(--navy-800);
  border:1px solid var(--border-2); border-radius:8px;
  padding:0 16px;
  font-family:var(--font-body); font-weight:600; font-size:13px;
  cursor:pointer; transition:all 120ms;
  flex-shrink:0;
}}
.btn-apply:hover {{ background:var(--cream-200); border-color:var(--navy-700); }}
.btn-apply:disabled {{ opacity:0.5; cursor:wait; }}
.code-status {{
  margin-top:8px; font-size:13px; padding:8px 12px;
  border-radius:7px; display:none;
}}
.code-status.show {{ display:block; }}
.code-status.ok {{ background:var(--success-bg); color:#205c47; border:1px solid rgba(45,122,95,0.25); }}
.code-status.bad {{ background:var(--danger-bg); color:#8a2828; border:1px solid rgba(178,58,58,0.25); }}

/* Price card update animation when discount applied */
.price-strike {{
  text-decoration:line-through; color:var(--ink-500);
  font-size:18px; font-weight:500; display:block;
  margin-bottom:4px;
}}
.price-discount-tag {{
  display:inline-block; margin-left:8px;
  font-family:var(--font-body); font-size:12px; font-weight:600;
  padding:3px 10px; border-radius:999px;
  background:var(--success-bg); color:#205c47;
  border:1px solid rgba(45,122,95,0.25);
  vertical-align:middle;
}}

.btn-pay {{
  width:100%; padding:14px;
  background:var(--navy-800); color:var(--cream-50);
  border:0; border-radius:10px;
  font-family:var(--font-body); font-weight:600; font-size:15px;
  cursor:pointer; transition:background 120ms;
  margin-top:16px;
  display:inline-flex; align-items:center; justify-content:center; gap:8px;
}}
.btn-pay:hover {{ background:var(--navy-900); }}
.btn-pay:disabled {{ opacity:0.6; cursor:wait; }}
.btn-pay svg {{ width:18px; height:18px; }}
.error {{
  margin-top:14px; padding:10px 14px;
  background:var(--danger-bg); color:#8a2828;
  border:1px solid rgba(178,58,58,0.25);
  border-radius:7px; font-size:13.5px;
  display:none;
}}
.error.show {{ display:block; }}
.license-success {{
  display:none; margin-top:24px;
  background:var(--cream-50); border:1px solid var(--border-1);
  border-radius:14px;
  box-shadow:0 0 0 1px var(--gold-500) inset, 0 8px 16px rgba(14,31,51,0.10);
  padding:32px 32px 28px; position:relative;
}}
.license-success.show {{ display:block; }}
.license-success::before, .license-success::after {{
  content:''; position:absolute; left:14px; right:14px;
  height:1px; background:var(--gold-500); opacity:0.5;
}}
.license-success::before {{ top:14px; }}
.license-success::after {{ bottom:14px; }}
.license-success .stamp {{
  font-size:10px; font-weight:700; letter-spacing:0.22em;
  text-transform:uppercase; color:var(--gold-700);
  text-align:center; margin-bottom:16px;
}}
.license-success h3 {{
  font-family:var(--font-display); font-weight:500; font-size:22px;
  color:var(--navy-950); margin:0 0 6px; letter-spacing:-0.015em;
  text-align:center;
}}
.license-success .subtitle {{
  font-size:14px; color:var(--ink-500); text-align:center;
  margin:0 0 22px;
}}
.license-success .field-label {{
  font-size:11px; font-weight:600; letter-spacing:0.12em;
  text-transform:uppercase; color:var(--ink-500); margin-bottom:6px;
}}
.license-success .key-box {{
  background:var(--navy-950); color:var(--cream-50);
  padding:14px 16px; border-radius:8px;
  font-family:var(--font-mono); font-size:12.5px;
  word-break:break-all; line-height:1.5;
  display:flex; align-items:flex-start; gap:12px;
}}
.license-success .key-box .key-text {{ flex:1; }}
.license-success .key-box button {{
  background:rgba(245,241,232,0.10); color:var(--cream-50);
  border:0; padding:6px 10px; border-radius:6px;
  font-family:var(--font-body); font-size:11.5px; cursor:pointer;
  flex-shrink:0;
}}
.license-success .key-box button:hover {{ background:rgba(245,241,232,0.20); }}
.license-success .save-note {{
  margin-top:14px; font-size:13px; color:var(--ink-700);
  background:var(--cream-100); border:1px solid var(--border-1);
  border-radius:8px; padding:10px 14px;
}}
.license-success .save-note strong {{ color:var(--navy-950); }}
footer.kfooter {{
  text-align:center; font-size:12px; color:var(--ink-500);
  margin-top:48px; padding:18px;
}}
footer.kfooter a {{ color:var(--ink-500); text-decoration:none; }}
footer.kfooter a:hover {{ color:var(--navy-900); }}
</style>
</head>
<body>

<div class="topbar">
  <div class="inner">
    <span>Keysat</span>
    <span class="operator">Sold by {operator}</span>
  </div>
</div>

<div class="wrap">
  <div class="eyebrow">Buy a license</div>
  <h1>{product_name}</h1>
  <div class="product-slug">{product_slug}</div>
  <p class="description">{product_description}</p>

  <div class="cert">
    <div class="price-label">Price</div>
    <div class="price" id="price-display">
      <span id="price-strike-line" class="price-strike" style="display:none"></span>
      <span id="price-current">{price_sats_fmt}</span><span class="unit">sats</span>
      <span id="price-discount-tag" class="price-discount-tag" style="display:none"></span>
    </div>
  </div>

  <form id="buy-form">
    <div class="field">
      <label for="email">Email (for receipt &amp; license)</label>
      <input type="email" id="email" name="email" placeholder="you@example.com" required>
      <div class="hint">We&rsquo;ll send your license key here after payment confirms.</div>
    </div>
    <div class="field">
      <label for="code">Discount code (optional)</label>
      <div class="code-row">
        <input type="text" id="code" name="code" placeholder="FOUNDERS50" autocomplete="off">
        <button type="button" class="btn-apply" id="btn-apply">Apply</button>
      </div>
      <div class="code-status" id="code-status" role="status" aria-live="polite"></div>
    </div>
    <button type="submit" class="btn-pay" id="btn-pay">
      <svg id="btn-pay-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
        <circle cx="12" cy="12" r="10"></circle>
        <path d="M9.5 8.5h5a2 2 0 010 4h-5m0 0h5a2 2 0 010 4h-5m0-8v8m2-10v2m0 8v2"></path>
      </svg>
      <span id="btn-pay-label">Pay with Bitcoin</span>
    </button>
    <div class="error" id="err"></div>
  </form>

  <div class="license-success" id="license-success" role="region" aria-label="License issued">
    <div class="stamp">&mdash; License issued &mdash;</div>
    <h3>You&rsquo;re licensed.</h3>
    <p class="subtitle">No payment needed for this code. Your signed license is below.</p>
    <div class="field-label">License key</div>
    <div class="key-box">
      <span class="key-text" id="license-key-text">&hellip;</span>
      <button id="license-key-copy">Copy</button>
    </div>
    <div class="save-note">
      <strong>Save this somewhere safe.</strong> The license key is signed at issue time and verifies offline. We&rsquo;ll also send a copy to <span id="license-email-display"></span> for your records.
    </div>
  </div>
</div>

<footer class="kfooter">
  <span>Powered by <a href="https://keysat.xyz" target="_blank" rel="noopener">Keysat</a> &middot; Bitcoin-paid software licensing</span>
</footer>

<script>
(function() {{
  const form = document.getElementById('buy-form');
  const btn = document.getElementById('btn-pay');
  const btnLabel = document.getElementById('btn-pay-label');
  const btnIcon = document.getElementById('btn-pay-icon');
  const errEl = document.getElementById('err');
  const successEl = document.getElementById('license-success');
  const keyTextEl = document.getElementById('license-key-text');
  const emailDisplayEl = document.getElementById('license-email-display');
  const codeInput = document.getElementById('code');
  const applyBtn = document.getElementById('btn-apply');
  const codeStatus = document.getElementById('code-status');
  const priceCurrent = document.getElementById('price-current');
  const priceStrike = document.getElementById('price-strike-line');
  const priceTag = document.getElementById('price-discount-tag');
  const PRODUCT_SLUG = {slug_json};
  const BASE_PRICE_FMT = priceCurrent.textContent;

  // State of the most recent successful Apply. When set with kind=free_license
  // and the same code is still in the input, the submit handler skips the
  // "try /v1/redeem then fall through" dance and goes straight to redeem.
  let appliedCode = null; // {{ code, kind, is_free, final_price_sats }}

  function showError(msg) {{
    errEl.textContent = msg;
    errEl.classList.add('show');
  }}
  function clearError() {{ errEl.classList.remove('show'); }}
  function showLicense(licenseKey, email) {{
    keyTextEl.textContent = licenseKey;
    emailDisplayEl.textContent = email || '(no email provided)';
    form.style.display = 'none';
    successEl.classList.add('show');
    successEl.scrollIntoView({{ behavior: 'smooth', block: 'center' }});
  }}

  function fmtNum(n) {{
    return Number(n).toLocaleString('en-US');
  }}

  function setStatus(kind, text) {{
    codeStatus.classList.remove('ok', 'bad');
    if (!kind) {{ codeStatus.classList.remove('show'); codeStatus.textContent = ''; return; }}
    codeStatus.classList.add(kind === 'ok' ? 'ok' : 'bad', 'show');
    codeStatus.textContent = text;
  }}

  function resetPrice() {{
    priceCurrent.textContent = BASE_PRICE_FMT;
    priceStrike.style.display = 'none';
    priceStrike.textContent = '';
    priceTag.style.display = 'none';
    priceTag.textContent = '';
  }}
  function setPaidButton() {{
    btnLabel.textContent = 'Pay with Bitcoin';
    btnIcon.style.display = '';
  }}
  function setRedeemButton() {{
    btnLabel.textContent = 'Redeem license';
    btnIcon.style.display = 'none';
  }}

  // Reset apply state if the buyer edits the code after a successful Apply.
  codeInput.addEventListener('input', function() {{
    if (appliedCode && codeInput.value.trim().toUpperCase() !== appliedCode.code.toUpperCase()) {{
      appliedCode = null;
      resetPrice();
      setPaidButton();
      setStatus(null);
    }}
  }});

  applyBtn.addEventListener('click', async function() {{
    clearError();
    const code = codeInput.value.trim();
    if (!code) {{
      setStatus('bad', 'Enter a code first.');
      return;
    }}
    applyBtn.disabled = true;
    const orig = applyBtn.textContent;
    applyBtn.textContent = 'Checking…';
    try {{
      const url = '/v1/discount-codes/preview?code='
        + encodeURIComponent(code) + '&product=' + encodeURIComponent(PRODUCT_SLUG);
      const resp = await fetch(url);
      if (!resp.ok) {{
        let msg = 'HTTP ' + resp.status;
        try {{ const j = await resp.json(); msg = j.message || j.error || msg; }} catch(_) {{}}
        throw new Error(msg);
      }}
      const j = await resp.json();
      if (!j.valid) {{
        appliedCode = null;
        resetPrice();
        setPaidButton();
        setStatus('bad', j.message || 'Code not valid.');
        return;
      }}
      appliedCode = {{
        code: j.code,
        kind: j.kind,
        is_free: !!j.is_free,
        final_price_sats: j.final_price_sats,
      }};
      // Update price card
      if (j.kind === 'free_license' || j.final_price_sats === 0) {{
        priceStrike.textContent = fmtNum(j.base_price_sats) + ' sats';
        priceStrike.style.display = 'block';
        priceCurrent.textContent = 'FREE';
        priceTag.textContent = '100% off';
        priceTag.style.display = 'inline-block';
        setRedeemButton();
      }} else {{
        priceStrike.textContent = fmtNum(j.base_price_sats) + ' sats';
        priceStrike.style.display = 'block';
        priceCurrent.textContent = fmtNum(j.final_price_sats);
        if (j.kind === 'percent') {{
          priceTag.textContent = (j.amount_pct || ((j.discount_applied_sats / j.base_price_sats) * 100).toFixed(0)) + '% off';
        }} else {{
          priceTag.textContent = fmtNum(j.discount_applied_sats) + ' sats off';
        }}
        priceTag.style.display = 'inline-block';
        setPaidButton();
      }}
      setStatus('ok', j.message || 'Code applied.');
    }} catch (err) {{
      appliedCode = null;
      resetPrice();
      setPaidButton();
      setStatus('bad', err.message || 'Could not validate code.');
    }} finally {{
      applyBtn.disabled = false;
      applyBtn.textContent = orig;
    }}
  }});

  // Try free-license redemption first if a code was provided. If that
  // path returns "this code requires payment", fall through to the
  // BTCPay flow with the code applied. Any other error stops here.
  async function tryFreeRedeem(code, email) {{
    const resp = await fetch('/v1/redeem', {{
      method: 'POST',
      headers: {{ 'Content-Type': 'application/json' }},
      body: JSON.stringify({{
        product: PRODUCT_SLUG,
        code,
        buyer_email: email || undefined,
      }}),
    }});
    if (resp.ok) {{
      const j = await resp.json();
      return {{ ok: true, license_key: j.license_key }};
    }}
    let msg = 'HTTP ' + resp.status;
    try {{
      const j = await resp.json();
      msg = j.message || j.error || msg;
    }} catch (_) {{}}
    // Distinguish "fall through to paid flow" from real errors.
    if (resp.status === 400 && /requires payment/i.test(msg)) {{
      return {{ ok: false, fallThrough: true }};
    }}
    return {{ ok: false, fallThrough: false, msg }};
  }}

  async function startPaidPurchase(code, email) {{
    const body = {{ product: PRODUCT_SLUG, buyer_email: email || undefined }};
    if (code) body.code = code;
    const resp = await fetch('/v1/purchase', {{
      method: 'POST',
      headers: {{ 'Content-Type': 'application/json' }},
      body: JSON.stringify(body),
    }});
    if (!resp.ok) {{
      let msg = 'HTTP ' + resp.status;
      try {{
        const j = await resp.json();
        msg = j.message || j.error || msg;
      }} catch (_) {{}}
      throw new Error(msg);
    }}
    const j = await resp.json();
    if (!j.checkout_url) throw new Error('No checkout URL returned by server');
    window.location.href = j.checkout_url;
  }}

  // "Copy" on the license key box.
  document.getElementById('license-key-copy').addEventListener('click', async function() {{
    try {{
      await navigator.clipboard.writeText(keyTextEl.textContent);
      this.textContent = 'Copied';
      setTimeout(() => {{ this.textContent = 'Copy'; }}, 1400);
    }} catch (e) {{}}
  }});

  form.addEventListener('submit', async function(e) {{
    e.preventDefault();
    clearError();
    btn.disabled = true;
    const originalLabel = btnLabel.textContent;
    btnLabel.textContent = 'Working…';

    const email = document.getElementById('email').value.trim();
    const code = codeInput.value.trim();
    const codeMatchesApplied = appliedCode &&
      code.toUpperCase() === appliedCode.code.toUpperCase();

    try {{
      // Fast path: a free_license code was already validated via Apply.
      if (codeMatchesApplied && appliedCode.is_free) {{
        const r = await tryFreeRedeem(code, email);
        if (r.ok) {{ showLicense(r.license_key, email); return; }}
        // If the server changed its mind, surface the error rather than silently
        // routing to a paid flow that the buyer didn't consent to.
        throw new Error(r.msg || 'Could not redeem free license.');
      }}

      // Slower path (no Apply or non-free code): keep the original try-then-fallthrough.
      if (code) {{
        const r = await tryFreeRedeem(code, email);
        if (r.ok) {{ showLicense(r.license_key, email); return; }}
        if (!r.fallThrough) {{
          throw new Error(r.msg || 'Code rejected');
        }}
        // else fall through to the BTCPay path with the code applied
      }}

      btnLabel.textContent = 'Creating invoice…';
      await startPaidPurchase(code, email);
    }} catch (err) {{
      showError('Could not complete: ' + (err.message || err));
      btn.disabled = false;
      btnLabel.textContent = originalLabel;
    }}
  }});
}})();
</script>

</body>
</html>
"#,
        operator = operator,
        product_name = product_name,
        product_slug = product_slug,
        product_description = product_description,
        price_sats_fmt = price_sats_fmt,
        slug_json = serde_json::to_string(&product.slug).unwrap_or_else(|_| "\"\"".into()),
    );
    Ok(Html(body))
}

fn not_found_html(slug: &str) -> String {
    let slug_safe = html_escape(slug);
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Product not found</title>
<style>
body{{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;
  max-width:32rem;margin:4rem auto;padding:0 1.25rem;color:#222;background:#fafafa;line-height:1.55}}
h1{{font-size:1.5rem;margin-top:0}}
code{{background:#eee;padding:0.1em 0.4em;border-radius:4px;font-family:ui-monospace,monospace}}
</style></head>
<body>
<h1>Product not found</h1>
<p>No product is registered under the slug <code>{slug_safe}</code>, or it&rsquo;s currently inactive.</p>
<p>If you arrived here from a link the seller shared, double-check that you&rsquo;ve typed the URL correctly. Otherwise, ask the seller to confirm the product slug.</p>
</body></html>"#,
        slug_safe = slug_safe
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn format_thousands(n: i64) -> String {
    // Renders 50000 as "50,000" — visible price legibility for sat amounts.
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}
