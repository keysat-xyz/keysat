#!/usr/bin/env bash
# Provisioner step (the human operator's job, NOT the agent's): with the
# fixture's master key, mint a merchant-onboard scoped key and capture the
# issuer public key. Writes both into the run state file.
# Usage: provision.sh [RUN_DIR]   (defaults to runs/current)
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"
require curl; require jq

RUN_DIR="${1:-$(readlink "$CURRENT_LINK")}"
[[ -d "$RUN_DIR" ]] || die "no run dir (boot a fixture first)"
STATE="$RUN_DIR/state.env"
BASE_URL="$(state_get "$STATE" BASE_URL)"
MASTER="$(state_get "$STATE" MASTER_KEY)"

log "minting merchant-onboard scoped key via master key"
RESP="$(curl -fsS -X POST "$BASE_URL/v1/admin/api-keys" \
  -H "Authorization: Bearer $MASTER" -H "Content-Type: application/json" \
  -d '{"label":"onboarding-agent","role":"merchant-onboard","scopes":[]}')" \
  || die "key mint failed"
TOKEN="$(echo "$RESP" | jq -r '.token')"
[[ "$TOKEN" == ks_* ]] || die "unexpected mint response: $RESP"
state_set "$STATE" MERCHANT_KEY "$TOKEN"

log "fetching issuer public key"
PUBKEY_PEM="$(curl -fsS "$BASE_URL/v1/issuer/public-key" | jq -r '.public_key_pem')"
[[ "$PUBKEY_PEM" == *"BEGIN PUBLIC KEY"* ]] || die "could not fetch issuer public key"
printf '%s' "$PUBKEY_PEM" > "$RUN_DIR/issuer.pub"
state_set "$STATE" ISSUER_PUBKEY_FILE "$RUN_DIR/issuer.pub"

ok "merchant-onboard key minted; issuer pubkey saved to $RUN_DIR/issuer.pub"
echo "$TOKEN"
