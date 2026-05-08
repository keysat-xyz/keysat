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
  version: '0.2.0:2',
  releaseNotes: { en_US: ROUTINE_NOTES },
  // No on-disk transformation needed — v0.2.0:0 is a label change.
  // SQLite-level migrations live separately under
  // licensing-service/migrations/ and run at daemon boot regardless
  // of the ExVer-level version graph.
  migrations: {},
})
