#!/usr/bin/env bash
# Materialize a fresh, pristine proof-of-work app for the agent to integrate
# into. Copies sandbox-template/ to /tmp/onboarding-tester/sandbox-<run>/ and
# runs `npm install` so the app is known-good before the agent touches it.
# The agent mutates ONLY this copy. Usage: make-sandbox.sh [RUN_DIR]
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"
require node; require npm

RUN_DIR="${1:-$(readlink "$CURRENT_LINK")}"
[[ -d "$RUN_DIR" ]] || die "no run dir (boot a fixture first)"
STATE="$RUN_DIR/state.env"
RUN_ID="$(state_get "$STATE" RUN_ID)"

mkdir -p "$SANDBOX_BASE"
SANDBOX="$SANDBOX_BASE/sandbox-$RUN_ID"
rm -rf "$SANDBOX"
log "copying pristine proof-of-work to $SANDBOX"
# copy template without any stray build artifacts
( cd "$TEMPLATE_DIR" && find . -type d \( -name node_modules -o -name .next \) -prune -o -type f -print \
    | while IFS= read -r f; do mkdir -p "$SANDBOX/$(dirname "$f")"; cp "$f" "$SANDBOX/$f"; done )

log "installing base app dependencies (npm install)…"
( cd "$SANDBOX" && npm install --no-audit --no-fund >"$RUN_DIR/sandbox-npm.log" 2>&1 ) \
  || { tail -20 "$RUN_DIR/sandbox-npm.log" >&2; die "sandbox npm install failed"; }

state_set "$STATE" SANDBOX "$SANDBOX"
ok "pristine sandbox ready at $SANDBOX"
echo "$SANDBOX"
