# Stage 2 result — agent connects BTCPay (regtest) + buyer pays (payments)

**Verdict: `completed-clean` on run 3 (0 findings).** A fresh adopter, using only the
published docs and a **scoped** key (`merchant-onboard` + `payment_providers:write`, no
master key), can connect a regtest BTCPay over the API with **no browser step**, stand up
a paid product, produce a buyer checkout, and have a **real (regtest) on-chain payment
settle into a signed license** that validates offline.

This is the buyer-pays half of the onboarding harness (Stage 1 = no-payments SDK
integration). It is gated on the **agent-payment-connect** daemon feature (slices 3-4):
the scoped BTCPay connect is allowed only on a **sandbox** daemon for a **non-mainnet**
network. See `plans/agent-payment-connect-scope.md` and `stage2/FINDINGS.md`.

## Method

`stage2/run-stage2.sh` boots a disposable Keysat daemon in **sandbox mode**
(`KEYSAT_SANDBOX_MODE=1`) wired to the regtest BTCPay stack (`stage2/btcpay-regtest/`),
mints a scoped key carrying `payment_providers:write`, serves `keysat-docs/` as the
corpus, and materializes a sandbox app. The daemon binds `0.0.0.0` and registers its
settle webhook via `host.docker.internal` so the BTCPay container can reach it. The
global `onboarding-tester` agent then drives the journey **docs-only**. The test buyer's
wallet is `stage2/buyer-pay.sh` (pays the invoice on regtest + mines a confirmation).

## Convergence

| Run | Verdict | Findings |
|-----|---------|----------|
| 1 | blocked-at-step-1 (docs) | 2 blockers (agent.html#not-exposed said provider-connect is master-only; the connect/status/callback endpoints absent from OpenAPI) + 2 stumbles (headless callback pattern undocumented; `payment_providers:write` scope undocumented) + 1 nit. |
| 2 | **completed-clean** | 1 doc nit (install.html BTCPay permission list wrong) + 1 harness-script bug (`buyer-pay.sh` missing `-rpcwallet`). |
| 3 | **completed-clean (0)** | none. Walkthrough harvested below. |

The capability worked end to end from run 1 (the agent connected BTCPay headlessly and got
a license); the blockers were purely that the docs *said it was impossible* and didn't
document the path.

## Doc fixes shipped this loop

**`keysat-docs/` (deploys independently):**
- `agent.html`: corrected the `#auth` master-only statement; added an **A-la-carte extra
  scopes** subsection (`payment_providers:write`); narrowed `#not-exposed` to the accurate
  gate (scoped connect allowed only sandbox + non-mainnet; disconnect + production/mainnet
  stay master-only); added the **Connect BTCPay programmatically (sandbox)** workflow
  (`#connect-btcpay`) with the 3-step API flow.
- `install.html`: corrected the BTCPay permission list to the five the daemon actually
  requests; added an "automating setup?" pointer to the agent path.

**`licensing-service/src/api/openapi.rs` (served spec; ships next daemon release):**
- Added `/v1/admin/btcpay/connect`, `/v1/btcpay/authorize/callback`,
  `/v1/admin/btcpay/status`, `/v1/admin/btcpay/disconnect`; added the `scopes` field to
  scoped-key creation; noted the read-only `sandbox` flag on `/v1/admin/tier`.

## Reproduce

```sh
(cd stage2/btcpay-regtest && docker compose -p keysat-btcpay up -d)   # one-time
./stage2/run-stage2.sh                 # boots sandbox daemon + regtest wiring + scoped key
# feed runs/<id>/AGENT_BRIEF.md to the onboarding-tester agent
./teardown.sh runs/<id>                # stops daemon + docs server
```

## Publishable walkthrough (harvested, run 3)

All it took, on a sandbox Keysat with a scoped `payment_providers:write` key and a regtest
BTCPay store key (no master key, no browser):

1. **Connect BTCPay** — `POST /v1/admin/btcpay/connect` -> `state`; then
   `GET /v1/btcpay/authorize/callback?state=<state>&apiKey=<btcpay_store_key>`; confirm with
   `GET /v1/admin/btcpay/status`.
2. **Define a paid product** — `POST /v1/admin/products` + `POST /v1/admin/policies`.
3. **Create a checkout** — `POST /v1/purchase` -> `checkout_url` + `amount_sats`.
4. **Buyer pays** (regtest on-chain), daemon settles via webhook, `GET /v1/purchase/<id>`
   returns `status: settled` + a signed `license_key`.
5. **Validate** — `POST /v1/validate` -> `ok: true` with the tier's entitlements.
