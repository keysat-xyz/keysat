#!/usr/bin/env bash
# De-risk probe + .live-env minter for the Stage 2 / combined onboarding harness.
# Run once after `docker compose -p keysat-btcpay up -d`.
#
# Two jobs:
#   A. Mint .live-env — create the two stores the harness needs (one with an
#      on-chain regtest wallet, one without) plus store-scoped BTCPay API tokens
#      carrying the five permissions the Connect-BTCPay flow documents
#      (install.html#connect-btcpay), and write them to .live-env for
#      run-stage2.sh / validate-gate.sh to source.
#   B. De-risk (spec §6.1) — dump the exact Greenfield responses the slice-3
#      network gate consults (payment-methods, wallet/address) into probe-out/
#      and classify the receive-address HRP.
#
# Idempotency: assumes a FRESH instance (compose `up -d` after `down -v`).
# Re-running against a live instance creates duplicate stores — tear down first.
# Read-only against Keysat; only mutates the throwaway BTCPay instance.
set -uo pipefail

BASE="${BTCPAY_BASE:-http://127.0.0.1:49392}"
ADMIN_EMAIL="admin@keysat.local"
ADMIN_PW="keysatregtest1!"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT_DIR="$HERE/probe-out"
LIVE_ENV="$HERE/.live-env"
mkdir -p "$OUT_DIR"

# Permissions the documented Connect-BTCPay flow grants (install.html#connect-btcpay).
STORE_PERMS='canviewstoresettings canmodifystoresettings canviewinvoices cancreateinvoice canmodifyinvoices'
BTND=keysat-btcpay-bitcoind-1

hr(){ printf '\n\033[1;36m=== %s ===\033[0m\n' "$*"; }
jqp(){ jq . 2>/dev/null || cat; }
AUTH=(-u "$ADMIN_EMAIL:$ADMIN_PW")
cli(){ docker exec "$BTND" bitcoin-cli -regtest -rpcuser=keysat -rpcpassword=keysat -rpcport=43782 "$@"; }

create_store(){ # NAME -> store id
  curl -sS "${AUTH[@]}" -X POST "$BASE/api/v1/stores" \
    -H 'Content-Type: application/json' -d "{\"name\":\"$1\"}" | jq -r '.id'
}
store_token(){ # STORE_ID -> store-scoped API key with the 5 documented perms
  local sid="$1" perms="" p
  for p in $STORE_PERMS; do perms="$perms\"btcpay.store.$p:$sid\","; done
  curl -sS "${AUTH[@]}" -X POST "$BASE/api/v1/api-keys" \
    -H 'Content-Type: application/json' \
    -d "{\"label\":\"keysat-$sid\",\"permissions\":[${perms%,}]}" | jq -r '.apiKey'
}

# --- 0. wait for BTCPay --------------------------------------------------------
hr "0. waiting for BTCPay health at $BASE"
for i in $(seq 1 120); do
  curl -fsS "$BASE/api/v1/health" >/dev/null 2>&1 && break
  sleep 2
  [[ $i == 120 ]] && { echo "BTCPay never became healthy"; exit 1; }
done
curl -fsS "$BASE/api/v1/health" | jqp

# --- 1. create first admin (unauthenticated, only works on a fresh instance) ---
hr "1. create first admin (idempotent: 'already exists' is fine)"
curl -sS -X POST "$BASE/api/v1/users" \
  -H 'Content-Type: application/json' \
  -d "{\"email\":\"$ADMIN_EMAIL\",\"password\":\"$ADMIN_PW\",\"isAdministrator\":true}" | jqp

# --- 2. admin user API key (KEYSAT_LIVE_BTCPAY_KEY; broad, for ad-hoc admin use) -
hr "2. mint admin user API key"
ADMIN_KEY="$(curl -sS "${AUTH[@]}" -X POST "$BASE/api/v1/api-keys" \
  -H 'Content-Type: application/json' \
  -d '{"label":"keysat-admin","permissions":["btcpay.server.canmodifyserversettings","btcpay.store.canmodifystoresettings","btcpay.store.canmodifyinvoices"]}' \
  | jq -r '.apiKey')"
echo "ADMIN_KEY=${ADMIN_KEY:0:8}…"

# --- 3. regtest store WITH an on-chain wallet ----------------------------------
hr "3. create regtest store (with on-chain wallet)"
STORE_REGTEST="$(create_store 'Keysat Regtest Co')"
echo "STORE_REGTEST=$STORE_REGTEST"
[[ -z "$STORE_REGTEST" || "$STORE_REGTEST" == null ]] && { echo "no regtest store id"; exit 1; }

