#!/usr/bin/env bash
# Stage 2 setup: a sandbox Keysat daemon wired to the regtest BTCPay stack, a
# scoped key that can BOTH onboard a catalog AND connect a payment provider
# (merchant-onboard + payment_providers:write), the docs corpus, and a sandbox
# app — then the agent brief for the buyer-pays journey.
#
# Networking: the daemon binds 0.0.0.0 and registers its BTCPay webhook via
# host.docker.internal so the BTCPay *container* can reach it on settle; the
# agent/harness reach the daemon on 127.0.0.1. Sandbox mode + a non-mainnet
# (regtest) store are what let the scoped key connect BTCPay at all.
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/../lib.sh"
require curl; require jq; require openssl; require node
STAGE2_DIR="$HARNESS_DIR/stage2"
BTCPAY_URL="$(grep -h KEYSAT_LIVE_BTCPAY_URL "$STAGE2_DIR/btcpay-regtest/.live-env" 2>/dev/null | cut -d= -f2-)"
BTCPAY_URL="${BTCPAY_URL:-http://127.0.0.1:49392}"

curl -fsS "$BTCPAY_URL/api/v1/health" >/dev/null 2>&1 \
  || die "regtest BTCPay not reachable at $BTCPAY_URL — run: (cd $STAGE2_DIR/btcpay-regtest && docker compose -p keysat-btcpay up -d)"

[[ -x "$DAEMON_BIN" ]] || { log "building daemon (cargo build --release)…"; ( cd "$DAEMON_DIR" && cargo build --release >/dev/null ) || die "daemon build failed"; }

RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-stage2-$$"
RUN_DIR="$RUNS_DIR/$RUN_ID"; mkdir -p "$RUN_DIR/data" "$RUN_DIR/reports"
STATE="$RUN_DIR/state.env"; : > "$STATE"
PORT="$(free_port)"; MASTER="$(openssl rand -hex 32)"
BASE_URL="http://127.0.0.1:$PORT"                 # agent/harness-facing
PUBLIC_URL="http://host.docker.internal:$PORT"    # BTCPay-container-facing (webhooks)

state_set "$STATE" RUN_ID "$RUN_ID"; state_set "$STATE" RUN_DIR "$RUN_DIR"
state_set "$STATE" PORT "$PORT"; state_set "$STATE" BASE_URL "$BASE_URL"
state_set "$STATE" MASTER_KEY "$MASTER"; state_set "$STATE" BTCPAY_URL "$BTCPAY_URL"

log "booting sandbox daemon on 0.0.0.0:$PORT (btcpay → $BTCPAY_URL)"
KEYSAT_BIND="0.0.0.0:$PORT" \
KEYSAT_DB_PATH="$RUN_DIR/data/keysat.db" \
KEYSAT_ADMIN_API_KEY="$MASTER" \
KEYSAT_SANDBOX_MODE=1 \
BTCPAY_URL="$BTCPAY_URL" \
KEYSAT_PUBLIC_URL="$PUBLIC_URL" \
KEYSAT_OPERATOR_NAME="Stage 2 Sandbox" \
  nohup "$DAEMON_BIN" >"$RUN_DIR/daemon.log" 2>&1 &
state_set "$STATE" DAEMON_PID "$!"
ln -sfn "$RUN_DIR" "$CURRENT_LINK"
wait_http "$BASE_URL/healthz" 75 || { tail -20 "$RUN_DIR/daemon.log" >&2; die "daemon failed to start"; }

# Confirm the sandbox flag is actually on (the whole gate depends on it).
[[ "$(curl -fsS -H "Authorization: Bearer $MASTER" "$BASE_URL/v1/admin/tier" | jq -r '.sandbox')" == "true" ]] \
  || die "daemon did not report sandbox mode"

log "minting scoped key: merchant-onboard + payment_providers:write"
SK="$(curl -fsS -X POST "$BASE_URL/v1/admin/api-keys" -H "Authorization: Bearer $MASTER" \
  -H 'Content-Type: application/json' \
  -d '{"label":"stage2-agent","role":"merchant-onboard","scopes":["payment_providers:write"]}' \
  | jq -r '.token')"
