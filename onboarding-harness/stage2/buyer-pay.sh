#!/usr/bin/env bash
# The "test buyer's wallet": pay a BTCPay invoice on regtest by sending to its
# on-chain address from the regtest bitcoind and mining a confirmation. Used by
# the Stage 2 harness to drive settlement (BTCPay → webhook → Keysat issues the
# license) once the merchant journey has produced a checkout invoice.
#
# Usage: buyer-pay.sh <btcpay_base_url> <store_api_key> <store_id> <invoice_id>
# Prints the funding txid on success.
set -euo pipefail
BASE="${1:?btcpay base url}"; KEY="${2:?store api key}"; STORE="${3:?store id}"; INV="${4:?invoice id}"
BTND=keysat-btcpay-bitcoind-1
cli(){ docker exec "$BTND" bitcoin-cli -regtest -rpcuser=keysat -rpcpassword=keysat -rpcport=43782 "$@"; }
# Wallet RPCs must name the wallet explicitly: NBXplorer loads its own wallet, so
# bitcoind has >1 loaded and a bare wallet call errors "Wallet file not specified".
wcli(){ cli -rpcwallet=miner "$@"; }

# Pull the invoice's on-chain payment address + BTC amount from BTCPay.
PM="$(curl -fsS -H "Authorization: token $KEY" \
  "$BASE/api/v1/stores/$STORE/invoices/$INV/payment-methods")"
ADDR="$(echo "$PM" | jq -r '[.[] | select((.paymentMethodId|ascii_upcase)=="BTC-CHAIN" or (.paymentMethodId|ascii_upcase)=="BTC")][0].destination // empty')"
AMT="$(echo "$PM"  | jq -r '[.[] | select((.paymentMethodId|ascii_upcase)=="BTC-CHAIN" or (.paymentMethodId|ascii_upcase)=="BTC")][0].amount // empty')"
[[ -n "$ADDR" && -n "$AMT" ]] || { echo "no on-chain payment method on invoice $INV" >&2; echo "$PM" >&2; exit 1; }

# Ensure the miner wallet has spendable coins, then pay + confirm.
cli -named createwallet wallet_name=miner load_on_startup=true >/dev/null 2>&1 || cli loadwallet miner >/dev/null 2>&1 || true
MINE_ADDR="$(wcli getnewaddress)"
cli generatetoaddress 101 "$MINE_ADDR" >/dev/null   # generatetoaddress is node-level (no wallet needed)
TXID="$(wcli sendtoaddress "$ADDR" "$AMT")"
cli generatetoaddress 1 "$MINE_ADDR" >/dev/null   # 1 conf (BTCPay HighSpeed settles at 0-conf seen / 1-conf)
echo "$TXID"