gen_body='{"savePrivateKeys":false,"importKeysToRPC":false,"wordList":"English","wordCount":12,"scriptPubKeyType":"Segwit"}'
PMID=""
for cand in BTC-CHAIN BTC; do
  hr "3b. generate wallet on pmid=$cand"
  code="$(curl -sS -o "$OUT_DIR/gen-$cand.json" -w '%{http_code}' "${AUTH[@]}" \
    -X POST "$BASE/api/v1/stores/$STORE_REGTEST/payment-methods/$cand/wallet/generate" \
    -H 'Content-Type: application/json' -d "$gen_body")"
  echo "HTTP $code"; cat "$OUT_DIR/gen-$cand.json" | jqp
  [[ "$code" == 2* ]] && { PMID="$cand"; break; }
done
[[ -z "$PMID" ]] && { echo "!! wallet generate failed for both pmid forms"; exit 1; }

# --- 4. mine regtest blocks so the wallet has a usable address -----------------
hr "4. mine regtest blocks"
ADDR_FOR_MINE="$(cli getnewaddress 2>/dev/null || true)"
echo "miner address: ${ADDR_FOR_MINE:-<none>}"
[[ -n "$ADDR_FOR_MINE" ]] && { cli generatetoaddress 101 "$ADDR_FOR_MINE" >/dev/null 2>&1 \
  && echo "mined 101 blocks" || echo "mine failed (non-fatal for detection probe)"; }

# --- 5. no-wallet store (fail-closed arm of the gate) --------------------------
hr "5. create no-wallet store"
STORE_NOWALLET="$(create_store 'Keysat NoWallet Co')"
echo "STORE_NOWALLET=$STORE_NOWALLET"
[[ -z "$STORE_NOWALLET" || "$STORE_NOWALLET" == null ]] && { echo "no nowallet store id"; exit 1; }

# --- 6. store-scoped tokens (what the agent/harness hand Keysat at connect) -----
hr "6. mint store-scoped tokens"
GATE_TOK_REGTEST="$(store_token "$STORE_REGTEST")"
GATE_TOK_NOWALLET="$(store_token "$STORE_NOWALLET")"
echo "GATE_TOK_REGTEST=${GATE_TOK_REGTEST:0:8}…  GATE_TOK_NOWALLET=${GATE_TOK_NOWALLET:0:8}…"
[[ "$GATE_TOK_REGTEST" == null || -z "$GATE_TOK_REGTEST" ]] && { echo "regtest token mint failed"; exit 1; }

# --- 7. THE PAYLOADS the slice-3 gate consults --------------------------------
hr "7a. GET payment-methods  (does it expose derivationScheme? what pmid?)"
curl -sS "${AUTH[@]}" "$BASE/api/v1/stores/$STORE_REGTEST/payment-methods?includeConfig=true" \
  | tee "$OUT_DIR/payment-methods.json" | jqp

hr "7b. GET wallet/address  (THE network artifact — expect bcrt1…)"
ADDR_JSON="$(curl -sS "${AUTH[@]}" "$BASE/api/v1/stores/$STORE_REGTEST/payment-methods/${PMID:-BTC-CHAIN}/wallet/address")"
echo "$ADDR_JSON" | tee "$OUT_DIR/wallet-address.json" | jqp
ADDR="$(echo "$ADDR_JSON" | jq -r '.address // empty')"

# --- 8. classify --------------------------------------------------------------
hr "8. network classification"
echo "pmid used      : ${PMID:-BTC-CHAIN}"
echo "receive address: ${ADDR:-<none>}"
case "$ADDR" in
  bcrt1*) echo "=> prefix bcrt1  => REGTEST  ✅ (non-mainnet → scoped connect allowed)";;
  tb1*)   echo "=> prefix tb1    => TESTNET/SIGNET (non-mainnet)";;
  bc1*)   echo "=> prefix bc1    => MAINNET ❌";;
  [mn2]*) echo "=> legacy base58 m/n/2 => TEST/REGTEST (non-mainnet)";;
  [13]*)  echo "=> legacy base58 1/3   => MAINNET ❌";;
  "")     echo "=> NO ADDRESS (Lightning-only / unconfigured) => FAIL-CLOSED → mainnet → master-only";;
  *)      echo "=> UNRECOGNIZED prefix => FAIL-CLOSED → mainnet → master-only";;
esac

# --- 9. write .live-env -------------------------------------------------------
hr "9. write .live-env"
cat > "$LIVE_ENV" <<EOF
export KEYSAT_LIVE_BTCPAY_URL=$BASE
export KEYSAT_LIVE_BTCPAY_KEY=$ADMIN_KEY
export KEYSAT_LIVE_BTCPAY_STORE_REGTEST=$STORE_REGTEST
export KEYSAT_LIVE_BTCPAY_STORE_NOWALLET=$STORE_NOWALLET
export GATE_TOK_REGTEST=$GATE_TOK_REGTEST
export GATE_TOK_NOWALLET=$GATE_TOK_NOWALLET
EOF
echo "wrote $LIVE_ENV"

hr "done — raw payloads under $OUT_DIR/, credentials in $LIVE_ENV"