[[ "$SK" == ks_* ]] || die "scoped key mint failed"
state_set "$STATE" MERCHANT_KEY "$SK"

"$HARNESS_DIR/serve-docs.sh"   "$RUN_DIR" >/dev/null
"$HARNESS_DIR/make-sandbox.sh" "$RUN_DIR" >/dev/null
DOCS_URL="$(state_get "$STATE" DOCS_URL)"; SANDBOX="$(state_get "$STATE" SANDBOX)"

# Two BTCPay store contexts the test buyer/agent can use (regtest store has an
# on-chain wallet; created during de-risk). The agent connects via the scoped
# key; the BTCPay credential it needs is provided as the "operator's BTCPay".
[[ -f "$STAGE2_DIR/btcpay-regtest/.live-env" ]] \
  || die ".live-env missing — run stage2/btcpay-regtest/probe.sh first to mint the BTCPay store token (GATE_TOK_REGTEST)"
source "$STAGE2_DIR/btcpay-regtest/.live-env"

cat > "$RUN_DIR/AGENT_BRIEF.md" <<EOF
# Onboarding-tester brief — Keysat Stage 2 (agent connects BTCPay regtest + buyer pays)

You are a **fresh adopter**, following \`~/Projects/standards/guides/onboarding-tester.md\`.
Reach the goal using **only the docs corpus**. Never read Keysat source to unblock
yourself — a gap in the docs is a finding.

## Goal (checkable end-state)
Acting for a merchant on a **sandbox** Keysat instance, using a **scoped, non-master**
API key (it carries \`payment_providers:write\`), and the published docs only:

1. **Connect a BTCPay payment provider** (this box's regtest BTCPay) to Keysat over the
   API — no master key, no human clicking in a browser. (You hold a BTCPay credential for
   the regtest server, the way an operator delegating setup would hand one to you.)
2. Create a product with a **paid** policy/tier.
3. Produce a **buyer checkout** for that product (a purchase invoice).
4. Confirm that paying the invoice issues a license (the harness will pay it on regtest if
   you cannot from the docs alone — note where the docs leave that to plumbing).

Success = a paid product whose purchase, once settled, yields a valid license — reached
from the docs alone, under a scoped key, with BTCPay connected by you.

## Docs corpus (the ONLY how-to sources)
- Keysat docs site: **$DOCS_URL** (start at \`/agent.html\`, \`/integrate.html\`).
- Daemon OpenAPI: **$BASE_URL/v1/openapi.json**.

## Credentials you were handed
- Keysat server: **$BASE_URL**
- Scoped API key (merchant-onboard + payment_providers:write): **$SK**
- Regtest BTCPay server: **${KEYSAT_LIVE_BTCPAY_URL:-$BTCPAY_URL}**, store
  **${KEYSAT_LIVE_BTCPAY_STORE_REGTEST:-<regtest store id>}**, BTCPay token
  **${GATE_TOK_REGTEST:-<btcpay store token>}** (your "operator's BTCPay" access).
- You were NOT given the master Keysat admin key. If a step seems to need it, that is
  either an intended operator-only boundary (note it) or a doc gap (log it).

## Out of corpus (do not open)
Anything under the Keysat source tree, migrations, tests, or this harness.

## Output
Write your friction report to \`$RUN_DIR/reports/friction.md\` AND return it as your final
message, in your guide's format. Most-severe-first. On \`completed-clean\`, also emit the
publishable "all the agent had to do was X, Y, Z" walkthrough (secret-free).
EOF

ok "Stage 2 staged. Run id: $RUN_ID"
cat >&2 <<EOF

  Daemon (agent)  : $BASE_URL   (sandbox, btcpay → $BTCPAY_URL)
  Docs corpus     : $DOCS_URL
  Scoped key      : $SK
  Sandbox app     : $SANDBOX
  Agent brief     : $RUN_DIR/AGENT_BRIEF.md
  Buyer-pay helper: $STAGE2_DIR/buyer-pay.sh
  Tear down       : $HARNESS_DIR/teardown.sh "$RUN_DIR"
EOF
echo "$RUN_ID"
