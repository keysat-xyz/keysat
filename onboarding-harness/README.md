# Keysat onboarding harness

A disposable test rig that runs the global **`onboarding-tester`** agent against
Keysat's developer SDK-integration journey, to find every place the *published
docs* leave a newcomer stuck — and, on a clean run, to harvest a publishable
"all it took was X, Y, Z" walkthrough.

The premise (from `~/Projects/standards/guides/onboarding-tester.md`): the agent
is a fresh adopter who may use **only the published docs corpus**, never Keysat
source. The harness builder (you) may read Keysat freely; the agent may not.

## What a run sets up

| Piece | What it is | Disposable via |
|-------|------------|----------------|
| Fixture daemon | a fresh `keysat` release binary on `127.0.0.1:<port>`, throwaway SQLite, fresh issuer keypair | `teardown.sh` |
| Provisioning | a **merchant-onboard** scoped key minted with the fixture's master key (the operator's job, not the agent's) | — |
| Docs corpus | `keysat-docs/` served over HTTP — the only how-to source the agent may read | `teardown.sh` |
| Sandbox | a pristine Next.js/TS proof-of-work (`sandbox-template/`) copied to `/tmp/onboarding-tester/`, with one ungated "Pro export" to gate | `teardown.sh` |

The fixture's dummy `BTCPAY_URL` is never dialed in this path: **Stage 1 is
license issuance + SDK integration, no payments.**

## Usage

```sh
./run.sh                       # boot + provision + serve docs + sandbox; writes AGENT_BRIEF.md
# → feed runs/<id>/AGENT_BRIEF.md to the onboarding-tester agent
./teardown.sh runs/<id>        # stop daemon + docs server, remove sandbox
./teardown.sh runs/<id> --purge   # also delete the run dir
```

Individual stages (`boot-fixture.sh`, `provision.sh`, `serve-docs.sh`,
`make-sandbox.sh`) can be run on their own; each reads/writes
`runs/<id>/state.env` and `runs/current` points at the active run.

## The loop

1. `./run.sh`, then run the `onboarding-tester` agent on the brief.
2. Read `runs/<id>/reports/friction.md`. If `completed-clean`, harvest the
   walkthrough into `keysat-docs/agent.html`. Otherwise fix the highest-severity
   **doc** gaps (additively — document missing API/how-to; don't rewrite
   marketing copy), tear down, and re-run on a fresh fixture.
3. Repeat until `completed-clean`.

## Stage 2 (gated, not built yet)

The buyer-pays-on-regtest path needs Keysat to ship `payment_providers:write` +
the sandbox-mode daemon flag + the network gate (slices 3–5, in progress). It
adds a Dockerized BTCPay regtest stack and grants the agent
`merchant-onboard` + `payment_providers:write` so it can connect BTCPay
(regtest) and drive a test buyer payment end to end. Connecting a *mainnet*
wallet stays operator-only by design — that boundary is a feature, not a gap.

## Requirements

`cargo`, `node`/`npm`, `python3`, `curl`, `jq`, `openssl`. (Docker is only
needed for Stage 2.)
