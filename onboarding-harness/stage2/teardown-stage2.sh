#!/usr/bin/env bash
# Full Stage 2 teardown — run when the onboarding test is done so nothing keeps
# running. Stops, for each Stage 2 run: the ephemeral daemon + docs server +
# sandbox copy (via the shared teardown.sh); then kills any sandbox dev server
# the onboarding-tester left behind; then stops the shared regtest BTCPay docker
# stack (containers + volumes).
#
# Usage:
#   ./teardown-stage2.sh                 # tear down ALL Stage 2 runs + dev servers + BTCPay stack
#   ./teardown-stage2.sh --keep-btcpay   # same, but leave the BTCPay stack up (iterating)
#   ./teardown-stage2.sh runs/<id>       # one specific run dir (path relative to onboarding-harness/)
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/../lib.sh"

KEEP_BTCPAY=0; ONE_RUN=""
for a in "$@"; do
  case "$a" in
    --keep-btcpay) KEEP_BTCPAY=1 ;;
    *) ONE_RUN="$a" ;;
  esac
done

# 1. Per-run teardown (daemon + docs server + sandbox copy + freed ports).
if [[ -n "$ONE_RUN" ]]; then
  "$HARNESS_DIR/teardown.sh" "$ONE_RUN" || true
else
  shopt -s nullglob
  any=0
  for d in "$RUNS_DIR"/*stage2*/; do
    [[ -f "${d}state.env" ]] || continue
    "$HARNESS_DIR/teardown.sh" "${d%/}" || true
    any=1
  done
  [[ "$any" == 0 ]] && warn "no Stage 2 run dirs found under $RUNS_DIR"
fi

# 2. Kill any sandbox dev server the agent left running. The proof-of-work app
#    serves on :4311 (npm run dev); the onboarding-tester may start it and not
#    stop it.
for pid in $(lsof -ti tcp:4311 -sTCP:LISTEN 2>/dev/null || true); do
  kill "$pid" 2>/dev/null && log "stopped orphaned sandbox dev server (pid $pid on :4311)" || true
done

# 3. Stop the shared regtest BTCPay stack (containers + volumes) unless told to keep it.
if [[ "$KEEP_BTCPAY" == 1 ]]; then
  ok "left BTCPay regtest stack running (--keep-btcpay)"
elif docker ps -a --filter "name=keysat-btcpay" --format '{{.Names}}' 2>/dev/null | grep -q .; then
  ( cd "$HERE/btcpay-regtest" && docker compose -p keysat-btcpay down -v ) >/dev/null 2>&1 \
    && ok "stopped BTCPay regtest stack (containers + volumes removed)" \
    || warn "could not fully stop BTCPay — check: docker ps -a --filter name=keysat-btcpay"
else
  ok "BTCPay regtest stack already stopped"
fi

ok "Stage 2 teardown complete"
