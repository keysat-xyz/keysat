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
    extract::{Path, Query, State},
    http::StatusCode,
    response::Html,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct BuyPageQuery {
    /// Optional tier slug (deep-link support). Pre-selects a tier when
    /// the buyer arrives from a tier-specific marketing CTA.
    #[serde(default)]
    pub policy: Option<String>,
}

pub async fn render(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Query(q): Query<BuyPageQuery>,
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

    // Tiered pricing: fetch active+public policies for this product. Sorted
    // by price ascending. Used to (a) decide whether to render the tier
    // picker (≥ 2 tiers), and (b) compute the displayed price for the
    // initially-selected tier.
    let public_policies = repo::list_public_policies_by_product(&state.db, &product.id)
        .await
        .unwrap_or_default();

    // Determine the initial selection: ?policy=<slug> deep-link wins, then
    // any policy marked metadata.highlight=true, then the first (cheapest)
    // policy, then None (single-price view).
    let initial_policy = if let Some(want) = q.policy.as_deref() {
        public_policies.iter().find(|p| p.slug == want).cloned()
    } else {
        None
    }
    .or_else(|| {
        public_policies
            .iter()
            .find(|p| {
                p.metadata
                    .get("highlight")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            })
            .cloned()
    })
    .or_else(|| public_policies.first().cloned());

    // The price displayed in the cert card on initial render.
    // For SAT-currency products this is straightforward — show
    // the sat amount. For USD/EUR-priced products we render the
    // listed amount (e.g. "$49.00") with the unit cell switched
    // to the currency code instead of "sats". The tier picker
    // (when multiple policies are public) currently still
    // formats per-tier prices as sats; that's a v0.3 polish
    // when we plumb the rate fetcher into the JS render path.
    let is_fiat = product.price_currency != "SAT";
    let displayed_price_sats = initial_policy
        .as_ref()
        .and_then(|p| p.price_sats_override)
        .unwrap_or(product.price_sats);
    let (price_sats_fmt, price_unit_label) = if is_fiat {
        // price_value is in cents (USD/EUR). Render as e.g. "49.00"
        // for $49.00 — the symbol/code goes in the unit cell.
        let cents = product.price_value;
        let formatted = format!("{}.{:02}", cents / 100, (cents.abs() % 100));
        let unit = match product.price_currency.as_str() {
            "USD" => "USD".to_string(),
            "EUR" => "EUR".to_string(),
            other => other.to_string(),
        };
        (formatted, unit)
    } else {
        (format_thousands(displayed_price_sats), "sats".to_string())
    };
    let _ = displayed_price_sats; // unused on the fiat path
    let initial_policy_slug = initial_policy
        .as_ref()
        .map(|p| p.slug.clone())
        .unwrap_or_default();

    // Look up applicable featured (launch-special) discounts per
    // policy. The tier picker renders the ribbon + slashed price for
    // any policy with a match. Sequential because policy count is
    // small per product.
    let mut featured_by_policy: std::collections::HashMap<String, crate::models::DiscountCode> =
        std::collections::HashMap::new();
    for p in &public_policies {
        if let Ok(Some(code)) =
            repo::find_applicable_featured_discount(&state.db, &product.id, &p.id).await
        {
            featured_by_policy.insert(p.id.clone(), code);
        }
    }

    // Server-render the tier picker HTML so the page is functional even
    // before JS runs. The picker only appears when the product has 2+
    // public policies; otherwise the existing single-price view is used.
    let tiers_html = render_tier_picker(
        &public_policies,
        &initial_policy,
        &product,
        &featured_by_policy,
    );
    // Compact JSON map of {policy_slug: {price, name}} so the JS can update
    // the price card when the buyer clicks a different tier.
    let tiers_json = build_tiers_json(&public_policies, &product);

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
  max-width:1040px; margin:0 auto;
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
/* Outer container width — was 560px (single-column friendly), now
   wider so the 3-tier grid below has room to breathe and matches the
   admin Policies page layout. Inner text + form blocks are constrained
   back to ~560px reading width by the `.wrap > :not(.tiers)` rule
   below so only the tier grid breaks out. */
.wrap {{ max-width:1040px; margin:48px auto; padding:0 24px; }}
.wrap > :not(.tiers) {{ max-width:560px; margin-left:auto; margin-right:auto; }}
.eyebrow {{
  font-size:11.5px; font-weight:700; letter-spacing:0.18em;
  text-transform:uppercase; color:var(--gold-700); margin-bottom:14px;
  /* `flex; width:fit-content` instead of `inline-flex` so the
     wrap-children margin:auto centering rule applies — otherwise
     this inline element would sit flush left of the wider 1040px
     container while its centered block-level siblings sit middle. */
  display:flex; width:fit-content; align-items:center; gap:10px;
}}
.eyebrow::before {{ content:''; display:inline-block; width:28px; height:1px; background:var(--gold-500); }}
h1 {{
  font-family:var(--font-display); font-weight:500;
  /* Fluid type — scales smoothly from ~28px on phones to 42px on
     desktop without an extra breakpoint. min/max guard the
     edges so it never gets too small or too large. */
  font-size:clamp(28px, 7vw, 42px);
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

/* Tier picker — shown when product has 2+ public policies. */
.tiers {{
  display:grid; gap:14px; margin:0 0 28px;
}}
.tiers-2 {{ grid-template-columns:repeat(2, 1fr); }}
.tiers-3 {{ grid-template-columns:repeat(3, 1fr); }}
.tiers-4 {{ grid-template-columns:repeat(2, 1fr); }}
@media (max-width:560px) {{
  .tiers-2, .tiers-3, .tiers-4 {{ grid-template-columns:1fr; }}
}}
/* Phone-sized viewports: tighten outer rhythm so the cert + form
   fit without a long preamble of whitespace. The desktop layout
   has 48px top margin and 32px cert padding, both of which feel
   wasteful on a 360-390px wide screen. */
@media (max-width:480px) {{
  .topbar {{ padding:12px 16px; }}
  .topbar .inner {{ font-size:13px; letter-spacing:0.22em; gap:8px; }}
  .topbar .operator {{ font-size:11px; }}
  .wrap {{ margin:24px auto; padding:0 16px; }}
  .cert {{ padding:24px 22px 22px; }}
  .price {{ font-size:30px; }}
  .description {{ font-size:15px; margin:0 0 24px; }}
  .tier {{ padding:18px 16px; }}
  .tier.selected {{ padding:17px 15px; }}
  .tier-name {{ font-size:17px; }}
  .tier-price {{ font-size:24px; }}
  .field input {{ font-size:16px; }}  /* prevent iOS zoom-on-focus */
}}
.tier {{
  position:relative;
  background:var(--cream-50); border:1px solid var(--border-1);
  border-radius:12px; padding:22px 20px 20px;
  display:flex; flex-direction:column; gap:10px;
  cursor:pointer; transition:all 150ms ease;
}}
.tier:hover {{
  border-color:var(--gold-500);
  box-shadow:0 4px 12px rgba(14,31,51,0.08);
  transform:translateY(-1px);
}}
.tier.selected {{
  border-color:var(--gold-500); border-width:2px;
  padding:21px 19px 19px; /* compensate for thicker border */
  background:#fff;
  box-shadow:0 0 0 3px rgba(191,160,104,0.12), 0 8px 16px rgba(14,31,51,0.10);
}}
.tier.highlighted {{ border-color:var(--gold-500); }}
.tier-popular {{
  position:absolute; top:-10px; left:50%; transform:translateX(-50%);
  background:var(--gold-500); color:var(--navy-950);
  font-family:var(--font-body); font-size:10.5px; font-weight:700;
  letter-spacing:0.16em; text-transform:uppercase;
  padding:4px 10px; border-radius:999px;
  white-space:nowrap;
}}
.tier-name {{
  font-family:var(--font-display); font-weight:600; font-size:18px;
  color:var(--navy-950); letter-spacing:-0.01em;
}}
.tier-price {{
  font-family:var(--font-display); font-weight:700; font-size:26px;
  color:var(--navy-950); letter-spacing:-0.02em;
  line-height:1.1;
}}
.tier-price-unit {{
  font-family:var(--font-body); font-size:13px; font-weight:500;
  color:var(--ink-500); margin-left:6px;
}}
.tier-meta {{
  font-size:12px; color:var(--ink-500);
  font-family:var(--font-body); font-weight:500;
}}
.tier-description {{
  font-size:13.5px; line-height:1.45; color:var(--ink-700); margin:0;
}}
/* Launch-special ribbon — diagonal banner anchored to the top-right
   corner of any tier with an active featured discount. Plus the
   strike-through original-price line that renders ABOVE the
   discounted price. */
.tier.has-launch {{ overflow:hidden; }}
.tier-launch-ribbon {{
  position:absolute; top:14px; right:-44px;
  background:var(--gold-500); color:var(--navy-950);
  font-family:var(--font-display); font-weight:700; font-size:10.5px;
  letter-spacing:0.14em; text-transform:uppercase;
  padding:4px 50px; transform:rotate(35deg);
  box-shadow:0 2px 6px rgba(14,31,51,0.15);
  z-index:2;
}}
.tier-launch-meta {{
  font-size:11.5px; color:var(--gold-700); font-weight:600;
  margin-top:4px;
}}
.tier-price-original {{
  font-family:var(--font-display); font-weight:500; font-size:14px;
  color:var(--ink-500); margin-top:4px;
  text-decoration:line-through; text-decoration-color:rgba(14,31,51,0.4);
}}
.tier-price-original-unit {{
  font-size:11.5px; margin-left:4px; color:var(--ink-500);
}}
.tier-entitlements, .tier-bullets {{
  list-style:none; padding:0; margin:6px 0 0;
  font-size:13px; color:var(--ink-700);
}}
.tier-entitlements li, .tier-bullets li {{
  padding:3px 0 3px 18px; position:relative;
}}
.tier-entitlements li::before, .tier-bullets li::before {{
  content:'✓'; position:absolute; left:0; top:3px;
  color:var(--gold-700); font-weight:700;
}}
/* Marketing bullets render above entitlements with a slightly tighter
   top margin so they read as one coherent feature list. */
.tier-bullets + .tier-entitlements {{ margin-top:2px; }}
.tier-select-btn {{
  margin-top:auto;
  padding:8px 12px;
  background:transparent; color:var(--navy-800);
  border:1px solid var(--border-2); border-radius:8px;
  font-family:var(--font-body); font-weight:600; font-size:13px;
  cursor:pointer; transition:all 120ms;
}}
.tier.selected .tier-select-btn {{
  background:var(--navy-800); color:var(--cream-50);
  border-color:var(--navy-800);
}}
.tier:hover .tier-select-btn {{
  border-color:var(--navy-800);
}}

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

  {tiers_html}

  <div class="cert">
    <div class="price-label" id="price-label">Price</div>
    <div class="price" id="price-display">
      <span id="price-strike-line" class="price-strike" style="display:none"></span>
      <span id="price-current">{price_sats_fmt}</span><span class="unit">{price_unit_label}</span>
      <span id="price-discount-tag" class="price-discount-tag" style="display:none"></span>
    </div>
  </div>

  <form id="buy-form">
    <div class="field">
      <label for="email">Email <span style="color:var(--ink-500); font-weight:400">(optional)</span></label>
      <input type="email" id="email" name="email" placeholder="you@example.com">
      <div class="hint">Useful only if you want a buyer reference for lost-key recovery. Skip it to pay anonymously — your license key is shown directly on this site either way.</div>
    </div>
    <div class="field">
      <label for="code">Discount code (optional)</label>
      <div class="code-row">
        <input type="text" id="code" name="code" placeholder="" autocomplete="off">
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
      <strong>Save this somewhere safe.</strong> The license key is signed at issue time and verifies offline.
      <div id="invoice-ref-line" style="margin-top:10px; font-family:var(--font-mono); font-size:12px; color:var(--ink-500); display:none">
        Reference for support: <code id="invoice-ref-id" style="background:var(--cream-200); padding:1px 6px; border-radius:5px; color:var(--ink-700);"></code>
      </div>
    </div>
  </div>
</div>

<footer class="kfooter">
  <span>Powered by <a href="https://keysat.xyz" target="_blank" rel="noopener">Keysat</a> &middot; Bitcoin-native self-hosted software licensing</span>
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
  // TIERS: {{ slug: {{name, price_sats}} }} — server-rendered. Empty if the
  // product has no public policies (single-price view).
  const TIERS = {tiers_json};
  // Initial tier slug (server-determined: ?policy=, then highlighted, then cheapest).
  let selectedPolicy = {initial_policy_json} || null;
  const priceLabel = document.getElementById('price-label');
  const BASE_PRICE_FMT = priceCurrent.textContent;
  // Recompute on tier change so the strike-through baseline tracks the
  // currently-selected tier rather than freezing to the initial render.
  let currentBaseFmt = BASE_PRICE_FMT;
  // Hoisted up here (was previously declared further down) because the
  // on-load `selectTier(selectedPolicy)` call below reads it. Leaving the
  // declaration below the call hits the temporal-dead-zone error and kills
  // every event handler on the page (including the form submit).
  let appliedCode = null; // {{ code, kind, is_free, final_price_sats }}

  function fmtSats(n) {{ return Number(n).toLocaleString('en-US'); }}

  // Render a tier's price in its native currency. SAT → "50,000"
  // (sats unit handled by the surrounding markup); USD/EUR → "49.00"
  // with the symbol baked into the unit cell. For fiat the
  // price_value is in cents (smallest indivisible unit), so we
  // divide by 100 for display.
  function formatTierPrice(tier) {{
    const cur = (tier.price_currency || 'SAT').toUpperCase();
    if (cur === 'SAT') {{
      return {{ amount: fmtSats(tier.price_sats), unit: 'sats', isFree: tier.price_sats === 0 }};
    }}
    const main = (tier.price_value || 0) / 100;
    return {{ amount: main.toFixed(2), unit: cur, isFree: main === 0 }};
  }}

  // Wire up tier-card clicks.
  document.querySelectorAll('.tier').forEach(function(card) {{
    card.addEventListener('click', function(e) {{
      e.preventDefault();
      const slug = card.getAttribute('data-policy-slug');
      if (slug) selectTier(slug);
    }});
  }});

  // On load, sync the price card + CTA to whatever tier was server-pre-selected.
  // Without this, a free tier would render with "0" price and "Pay with Bitcoin"
  // before the buyer interacts, which is wrong.
  if (selectedPolicy && TIERS[selectedPolicy]) {{
    selectTier(selectedPolicy);
  }}

  function selectTier(slug) {{
    if (!TIERS[slug]) return;
    selectedPolicy = slug;
    // Visual update — toggle .selected on cards AND swap the button
    // label so the chosen tier reads "Selected" while the others
    // stay "Select". Buyer gets a clear "yes, this is what's tied
    // to the price card below" signal.
    document.querySelectorAll('.tier').forEach(function(c) {{
      const isMatch = c.getAttribute('data-policy-slug') === slug;
      if (isMatch) c.classList.add('selected');
      else c.classList.remove('selected');
      const btn = c.querySelector('.tier-select-btn');
      if (btn) btn.textContent = isMatch ? 'Selected' : 'Select';
    }});
    // Reset any active discount apply state — a different tier may not
    // honor the same code (server validates again on the next Apply).
    if (appliedCode) {{
      appliedCode = null;
      setStatus(null);
      setPaidButton();
    }}
    // Reflect new base price in the cert card. For fiat-priced
    // products the unit cell ("sats" → "USD" / "EUR") also swaps.
    // Recurring tiers: append a cadence suffix to the unit so the
    // headline price reads "$25 / mo" not just "$25".
    const t = TIERS[slug];
    const fmt = formatTierPrice(t);
    currentBaseFmt = fmt.amount;
    priceStrike.style.display = 'none';
    priceTag.style.display = 'none';
    const unitEl = document.querySelector('.unit');
    let unitText = fmt.unit;
    if (t.is_recurring) {{
      const days = t.renewal_period_days || 0;
      const suffix = days === 7 ? ' / wk'
        : days === 30 ? ' / mo'
        : days === 90 ? ' / qtr'
        : days === 180 ? ' / 6mo'
        : days === 365 ? ' / yr'
        : days > 0 ? (' / ' + days + 'd')
        : '';
      unitText = fmt.unit + suffix;
    }}
    if (unitEl) unitEl.textContent = unitText;
    if (priceLabel) priceLabel.textContent = 'Price · ' + t.name;
    // Trial recurring tier: first cycle is free, daemon issues the
    // license inline. Surface that as a "Start N-day free trial"
    // CTA instead of "Pay with Bitcoin" so the buyer knows they
    // aren't charged today. Renewal copy stays in the price unit
    // suffix ("$25 / mo") so they can still see what happens after.
    if (t.is_recurring && (t.trial_days || 0) > 0) {{
      priceCurrent.textContent = 'FREE';
      if (unitEl) unitEl.textContent = ' for ' + t.trial_days + ' days';
      setTrialButton(t.trial_days);
    }} else if (fmt.isFree) {{
      // Free non-trial tier: "Redeem license".
      priceCurrent.textContent = 'FREE';
      if (unitEl) unitEl.textContent = '';
      setRedeemButton();
    }} else {{
      priceCurrent.textContent = currentBaseFmt;
      setPaidButton();
    }}
  }}

  // (appliedCode hoisted above — see comment near `let currentBaseFmt`.)

  function showError(msg) {{
    errEl.textContent = msg;
    errEl.classList.add('show');
  }}
  function clearError() {{ errEl.classList.remove('show'); }}
  function showLicense(licenseKey, invoiceId) {{
    keyTextEl.textContent = licenseKey;
    if (invoiceId) {{
      const refLine = document.getElementById('invoice-ref-line');
      const refId = document.getElementById('invoice-ref-id');
      if (refLine && refId) {{
        refId.textContent = invoiceId;
        refLine.style.display = 'block';
      }}
    }}
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
    priceCurrent.textContent = currentBaseFmt;
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
  function setTrialButton(days) {{
    btnLabel.textContent = 'Start ' + (days || 7) + '-day free trial';
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
      let url = '/v1/discount-codes/preview?code='
        + encodeURIComponent(code) + '&product=' + encodeURIComponent(PRODUCT_SLUG);
      if (selectedPolicy) url += '&policy_slug=' + encodeURIComponent(selectedPolicy);
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
        policy_slug: selectedPolicy || undefined,
      }}),
    }});
    if (resp.ok) {{
      const j = await resp.json();
      return {{ ok: true, license_key: j.license_key, invoice_id: j.invoice_id }};
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
    if (selectedPolicy) body.policy_slug = selectedPolicy;
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
    // Free-tier shortcut: server issued the license inline (no BTCPay).
    // Show the license card directly instead of redirecting to a 0-sat
    // checkout page.
    if (j.license_key) {{
      showLicense(j.license_key, j.invoice_id);
      return {{ inline: true }};
    }}
    if (!j.checkout_url) throw new Error('No checkout URL returned by server');
    window.location.href = j.checkout_url;
    return {{ inline: false }};
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
        if (r.ok) {{ showLicense(r.license_key, r.invoice_id); return; }}
        // If the server changed its mind, surface the error rather than silently
        // routing to a paid flow that the buyer didn't consent to.
        throw new Error(r.msg || 'Could not redeem free license.');
      }}

      // Slower path (no Apply or non-free code): keep the original try-then-fallthrough.
      if (code) {{
        const r = await tryFreeRedeem(code, email);
        if (r.ok) {{ showLicense(r.license_key, r.invoice_id); return; }}
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
        price_unit_label = price_unit_label,
        tiers_html = tiers_html,
        slug_json = serde_json::to_string(&product.slug).unwrap_or_else(|_| "\"\"".into()),
        tiers_json = tiers_json,
        initial_policy_json = serde_json::to_string(&initial_policy_slug)
            .unwrap_or_else(|_| "\"\"".into()),
    );
    Ok(Html(body))
}

/// Build the server-rendered tier-picker HTML. Returns an empty string
/// when the product has fewer than 2 public policies (i.e., the existing
/// single-price view is sufficient).
fn render_tier_picker(
    policies: &[crate::models::Policy],
    initial: &Option<crate::models::Policy>,
    product: &crate::models::Product,
    featured_by_policy: &std::collections::HashMap<String, crate::models::DiscountCode>,
) -> String {
    if policies.len() < 2 {
        return String::new();
    }
    let n = policies.len().min(4);
    let class_n = match n {
        2 => "tiers-2",
        3 => "tiers-3",
        _ => "tiers-4",
    };
    let cards: Vec<String> = policies
        .iter()
        .map(|p| {
            let name = html_escape(&p.name);
            let slug_attr = html_escape(&p.slug);
            // For SAT-currency products, the override is in sats; for
            // fiat-priced products it's in cents (USD/EUR). The price
            // unit cell renders in the right denomination either way.
            let base_price_units: i64 = if product.price_currency == "SAT" {
                p.price_sats_override.unwrap_or(product.price_sats)
            } else {
                p.price_sats_override.unwrap_or(product.price_value)
            };
            // Featured discount (if any) — apply the same math the
            // purchase endpoint uses so the buyer sees the same number
            // here as at checkout. Note: for fiat products the units
            // are cents, but compute_discount is currency-agnostic
            // (works on any positive integer).
            let featured = featured_by_policy.get(&p.id);
            let discount_units = featured
                .map(|c| crate::api::purchase::compute_discount(&c.kind, c.amount, base_price_units))
                .unwrap_or(0);
            let post_discount_units = (base_price_units - discount_units).max(0);
            let (price_fmt, price_unit) = if product.price_currency == "SAT" {
                (format_thousands(post_discount_units), "sats".to_string())
            } else {
                let cents = post_discount_units;
                let main = format!("{}.{:02}", cents / 100, (cents.abs() % 100));
                (main, product.price_currency.clone())
            };
            // Original (pre-discount) price for the strikethrough.
            let original_fmt = if featured.is_some() {
                if product.price_currency == "SAT" {
                    format_thousands(base_price_units)
                } else {
                    format!("{}.{:02}", base_price_units / 100, (base_price_units.abs() % 100))
                }
            } else {
                String::new()
            };
            // Ribbon + slashed-original-price markup. Only emitted when
            // a featured discount actually applies.
            let (featured_ribbon, original_price_html) = if let Some(code) = featured {
                let tagline = if code.kind == "percent" {
                    format!("{}% OFF", code.amount / 100)
                } else if code.kind == "free_license" {
                    "FREE".to_string()
                } else if code.kind == "set_price" {
                    "LIMITED PRICE".to_string()
                } else {
                    "LAUNCH SPECIAL".to_string()
                };
                let remaining = code.max_uses.map(|m| (m - code.used_count).max(0)).unwrap_or(-1);
                let remaining_html = if remaining > 0 {
                    format!(
                        "<div class=\"tier-launch-meta\">Limited: {} of {} remaining</div>",
                        remaining,
                        code.max_uses.unwrap_or(0)
                    )
                } else {
                    String::new()
                };
                (
                    format!(
                        "<div class=\"tier-launch-ribbon\">{}</div>{}",
                        html_escape(&tagline),
                        remaining_html,
                    ),
                    format!(
                        "<div class=\"tier-price-original\">{}<span class=\"tier-price-original-unit\">{}</span></div>",
                        original_fmt,
                        if product.price_currency == "SAT" { "sats" } else { product.price_currency.as_str() },
                    ),
                )
            } else {
                (String::new(), String::new())
            };
            let description = p
                .metadata
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let description_html = if description.is_empty() {
                String::new()
            } else {
                format!(
                    "<p class=\"tier-description\">{}</p>",
                    html_escape(description)
                )
            };
            let highlighted = p
                .metadata
                .get("highlight")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let selected = initial
                .as_ref()
                .map(|ip| ip.slug == p.slug)
                .unwrap_or(false);
            // If the product has an entitlements catalog, render
            // each policy entitlement using the catalog's display
            // name + description (as a tooltip). Falls back to the
            // Marketing bullets — operator-controlled copy from
            // metadata.marketing_bullets. Rendered as ✓ checkmarks
            // above (default) or below the entitlement bullets based
            // on metadata.marketing_bullets_position. Skipped silently
            // if absent / wrong shape.
            let bullets_below = p
                .metadata
                .get("marketing_bullets_position")
                .and_then(|v| v.as_str())
                .map(|s| s == "below")
                .unwrap_or(false);
            let marketing_html = p
                .metadata
                .get("marketing_bullets")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    let lis: Vec<String> = arr
                        .iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .map(|s| format!("<li>{}</li>", html_escape(s)))
                        .collect();
                    if lis.is_empty() {
                        String::new()
                    } else {
                        format!("<ul class=\"tier-bullets\">{}</ul>", lis.join(""))
                    }
                })
                .unwrap_or_default();
            // raw slug if the catalog is empty or the slug isn't in
            // it (legacy slugs that predate the catalog land here).
            let entitlements_html = if p.entitlements.is_empty() {
                String::new()
            } else {
                let catalog = product.entitlements_catalog.as_deref().unwrap_or(&[]);
                let lis: Vec<String> = p
                    .entitlements
                    .iter()
                    .map(|slug| {
                        let entry = catalog.iter().find(|e| &e.slug == slug);
                        let display = entry
                            .map(|e| if e.name.trim().is_empty() { e.slug.as_str() } else { e.name.as_str() })
                            .unwrap_or(slug.as_str());
                        let title_attr = entry
                            .map(|e| e.description.as_str())
                            .filter(|s| !s.is_empty())
                            .map(|d| format!(" title=\"{}\"", html_escape(d)))
                            .unwrap_or_default();
                        format!(
                            "<li{}>{}</li>",
                            title_attr,
                            html_escape(display),
                        )
                    })
                    .collect();
                format!("<ul class=\"tier-entitlements\">{}</ul>", lis.join(""))
            };
            let dur_html = if p.duration_seconds > 0 {
                let days = p.duration_seconds / 86_400;
                if days > 0 {
                    format!("<div class=\"tier-meta\">{} days</div>", days)
                } else {
                    let hours = p.duration_seconds / 3600;
                    format!("<div class=\"tier-meta\">{} hours</div>", hours.max(1))
                }
            } else {
                "<div class=\"tier-meta\">Perpetual</div>".to_string()
            };
            let mut classes = String::from("tier");
            if selected {
                classes.push_str(" selected");
            }
            if highlighted {
                classes.push_str(" highlighted");
            }
            let popular_pill = if highlighted {
                "<div class=\"tier-popular\">Most popular</div>"
            } else {
                ""
            };
            let trial_meta = if p.is_trial {
                "<div class=\"tier-meta\" style=\"color:var(--gold-700); font-weight:600\">Trial</div>".to_string()
            } else {
                String::new()
            };
            // Recurring-subscription cadence rendering:
            //   - Tier card shows "Renews every N days" / "monthly" / "annually" beneath duration.
            //   - The price unit gets a "/mo" / "/yr" / "/Nd" suffix so the headline price
            //     reads as a subscription rate, not a one-time cost.
            //   - First-cycle trial banner shows when trial_days > 0.
            let (cadence_suffix, recurring_meta, trial_banner) = if p.is_recurring {
                let days = p.renewal_period_days.max(0);
                let (suffix, label) = match days {
                    7 => ("/wk", "Renews weekly".to_string()),
                    30 => ("/mo", "Renews monthly".to_string()),
                    90 => ("/qtr", "Renews quarterly".to_string()),
                    180 => ("/6mo", "Renews semi-annually".to_string()),
                    365 => ("/yr", "Renews annually".to_string()),
                    other => (
                        // Static lifetime suffix for non-canonical cadences
                        // (use Box::leak only for predictable known values;
                        // fall back to plain "" + custom meta text).
                        "",
                        format!("Renews every {other} days"),
                    ),
                };
                let trial_banner = if p.trial_days > 0 {
                    format!(
                        "<div class=\"tier-meta\" style=\"color:var(--gold-700); font-weight:600\">{} day free trial</div>",
                        p.trial_days
                    )
                } else {
                    String::new()
                };
                (
                    suffix,
                    format!("<div class=\"tier-meta\">{}</div>", html_escape(&label)),
                    trial_banner,
                )
            } else {
                ("", String::new(), String::new())
            };
            // Add `has-launch` to the card class when a featured
            // discount applies so the CSS can lift the price + draw
            // the diagonal ribbon.
            let classes = if featured.is_some() {
                format!("{} has-launch", classes)
            } else {
                classes.clone()
            };
            // Operator-controlled order: above (default) or below.
            let (first_block, second_block) = if bullets_below {
                (&entitlements_html, &marketing_html)
            } else {
                (&marketing_html, &entitlements_html)
            };
            format!(
                r#"<div class="{classes}" data-policy-slug="{slug}">{popular_pill}{featured_ribbon}<div class="tier-name">{name}</div>{original_price_html}<div class="tier-price">{price_fmt}<span class="tier-price-unit">{price_unit}{cadence_suffix}</span></div>{dur_html}{recurring_meta}{trial_banner}{trial_meta}{description_html}{first_block}{second_block}<button type="button" class="tier-select-btn">Select</button></div>"#,
                classes = classes,
                slug = slug_attr,
                popular_pill = popular_pill,
                featured_ribbon = featured_ribbon,
                name = name,
                original_price_html = original_price_html,
                price_fmt = price_fmt,
                price_unit = price_unit,
                cadence_suffix = cadence_suffix,
                dur_html = dur_html,
                recurring_meta = recurring_meta,
                trial_banner = trial_banner,
                trial_meta = trial_meta,
                description_html = description_html,
                first_block = first_block,
                second_block = second_block,
            )
        })
        .collect();
    format!(
        "<div class=\"tiers {n_cls}\">{cards}</div>",
        n_cls = class_n,
        cards = cards.join("")
    )
}

