#!/usr/bin/env bash
# De-risk probe for agent-payment-connect network detection (spec §6.1).
# Stands up a store + on-chain regtest wallet on the local BTCPay regtest stack,
# then dumps the exact Greenfield responses the slice-3 gate would consult:
#   - GET /api/v1/stores/{id}/payment-methods         (paymentMethodId form? derivationScheme exposed?)
#   - GET /api/v1/stores/{id}/payment-methods/{pmid}/wallet/address   (bcrt1… prefix?)
# Read-only against Keysat; only mutates the throwaway BTCPay instance.
set -uo pipefail

BASE="${BTCPAY_BASE:-http://127.0.0.1:49392}"
ADMIN_EMAIL="admin@keysat.local"
ADMIN_PW="keysatregtest1!"
OUT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/probe-out"
mkdir -p "$OUT_DIR"

hr(){ printf '\n\033[1;36m=== %s ===\033[0m\n' "$*"; }
jqp(){ jq . 2>/dev/null || cat; }

# --- 0. wait for BTCPay --------------------------------------------------------
hr "0. waiting for BTCPay health at $BASE"
for i in $(seq 1 120); do
  if curl -fsS "$BASE/api/v1/health" >/dev/null 2>&1; then break; fi
  sleep 2
  [[ $i == 120 ]] && { echo "BTCPay never became healthy"; exit 1; }
done
curl -fsS "$BASE/api/v1/health" | jqp

# --- 1. create first admin (unauthenticated, only works on a fresh instance) ---
hr "1. create first admin (idempotent: 'already exists' is fine)"
curl -sS -X POST "$BASE/api/v1/users" \
  -H 'Content-Type: application/json' \
  -d "{\"email\":\"$ADMIN_EMAIL\",\"password\":\"$ADMIN_PW\",\"isAdministrator\":true}" | jqp

# Basic-auth header for subsequent Greenfield calls.
AUTH=(-u "$ADMIN_EMAIL:$ADMIN_PW")

# --- 2. create a store ---------------------------------------------------------
hr "2. create store"
STORE_JSON="$(curl -sS "${AUTH[@]}" -X POST "$BASE/api/v1/stores" \
  -H 'Content-Type: application/json' -d '{"name":"Keysat Regtest Co"}')"
echo "$STORE_JSON" | jqp
STORE_ID="$(echo "$STORE_JSON" | jq -r '.id')"
echo "STORE_ID=$STORE_ID"
[[ -z "$STORE_ID" || "$STORE_ID" == null ]] && { echo "no store id"; exit 1; }

# --- 3. generate an on-chain wallet; try BTC-CHAIN then BTC --------------------
gen_body='{"savePrivateKeys":false,"importKeysToRPC":false,"wordList":"English","wordCount":12,"scriptPubKeyType":"Segwit"}'
PMID=""
for cand in BTC-CHAIN BTC; do
  hr "3. generate wallet on pmid=$cand"
  code="$(curl -sS -o "$OUT_DIR/gen-$cand.json" -w '%{http_code}' "${AUTH[@]}" \
    -X POST "$BASE/api/v1/stores/$STORE_ID/payment-methods/$cand/wallet/generate" \
    -H 'Content-Type: application/json' -d "$gen_body")"
  echo "HTTP $code"; cat "$OUT_DIR/gen-$cand.json" | jqp
  if [[ "$code" == 2* ]]; then PMID="$cand"; break; fi
done
[[ -z "$PMID" ]] && echo "!! wallet generate failed for both pmid forms (see above)"

# --- 4. mine some regtest blocks so the wallet has a usable address ------------
hr "4. mine regtest blocks"
ADDR_FOR_MINE="$(docker exec keysat-btcpay-bitcoind-1 bitcoin-cli -regtest -rpcuser=keysat -rpcpassword=keysat -rpcport=43782 getnewaddress 2>/dev/null || true)"
echo "miner address: ${ADDR_FOR_MINE:-<none>}"
if [[ -n "$ADDR_FOR_MINE" ]]; then
  docker exec keysat-btcpay-bitcoind-1 bitcoin-cli -regtest -rpcuser=keysat -rpcpassword=keysat -rpcport=43782 generatetoaddress 101 "$ADDR_FOR_MINE" >/dev/null 2>&1 \
    && echo "mined 101 blocks" || echo "mine failed (non-fatal for detection probe)"
fi

# --- 5. THE PAYLOADS the slice-3 gate consults --------------------------------
hr "5a. GET payment-methods  (does it expose derivationScheme? what pmid?)"
curl -sS "${AUTH[@]}" "$BASE/api/v1/stores/$STORE_ID/payment-methods?includeConfig=true" \
  | tee "$OUT_DIR/payment-methods.json" | jqp

hr "5b. GET wallet/address  (THE network artifact — expect bcrt1…)"
ADDR_JSON="$(curl -sS "${AUTH[@]}" "$BASE/api/v1/stores/$STORE_ID/payment-methods/${PMID:-BTC-CHAIN}/wallet/address")"
echo "$ADDR_JSON" | tee "$OUT_DIR/wallet-address.json" | jqp
ADDR="$(echo "$ADDR_JSON" | jq -r '.address // empty')"

# --- 6. classify --------------------------------------------------------------
hr "6. network classification"
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

hr "done — raw payloads under $OUT_DIR/"
