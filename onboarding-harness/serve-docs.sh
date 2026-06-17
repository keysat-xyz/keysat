#!/usr/bin/env bash
# Serve the keysat-docs/ site over HTTP as the "published docs corpus" the
# agent is allowed to read. Writes the docs URL + server pid into state.
# Usage: serve-docs.sh [RUN_DIR]
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

RUN_DIR="${1:-$(readlink "$CURRENT_LINK")}"
[[ -d "$RUN_DIR" ]] || die "no run dir (boot a fixture first)"
STATE="$RUN_DIR/state.env"
[[ -d "$DOCS_DIR" ]] || die "keysat-docs not found at $DOCS_DIR"

PORT="$(free_port)"
log "serving published docs corpus from $DOCS_DIR on 127.0.0.1:$PORT"
# --directory avoids a `cd` subshell, so $! is the real python PID (not a
# wrapper shell that would orphan the server on teardown). nohup survives the
# SIGHUP when this script exits.
nohup python3 -m http.server "$PORT" --bind 127.0.0.1 --directory "$DOCS_DIR" \
    >"$RUN_DIR/docs-server.log" 2>&1 &
DOCS_PID=$!
state_set "$STATE" DOCS_PID "$DOCS_PID"
state_set "$STATE" DOCS_PORT "$PORT"
state_set "$STATE" DOCS_URL "http://127.0.0.1:$PORT"

if ! wait_http "http://127.0.0.1:$PORT/" 25; then
  die "docs server failed to come up"
fi
ok "docs corpus served at http://127.0.0.1:$PORT (pid $DOCS_PID)"
echo "http://127.0.0.1:$PORT"
