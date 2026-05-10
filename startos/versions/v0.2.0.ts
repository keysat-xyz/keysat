// Draft of the v0.2.0 milestone version entry.
//
// NOT YET WIRED INTO `versions/index.ts` — this file sits ready to
// use when we cut v0.2.0:0 from the alpha-iteration line. To
// activate:
//   1. In `versions/index.ts`:
//        import { v0_2_0 } from './v0.2.0'
//        export const versions = VersionGraph.of({
//          current: v0_2_0,
//          other: [v0_1_0],   // ← so installs on 0.1.0:N can upgrade
//        })
//   2. Build the .s9pk (`make x86`).
//   3. Publish via `~/.keysat/publish.sh` (the version-changed gate
//      will fire because `0.2.0:0` differs from the recorded
//      `0.1.0:N`).
//
// Why this draft exists separately:
// - The cut is an irreversible release decision for already-installed
//   operators (downgrade paths exist in StartOS but they're sticky).
// - Wiring it in changes how StartOS computes the upgrade dialog
//   shown to operators on registry refresh — best to QA the
//   release-notes content in this file before flipping the switch.
// - Lets us write the v0.2.0 release notes carefully and then ship
//   them all at once, rather than amending mid-build.
//
// Version-string format reminder: ExVer is `<upstream>:<downstream>`.
// The `<upstream>` bump from 0.1.0 → 0.2.0 marks the milestone; the
// `:0` resets the downstream revision counter for the new line. The
// next routine wrapper update on the v0.2 line will be `0.2.0:1`,
// then `:2`, etc.

import { VersionInfo } from '@start9labs/start-sdk'

const RELEASE_NOTES = [
  'Keysat v0.2.0 — first non-alpha milestone. Operator-visible: web admin SPA replaces the StartOS Actions tab for day-to-day work, buyer self-service recovery, opt-in community analytics, and the wire format now agrees byte-for-byte across five language SDKs (Rust, TypeScript, Python, Go, plus the daemon itself).',
  '',
  '**The web admin SPA is the headline.** Daily operator work — creating products, configuring policies and discount codes, searching licenses, suspending/revoking, inspecting machines, registering webhook endpoints, browsing the audit log — happens in the embedded dashboard at /admin/. The StartOS Actions tab is intentionally trimmed to setup-time operations only (Connect/Disconnect BTCPay, Set operator name, Set web UI password, Activate Keysat license, Show credentials). No more "wall of buttons" for everyday tasks.',
  '',
  '**Buyer self-service recovery.** A buyer who lost their license key can re-derive it themselves from (invoice_id, buyer_email) at /recover on the daemon\'s public URL. No support ticket, no operator involvement. Per-IP rate limited (10 req/min), generic-404 on mismatch (does not leak which side of the pair was wrong), audit-logged with the email\'s SHA-256 hash so the log doesn\'t store PII.',
  '',
  '**Webhook delivery DLQ.** The outbound-webhook delivery worker has always retried failed deliveries with exponential backoff up to 10 attempts; failed deliveries past that were silent dead-letters. v0.2 surfaces them: `GET /v1/admin/webhook-deliveries?status=failed` lists them, `POST /v1/admin/webhook-deliveries/:id/retry` re-queues. Surfaced in the SPA on the Webhooks page (defaults to the "Failed" filter so the problem case is what an operator sees first).',
  '',
  '**Opt-in community analytics.** Off by default. When enabled (Overview page in the admin UI), the daemon sends a daily anonymous heartbeat: install_uuid (random, not derived from operator identity), daemon version, tier label, and counts (products / active licenses / settled invoices) floored to the nearest 5 to prevent fingerprinting an operator by their exact license count. Uptime is bucketed (<1d / 1-7d / 1-4w / >4w). Operator name, public URL, store id, API keys, buyer email are NEVER sent — and the test suite asserts none of those strings appear in the heartbeat payload.',
  '',
  '**Five-language SDK parity.** The Go SDK (github.com/keysat-xyz/keysat-client-go) lands alongside this release. Stdlib only — no third-party Go dependencies. All five implementations of the LIC1 wire format (daemon, Rust SDK, TypeScript SDK, Python SDK, Go SDK) pass the same crosscheck vectors at tests/crosscheck/vector.json byte-for-byte across v1 legacy, v2 trial-with-entitlements, and v2 perpetual-unbound fixtures.',
  '',
  '**PaymentProvider trait abstraction.** Internally, the four daemon code paths that talked to BTCPay (purchase, webhook, reconcile, tipping) all now go through the abstract PaymentProvider trait. BTCPay-specific concerns (URL rewriting, status-string normalization, metadata enrichment, payment-hash extraction) live inside the BtcpayProvider impl. This unblocks Zaprite (v0.3) — its impl drops in cleanly without touching call sites.',
  '',
  '**Test coverage.** The daemon\'s automated test count grew from ~9 in alpha-iteration :24 to 32 in :47: 9 unit + 12 API integration + 4 SQL migration regression + 4 wire-format crosscheck + 3 webhook-worker integration. Plus the four Go SDK crosscheck tests in the separate Go repo.',
  '',
  '**Upgrade from v0.1.0:N.** Straight drop-in. No new SQLite migrations on the v0.2.0:0 cut itself (those landed individually during the alpha iteration). Existing licenses, invoices, products, policies, and discount codes are untouched. Web UI password, BTCPay connection, operator name, tip-recipient configuration all carry over.',
  '',
  '**What\'s next (v0.3).** Zaprite payment provider for card payments. Recurring subscriptions. In-place tier upgrades for end customers. Multi-currency pricing (USD + sats with auto-conversion at invoice creation).',
].join('\n')

