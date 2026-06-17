# Stage 1 result — developer SDK-integration journey (no payments)

**Verdict: `completed-clean` on run 3.** A fresh adopter, using only the published
docs, can stand up a product, issue a license under a non-master `merchant-onboard`
key, integrate the TypeScript SDK into a Next.js app, and gate a feature so a valid
license unlocks it and an absent/invalid one blocks it.

## Method

The harness (`./run.sh`) boots a disposable `keysat` fixture (fresh SQLite, fresh
issuer keypair), mints a `merchant-onboard` scoped key with the fixture's master
key, serves `keysat-docs/` as the published corpus, and materializes a pristine
Next.js/TS proof-of-work (`sandbox-template/` → `/tmp/onboarding-tester/`). The
global `onboarding-tester` agent then drives the journey **docs-only** — it never
reads Keysat source. Corpus declared in-scope: the docs site, the daemon's
`/v1/openapi.json`, and the npm `@keysat/licensing-client` README.

## Convergence

| Run | Verdict | Findings |
|-----|---------|----------|
| 1 | completed-with-stumbles (5) + 1 nit | SDK `verify()` shape wrong in integrate.html; product `price_value` vs `price_sats`; licenses filter param; `merchant-onboard` role undocumented; issuer-pubkey response shape; phantom `GET /v1/admin/products`. |
| 2 | completed-with-stumbles (1) + 1 nit | "Find a license by email" pointed at the wrong endpoint; server-side key transport unstated. |
| 3 | **completed-clean** | none. Walkthrough harvested to `agent.html`. |

Each finding was verified against Keysat source before the doc was changed (the
agent can't read source; the harness builder can).

## Doc fixes shipped this loop

**`keysat-docs/` (static site — deploys independently):**
- `integrate.html`: rewrote the verify/error examples (TS/Rust/Python) to the real
  v0.3 SDK — `verify()` throws/returns `Err` and yields `VerifyOk{payload,…}`; no
  `valid` boolean; entitlements at `payload.entitlements`; errors are `LicensingError`
  (`.code` in TS, `.kind` in Python; Rust `Error::BadSignature`/`BadFormat`). Replaced the
  result-fields table; added an offline-expiry note (`isExpiredAt`/`is_expired_at`; TS/Rust
  `verifyWithTime`) and server-side key-transport guidance.
- `agent.html`: added the `merchant-onboard` role row; added "Create a product" and
  "Add a tier (policy)" workflows with the `price_value`/`price_sats` distinction;
  fixed the comp-license field name (`buyer_note` → `note`); pointed "Find a license
  by email" at `/v1/admin/licenses/search`; **added the publishable worked example**
  (the harvested walkthrough).
- `wire-format.html`: corrected the `GET /v1/issuer/public-key` response shape.

**`licensing-service/src/api/openapi.rs` (served spec — ships with the next daemon
release; the local fixture was rebuilt so the agent saw the fixes):**
- `GET /v1/admin/licenses` description: requires `product_id=<uuid>`, not a slug.
- Removed the phantom `GET /v1/admin/products` (only POST exists; list is the public
  `GET /v1/products`).
- Added the `/v1/admin/licenses/search` path (was referenced but undefined).
- Product schema: marked `price_value` as the write field, `price_sats` as derived.

## Reproduce

```sh
./run.sh                  # prints the fixture URL, docs URL, merchant key, sandbox path
# feed runs/<id>/AGENT_BRIEF.md to the onboarding-tester agent
./teardown.sh runs/<id>   # leaves nothing running
```

Per-run logs and the three friction reports live under `runs/` (gitignored; the
tokens there are worthless after teardown).
