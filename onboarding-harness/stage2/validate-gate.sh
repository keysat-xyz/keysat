#!/usr/bin/env bash
# End-to-end validation of the agent-payment-connect gate against the LIVE
# regtest BTCPay (the spec's hard requirement). Boots a throwaway Keysat daemon
# in sandbox mode pointed at the regtest BTCPay stack, mints a scoped
# `payment_providers:write` key, and drives the full OAuth round-trip for two
# stores:
#   - no-wallet store  → network undetermined → FAIL CLOSED → connect DENIED (400)
#   - regtest store    → bcrt1 address → non-mainnet → connect ALLOWED (persisted)
#
# Requires the regtest stack up (docker compose -p keysat-btcpay up -d) and
# .live-env populated (GATE_TOK_REGTEST / GATE_TOK_NOWALLET — single-store BTCPay
# tokens). Reads the daemon release binary built by `cargo build --release`.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/btcpay-regtest/.live-env"
BIN="$HERE/../../licensing-service/target/release/keysat"
[[ -x "$BIN" ]] || { echo "FAIL: release binary missing ($BIN) — run cargo build --release"; exit 1; }

PORT=$(node -e 'const s=require("net").createServer();s.listen(0,"127.0.0.1",()=>{console.log(s.address().port);s.close();})')
MASTER=$(openssl rand -hex 32)
TMP=$(mktemp -d)
BASE="http://127.0.0.1:$PORT"
pass=0; fail=0
ok(){ echo "  ✅ $*"; pass=$((pass+1)); }
no(){ echo "  ❌ $*"; fail=$((fail+1)); }

echo "== booting sandbox daemon on $BASE (btcpay → $KEYSAT_LIVE_BTCPAY_URL) =="
KEYSAT_BIND="127.0.0.1:$PORT" \
KEYSAT_DB_PATH="$TMP/keysat.db" \
KEYSAT_ADMIN_API_KEY="$MASTER" \
KEYSAT_SANDBOX_MODE=1 \
BTCPAY_URL="$KEYSAT_LIVE_BTCPAY_URL" \
KEYSAT_PUBLIC_URL="$BASE" \
KEYSAT_OPERATOR_NAME="Stage2 Gate Validation" \
  nohup "$BIN" >"$TMP/daemon.log" 2>&1 &
DAEMON_PID=$!
trap 'kill $DAEMON_PID 2>/dev/null; rm -rf "$TMP"' EXIT
for i in $(seq 1 75); do curl -fsS "$BASE/healthz" >/dev/null 2>&1 && break; sleep 0.2; [[ $i == 75 ]] && { echo "FAIL: daemon never healthy"; tail -20 "$TMP/daemon.log"; exit 1; }; done

M=(-H "Authorization: Bearer $MASTER")

echo "== 1. sandbox flag surfaced read-only in /v1/admin/tier =="
[[ "$(curl -sS "${M[@]}" "$BASE/v1/admin/tier" | jq -r '.sandbox')" == "true" ]] && ok "tier.sandbox == true" || no "sandbox flag not surfaced"

echo "== 2. mint scoped merchant-onboard + payment_providers:write key =="
SK="$(curl -sS "${M[@]}" -X POST "$BASE/v1/admin/api-keys" -H 'Content-Type: application/json' \
  -d '{"label":"agent","role":"merchant-onboard","scopes":["payment_providers:write"]}' | jq -r '.token')"
[[ "$SK" == ks_* ]] && ok "scoped key minted" || { no "mint failed"; }
S=(-H "Authorization: Bearer $SK")

# drive a connect: returns HTTP status of the callback. $1=btcpay token
drive_connect(){
  local tok="$1"
  local st; st="$(curl -sS "${S[@]}" -X POST "$BASE/v1/admin/btcpay/connect" | jq -r '.state')"
  [[ -n "$st" && "$st" != null ]] || { echo "000"; return; }
  curl -sS -o /tmp/gate-cb.out -w '%{http_code}' -X POST "$BASE/v1/btcpay/authorize/callback?state=$st" \
    --data-urlencode "apiKey=$tok"
}

echo "== 3. DENY: scoped connect to a no-wallet store (undetermined → fail-closed) =="
code="$(drive_connect "$GATE_TOK_NOWALLET")"
if [[ "$code" == 400 ]]; then
  ok "callback rejected with HTTP 400"
  grep -qi "non-mainnet" /tmp/gate-cb.out && ok "rejection cites the non-mainnet restriction" || no "rejection message unexpected: $(cat /tmp/gate-cb.out | head -c200)"
else
  no "expected 400, got $code ($(cat /tmp/gate-cb.out | head -c200))"
fi
[[ "$(curl -sS "${M[@]}" "$BASE/v1/admin/btcpay/status" | jq -r '.connected')" == "false" ]] && ok "no provider persisted on deny" || no "a provider was persisted despite deny!"
# The GET callback form (what the agent docs show) must ALSO deny with a 4xx,
# not a 200 error page (regression guard for the GET-handler status fix).
gst="$(curl -sS "${S[@]}" -X POST "$BASE/v1/admin/btcpay/connect" | jq -r '.state')"
gcode="$(curl -sS -o /dev/null -w '%{http_code}' "$BASE/v1/btcpay/authorize/callback?state=$gst&apiKey=$GATE_TOK_NOWALLET")"
[[ "$gcode" == 4* ]] && ok "GET callback form denies with HTTP $gcode (not a 200 error page)" || no "GET callback returned $gcode (expected 4xx)"

echo "== 4. ALLOW: scoped connect to the regtest store (bcrt1 → non-mainnet) =="
code="$(drive_connect "$GATE_TOK_REGTEST")"
if [[ "$code" == 200 ]]; then ok "callback succeeded with HTTP 200"; else no "expected 200, got $code ($(cat /tmp/gate-cb.out | head -c300))"; fi
ST_JSON="$(curl -sS "${M[@]}" "$BASE/v1/admin/btcpay/status")"
[[ "$(echo "$ST_JSON" | jq -r '.connected')" == "true" ]] && ok "provider persisted" || no "provider not persisted on allow"
[[ "$(echo "$ST_JSON" | jq -r '.store_id')" == "$KEYSAT_LIVE_BTCPAY_STORE_REGTEST" ]] && ok "persisted store is the regtest store" || no "wrong store persisted: $(echo "$ST_JSON" | jq -c '.store_id')"

echo "== 5. scoped connect is audited with the resolved network =="
AUD="$(curl -sS "${M[@]}" "$BASE/v1/admin/audit?action=payment_provider.connect_scoped" | jq -c '.entries[0] // empty')"
echo "    audit: $AUD"
echo "$AUD" | grep -qi "regtest" && ok "audit row records network=regtest" || no "audit row missing/!regtest"

echo
echo "==== RESULT: $pass passed, $fail failed ===="
[[ $fail == 0 ]] || exit 1