/// Build the JS-side TIERS map that the buy page uses to update the price
/// card and submit the right `policy_slug`. Empty object when no public
/// policies exist (script falls back to product price unchanged).
fn build_tiers_json(
    policies: &[crate::models::Policy],
    product: &crate::models::Product,
) -> String {
    // Each tier carries enough info for the JS to render its price
    // in the right unit. For SAT-currency products, `price_sats`
    // (legacy field) and `price_value` are equal; for fiat-priced
    // products, `price_sats` is a stale snapshot or 0 and the JS
    // uses (price_currency, price_value) as the source of truth.
    //
    // Per-policy currency override (price_currency_override) is
    // wired in for v0.3 — for now policies inherit the product's
    // currency. The JS handles both cases via fallback to the
    // product-level fields embedded in the page.
    let mut map = serde_json::Map::new();
    for p in policies {
        let price_sats_value = p.price_sats_override.unwrap_or(product.price_sats);
        // For fiat-priced products with a sat override on the
        // policy, that override is in the product's currency unit
        // (cents for USD/EUR). Most operators leave the override
        // unset; the inheritance path covers the common case.
        let price_value = p.price_sats_override.unwrap_or(product.price_value);
        map.insert(
            p.slug.clone(),
            serde_json::json!({
                "name": p.name,
                "price_sats": price_sats_value,
                "price_currency": product.price_currency,
                "price_value": price_value,
                "is_recurring": p.is_recurring,
                "renewal_period_days": p.renewal_period_days,
                "trial_days": p.trial_days,
            }),
        );
    }
    serde_json::to_string(&serde_json::Value::Object(map)).unwrap_or_else(|_| "{}".to_string())
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
