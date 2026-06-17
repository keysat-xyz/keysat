#!/usr/bin/env bash
# One-shot Stage 1 setup: boot fixture, provision the merchant-onboard key,
# serve the docs corpus, materialize a pristine sandbox, then emit the agent
# brief (AGENT_BRIEF.md) with the live URLs + credentials interpolated in.
#
# This script sets the stage; it does NOT run the agent (the orchestrator does
# that with the global onboarding-tester agent, feeding it AGENT_BRIEF.md).
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

RUN_ID="$("$HARNESS_DIR/boot-fixture.sh")"
RUN_DIR="$RUNS_DIR/$RUN_ID"
STATE="$RUN_DIR/state.env"
"$HARNESS_DIR/provision.sh"   "$RUN_DIR" >/dev/null
"$HARNESS_DIR/serve-docs.sh"  "$RUN_DIR" >/dev/null
"$HARNESS_DIR/make-sandbox.sh" "$RUN_DIR" >/dev/null

BASE_URL="$(state_get "$STATE" BASE_URL)"
DOCS_URL="$(state_get "$STATE" DOCS_URL)"
MERCHANT_KEY="$(state_get "$STATE" MERCHANT_KEY)"
SANDBOX="$(state_get "$STATE" SANDBOX)"
mkdir -p "$RUN_DIR/reports"

cat > "$RUN_DIR/AGENT_BRIEF.md" <<EOF
# Onboarding-tester brief — Keysat SDK integration (Stage 1, no payments)

You are a **fresh adopter**, following your operating guide
(\`~/Projects/standards/guides/onboarding-tester.md\`). Reach the goal below
using **only the docs corpus**. Never read Keysat's server or SDK source to
unblock yourself — if the docs don't get you there, that is a finding.

## Goal (checkable end-state)
A developer with a Next.js/TypeScript app wants to sell it. Using a **scoped,
non-master API key**, and the published docs only:

1. Define the product in Keysat's catalog.
2. Add at least one tier/policy with an entitlement.
3. Manually issue a license for that product/tier (a comp/dev license — no
   payment in this path).
4. Integrate the TypeScript SDK into the proof-of-work app so the **Pro export**
   (\`GET /api/export\`) is gated: it returns the CSV only with a valid license.
5. Verify the gate both ways: a **valid** license unlocks the export; **no**
   license and a **tampered/invalid** license are blocked (4xx, not the CSV).

Success = the gate demonstrably works both ways, reached from the docs alone.

## Docs corpus (the ONLY how-to sources you may consult)
- The Keysat docs site, served at: **$DOCS_URL** (start at \`/integrate.html\`
  and \`/agent.html\`; the whole site is in-corpus).
- The daemon's published OpenAPI spec: **$BASE_URL/v1/openapi.json**
  (unauthenticated; the docs explicitly point adopters here).
- The npm package README for \`@keysat/licensing-client\` (\`npm view\`, or the
  package page). The SDK's published README is in-corpus.

**Out of corpus (do not open):** anything under the Keysat source tree
(\`$WORKSPACE/licensing-service-startos\`, \`$WORKSPACE/licensing-client-*\`,
migrations, tests, this harness). Reading any of it invalidates the run — say so
if you do.

## Your sandbox (mutate ONLY this)
\`$SANDBOX\` — a pristine copy of the "Acme Reports" app. Read its own
\`README.md\` freely (it's your app). Deps are already installed. Run it with
\`npm run dev\` (it serves on http://localhost:4311). Put all scratch under
\`/tmp/onboarding-tester/\`.

## Credentials you were handed (a real adopter would get these from their operator)
- Keysat server URL: **$BASE_URL**
- Scoped API key (merchant-onboard role): **$MERCHANT_KEY**
- (The issuer public key is fetchable per the docs — find how.)

You were NOT given the master admin key. If a step seems to require it, that is
either an intended operator-only boundary (note it) or a doc gap (log it).

## Output
Write your friction report to \`$RUN_DIR/reports/friction.md\` AND return it as
your final message, exactly in the format from your guide (Verdict, Corpus &
goal, Friction log most-severe-first, Path walked, Confidence). On a
\`completed-clean\` verdict only, also emit the publishable walkthrough
(secret-free, placeholders for URL/key). Record commands and doc locations as
you go; do not work from memory.
EOF

ok "Stage 1 staged. Run id: $RUN_ID"
cat >&2 <<EOF

  Fixture URL : $BASE_URL
  Docs corpus : $DOCS_URL
  Merchant key: $MERCHANT_KEY
  Sandbox     : $SANDBOX
  Agent brief : $RUN_DIR/AGENT_BRIEF.md
  Reports dir : $RUN_DIR/reports/

  Tear down with:  $HARNESS_DIR/teardown.sh "$RUN_DIR"
EOF
echo "$RUN_ID"