// Routine wrapper-revision changelog. Newest first; each entry is
// what changed since the previous downstream-:N. The `:0` notes are
// in RELEASE_NOTES above (the milestone). Subsequent revisions
// append here.
const ROUTINE_NOTES = [
  '0.2.0:7 — Marketing-copy alignment. Package short and long descriptions now read "Bitcoin-native self-hosted licensing service for software creators" — matches keysat.xyz and the new positioning. Long description also calls out Zaprite (Bitcoin + cards), recurring subscriptions, and tier upgrades, all of which shipped in earlier :N revisions but weren\'t reflected in the registry listing. Same change applied to the daemon Cargo.toml description, repo READMEs, and the in-StartOS About panel for consistency. No code changes; pure copy.',
  '',
  '0.2.0:6 — **Recurring subs + trials + self-tier live refresh actually work now.** Major bug-and-UX-fix release driven by hands-on testing of v0.2.0:5. The recurring-sub feature shipped in :4 had a critical gap: buying a recurring policy issued a license but never created the corresponding subscription row, so the renewal worker never picked it up — purchases silently behaved like one-shots. The trial flow shipped with `trial_days` configurable in admin but the field had zero effect on the purchase path. And admin tier changes on the daemon\'s own license never propagated to the running daemon, making it impossible to test Creator-tier gates on the master Keysat. This release fixes all three plus a slate of UX papercuts found during testing.',
  '',
  '**Recurring purchases now create subscriptions.** `issue_license_for_invoice` calls `subscriptions::create_subscription` whenever the resolved policy has `is_recurring=1`. The Subscriptions tab populates correctly; the renewal worker sees the row; cancellation works. Idempotent against webhook re-delivery.',
  '',
  '**Free trials actually work.** When a buyer hits "Pay with Bitcoin" on a recurring policy with `trial_days > 0`, the daemon now: (a) synthesizes a free invoice via the same shortcut used for free-license-code redemptions, (b) issues a license inline with `expires_at = now + trial_days`, (c) creates the subscription with `next_renewal_at = trial_end` so the renewal worker fires the FIRST paid invoice when the trial ends, (d) returns the license key directly with no checkout step. The buy page CTA flips to "Start N-day free trial" so the buyer knows they\'re not being charged today. Discount codes are intentionally ignored on trial purchases (trial = free; layering a discount is a no-op). Trial license carries the TRIAL flag on the signed payload.',
  '',
  '**Self-tier live refresh.** The daemon\'s own tier (`state.self_tier`) was previously loaded from the on-disk LIC1 key at boot and never refreshed — entitlements baked into the signed payload at signing time were the daemon\'s permanent reality. Now there\'s a `license_self::refresh_self_tier_from_db` helper that re-reads the local `licenses` row and rebuilds `state.self_tier` from LIVE entitlements. Wired to fire (a) once at boot right after `check_at_boot`, (b) every hour as a background task, (c) on demand via `POST /v1/admin/self-license/refresh`. Admin tier changes now propagate. This is the same online-entitlement-refresh pattern any operator should implement in their own app — Keysat dogfoods it for itself.',
  '',
  '**Renewal-pending webhook payload enriched.** `subscription.renewal_pending` now includes `buyer_email`, `product_id`, `policy_id`, `cycle_start_at`, `cycle_end_at`, `due_at`, and `is_first_paid_cycle` so operators\' webhook receivers have everything they need to render and send "your free trial is ending" / "your monthly renewal is due" emails to the buyer with the checkout URL. (Without this, renewal invoices were created server-side but no one knew about them — the buyer had no way to learn they needed to pay.)',
  '',
  '**Admin Change Tier modal redesigned.** The "skip_payment" toggle is gone — admin tier changes always apply as comp from the UI now. Paid tier changes are buyer-initiated via the SDK\'s in-app upgrade flow; admin path is for operators who want to give someone a free upgrade or fix a screwup. Reduces the attack surface of "operator generates invoice, dismisses modal, orphan invoice lives on the provider." The modal also now detects downgrades (target rank or price < current), shows a yellow warning banner listing the entitlements the buyer will lose, and confirms via dialog. The dropdown shows the current tier in disabled state with "(current)" suffix — operators see what they\'re starting from but can\'t pick a no-op.',
  '',
  '**Self-tier guard.** `POST /v1/admin/licenses/<id>/change-tier` now refuses when `<id>` is the daemon\'s own self-license, with a clear error pointing at either the master Keysat\'s re-mint flow or the file-rename trick (`mv /data/keysat-license.txt /data/keysat-license.txt.bak; restart`) for testing Creator-tier gates.',
  '',
  '**Zaprite webhook flow improved.** Connect Zaprite now shows the EXACT `https://your-keysat-url/v1/zaprite/webhook` URL to paste (was a placeholder before, which Zaprite\'s form rejected). New "Show Zaprite webhook setup" StartOS Action surfaces the URL persistently for operators who skipped the step on first connect. Connect-while-already-connected returns 409 Conflict with a clear message instead of overwriting silently (BTCPay already had this guard).',
  '',
  '**Single "Switch active payment provider" StartOS action** replaces the two confusing "Activate BTCPay" / "Activate Zaprite" actions. Dropdown-driven, pre-fills with currently-active provider so opening it is informative.',
  '',
  '**UX polish on the admin dashboard:**',
  '- Policy list duration column is human-readable (`1 year` / `1 week` / `perpetual`) instead of raw seconds (`31536000s`).',
  '- "Preview buy page" button on each product\'s policies card opens `/buy/<slug>` in a new tab.',
  '- Buy page tier cards: clicked button reads "Selected" while others stay "Select" — clearer "this is the active choice" cue.',
  '- Licenses tab POLICY column shows display name primary with slug secondary (was slug-only).',
  '- Thank-you page copy: "Lightning settles in seconds; on-chain typically 10–20 minutes" instead of misleading "next block confirms" for Lightning payments.',
  '',
  '**KEYSAT_INTEGRATION.md adds section 0a "How enforcement actually works"** — the offline-vs-online framing every operator hits when they realize they want to revoke / downgrade / lapse a license. Walks through the two patterns (A: true perpetual, offline-only; B: perpetual price, online-enforced) with TS code samples and the design dials operators pick.',
  '',
  '**Test count: 77** (unchanged). The bug fixes are above the renewal-worker tests\' scope (those tests construct subscriptions explicitly via `create_subscription`, bypassing the broken purchase path); test additions deferred to the v0.3 work that\'ll cover the integration paths properly.',
  '',
  '**Upgrade path.** v0.2.0:5 → v0.2.0:6 is a drop-in. No new schema migrations. No behavior change unless you actively use recurring policies, trials, or admin tier changes — all of which were broken before and now work.',
  '',
  '0.2.0:5 — **In-place tier upgrades are functional end-to-end.** Buyers can self-serve "upgrade to Pro" inside the operator\'s app — they pay only the prorated difference for the time remaining in their current cycle, the existing license keeps its key, and the daemon flips entitlements on next online validation. Operators can force-change any license to any policy from the admin UI, with optional comp-mode (skip the invoice).',
  '',
  '**Buyer flow.** New `POST /v1/upgrade-quote` returns the prorated charge in the listed currency: "Standard $25/mo → Pro $75/mo with 15 days remaining = $25.00 today, $75.00 next cycle." `POST /v1/upgrade` creates a payment provider invoice for the prorated charge and returns a checkout URL. When the invoice settles, the webhook handler flips the license\'s policy_id + entitlements + max_machines + expires_at and any tied subscription\'s policy_id + listed_value + period_days. The signed license key stays the same — the buyer\'s app just sees the new entitlements on its next call to `/v1/validate`.',
  '',
  '**Admin flow.** New `POST /v1/admin/licenses/:id/change-tier` for force-changes. Two modes: `skip_payment: true` applies on the spot for comp upgrades / support fix-ups (no invoice, audit-logged); `skip_payment: false` creates an invoice and returns the checkout URL the operator forwards to the buyer through whatever channel (email, chat, etc.). Bypasses ladder rules — admin can move sideways, downgrade perpetuals, or change to/from policies that aren\'t in any ladder.',
  '',
  '**Tier ladder.** Policies gain a `tier_rank` integer column (NULL = excluded from buyer-facing upgrade flows). Operators set this in the policy editor: free=0, standard=1, pro=2, etc. The buyer endpoint enforces that target.tier_rank > current.tier_rank for upgrades; sideways and reverse moves return 400 "admin-only".',
  '',
  '**Recurring downgrades, scheduled at cycle boundary.** When the admin records a downgrade tier_change with `effective_at = next_renewal_at`, the renewal worker checks for pending changes before pricing the next cycle and applies them in place. This means "downgrade me at end of cycle" actually fires correctly — the next invoice bills at the new (lower) tier, not the old one. Audit-logged with `actor=system`, `applied_via=renewal_worker`.',
  '',
  '**New tables + columns.** Migration 0013 adds `policies.tier_rank` and a new `tier_changes` audit table (one row per upgrade or downgrade ever applied; FK\'d to license + invoice + both policies). Schema is purely additive — existing licenses and policies are untouched and inherit `tier_rank = NULL` (not in any ladder).',
  '',
  '**Webhook event.** `license.tier_changed` fires whenever a license\'s policy changes, with `actor=buyer|admin|system` so downstream tooling can distinguish self-service vs operator vs scheduled changes.',
  '',
  '**Test count: 77** (was 57 at v0.2.0:4). +5 covering renewal-worker pending-tier-change hook + admin endpoint variants; +6 buyer-endpoint variants + webhook tier-change branch; +8 unit tests for the quote/apply math; +1 migration regression test for the 0013 schema.',
  '',
  '**Upgrade path.** v0.2.0:4 → v0.2.0:5 is a drop-in. Migration 0013 is additive only. No behavior change for existing operators unless they explicitly set tier_rank on their policies and start using the new endpoints.',
  '',
  '0.2.0:4 — **Recurring subscriptions are functional end-to-end.** Migration 0011 stopped being dormant: operators on Pro/Patron tier can now mark a policy as recurring, the renewal worker creates fresh invoices on cadence, the buy page renders subscription pricing, and both operator and buyer can cancel cleanly.',
  '',
  '**Admin UI.** Policy editor (create + edit) gains a "Recurring subscription (Pro)" section: tick the box, pick a cadence (Monthly / Quarterly / Semi-annual / Annual / Custom days), set grace-period days (default 7) and optional free-trial days. The Policies list table shows a gold "every Nd" badge alongside the existing trial badge so recurring tiers are recognisable at a glance. Free / Creator-tier operators see a 402 with an upgrade link if they try to flip a policy to recurring — same gating pattern as the existing product/policy/code caps.',
  '',
  '**Buy page.** Recurring tier cards render a "Renews monthly / annually / every N days" line plus a "/mo" / "/yr" / "/Nd" suffix on the headline price ("$25 / mo" not just "$25"). First-cycle trial banner shows when trial_days > 0 ("14 day free trial"). Tier-switching JS keeps the cadence suffix in sync as the buyer clicks between tiers.',
  '',
  '**Renewal worker.** Background worker sweeps every 60 seconds for subs whose `next_renewal_at` has passed. SAT-priced subs use identity conversion (no rate fetcher); fiat-priced subs re-quote each cycle so a billing cycle always reflects the BTC/USD rate at the moment of renewal (per MULTI_CURRENCY_DESIGN). Failed renewals back off on a 5min → 30min → 2h → 6h → 12h schedule, capped at 5 consecutive failures before the worker stops touching the row. Past-due subs whose grace window has elapsed transition to `lapsed` automatically.',
  '',
  '**New Subscriptions tab in the admin UI.** Lists all subs with status filter pills (All / Active / Past due / Cancelled / Lapsed). Each row shows the license, cadence, listed price (in original currency), status, next renewal, consecutive failures, and a one-click Cancel button (confirms with an optional reason captured to the audit log). Cancellation is non-destructive — the license stays valid through the end of the current billing cycle, the renewal worker just stops creating new invoices.',
  '',
  '**Buyer self-service cancel.** New `POST /v1/subscriptions/cancel` endpoint takes the buyer\'s signed license key as auth (no admin token, no cookie) and cancels the tied subscription. SDKs can wire a "Cancel subscription" button in the operator\'s app without involving the operator\'s support workflow. Bad/wrong/revoked keys all return 401 (not 404) so a probe can\'t enumerate which licenses have active subs.',
  '',
  '**Webhooks.** New `subscription.cancelled` event fires with `actor=admin|buyer` so operators can distinguish self-service cancels in their downstream tooling. The existing `subscription.lapsed` event fires when the worker transitions a past-due sub past its grace window.',
  '',
  '**Auto-charge via saved payment profiles is NOT in this release.** The renewal worker creates fresh invoices that the buyer must pay manually. v0.2.0:5+ adds the auto-charge path (Zaprite\'s `paymentProfileId` flow). Until then, subscriptions are "we send you a fresh invoice link every month" — closer to GitHub Sponsors than Stripe.',
  '',
  '**Test count: 57** (was 42 at v0.2.0:3). +7 renewal-worker integration tests, +4 admin policy tests covering recurring fields and the Pro-tier gate, +4 cancellation tests covering both admin and buyer paths.',
  '',
  '**Upgrade path.** v0.2.0:3 → v0.2.0:4 is a drop-in. The schema columns for recurring policies were already added in v0.2.0:2 (migration 0011); existing policies have `is_recurring=0` so the renewal worker has nothing to do. No behavior change unless an operator explicitly creates a recurring policy.',
  '',
  '0.2.0:3 — **Durable payment-provider switching.** Fixes a gap from v0.2.0:2 where Connect Zaprite swapped the in-memory provider but BTCPay silently re-took active on the next daemon restart. Both providers\' configurations can now coexist, with a persisted preference flag determining which one is active. New "Activate BTCPay" / "Activate Zaprite" StartOS Actions let operators flip between configured providers in one click without re-running Connect. Disconnect on either provider clears the preference only if it pointed at the disconnected one — symmetric handling preserves operator intent.',
  '',
  'New endpoints: `GET /v1/admin/payment-provider/status` (both configs\' state + active preference in one call), `POST /v1/admin/payment-provider/activate` (flip active without re-authorizing). The boot-time loader now reads the persisted preference, so what an operator activates today is what loads tomorrow regardless of which config rows happen to be in the DB.',
  '',
  'Test count: 42 (added `payment_provider_preference_round_trip` covering the full lifecycle).',
  '',
  '0.2.0:2 — **Zaprite payment provider lands.** Operators can now choose between BTCPay (Bitcoin-only, you run the BTCPay Server yourself) and Zaprite (Bitcoin + fiat cards via Stripe/Square, brokered by Zaprite, settles to your connected wallets). Switching is Disconnect → Connect via new StartOS Actions ("Connect Zaprite" / "Disconnect Zaprite" / "Check Zaprite connection"). Existing BTCPay-connected operators see zero change unless they explicitly switch.',
  '',
  'How it works: paste your Zaprite API key (created at app.zaprite.com → Settings → API) into the Connect Zaprite action. Daemon validates the key, swaps the active provider atomically. Then add a webhook in your Zaprite dashboard pointing at `<your-keysat-url>/v1/zaprite/webhook`.',
  '',
  '**Webhook security.** Zaprite does NOT sign webhook deliveries (verified May 2026 against their public OpenAPI + dashboard). Keysat\'s defense is the externalUniqId round-trip: we attach our local invoice UUID at order creation, and the webhook handler trusts the body only insofar as the order id resolves to a local invoice in an expected state. An attacker spoofing a webhook would need to know a UUID we never put on the wire to reach a real local invoice.',
  '',
  '**Migration 0011 (dormant) lands the recurring-subscriptions schema** — `subscriptions` + `subscription_invoices` tables, plus `is_recurring`/`renewal_period_days`/`grace_period_days` (default 7)/`trial_days` (default 0) columns on policies. No daemon code uses these yet; phases 2-6 of `RECURRING_SUBSCRIPTIONS_DESIGN.md` ship in follow-up releases. The schema is purely additive and existing policies inherit the safe defaults.',
  '',
  '**Migration 0012** adds the `zaprite_config` table (singleton row mirroring `btcpay_config` from migration 0002).',
  '',
  '**Limitation called out cleanly:** Zaprite\'s API has no native subscription endpoints — Keysat\'s renewal worker (when it ships) drives the cycle on our side and uses Zaprite\'s `paymentProfileId` + `POST /v1/orders/charge` to charge saved cards each cycle. This is actually a cleaner model than provider-managed subscriptions because Keysat keeps the source of truth on when to bill.',
  '',
  '**Test count: 41** (was 39; +2 covering the Zaprite webhook event-parsing contract and the provider kind self-identification, +1 covering migration 0011\'s populated-data backfill contract).',
  '',
  '**Upgrade path.** v0.2.0:1 → v0.2.0:2 is a straight drop-in. Two new SQLite migrations (0011, 0012); both are additive only. No behavior change for current operators unless they explicitly run Connect Zaprite.',
  '',
  '0.2.0:1 — Buy-page discount-code box no longer shows a "FOUNDERS50" placeholder. Empty placeholder now; buyers paste their actual code without a misleading hint.',
  '',
  RELEASE_NOTES,
].join('\n\n')

export const v0_2_0 = VersionInfo.of({
  version: '0.2.0:7',
  releaseNotes: { en_US: ROUTINE_NOTES },
  // No on-disk transformation needed — v0.2.0:0 is a label change.
  // SQLite-level migrations live separately under
  // licensing-service/migrations/ and run at daemon boot regardless
  // of the ExVer-level version graph.
  migrations: {},
})
