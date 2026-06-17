#!/usr/bin/env bash
# Tear down a run: stop the daemon + docs server, remove the agent's sandbox
# copy. Keeps the run dir (logs + reports) unless --purge is given.
# Usage: teardown.sh [RUN_DIR] [--purge]
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

PURGE=0; RUN_DIR=""
for a in "$@"; do
  case "$a" in
    --purge) PURGE=1 ;;
    *) RUN_DIR="$a" ;;
  esac
done
RUN_DIR="${RUN_DIR:-$(readlink "$CURRENT_LINK" 2>/dev/null || true)}"
[[ -n "$RUN_DIR" && -d "$RUN_DIR" ]] || { warn "no run dir to tear down"; exit 0; }
STATE="$RUN_DIR/state.env"

for key in DAEMON_PID DOCS_PID; do
  pid="$(state_get "$STATE" "$key" 2>/dev/null || true)"
  if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    log "stopped $key ($pid)"
  fi
done

# Belt-and-suspenders: free the recorded ports in case a PID drifted.
for portkey in PORT DOCS_PORT; do
  port="$(state_get "$STATE" "$portkey" 2>/dev/null || true)"
  [[ -z "$port" ]] && continue
  for lpid in $(lsof -ti "tcp:$port" -sTCP:LISTEN 2>/dev/null || true); do
    kill "$lpid" 2>/dev/null && log "freed port $port (pid $lpid)" || true
  done
done

SANDBOX="$(state_get "$STATE" SANDBOX 2>/dev/null || true)"
if [[ -n "$SANDBOX" && -d "$SANDBOX" ]]; then rm -rf "$SANDBOX"; log "removed sandbox $SANDBOX"; fi

if [[ "$PURGE" == 1 ]]; then
  rm -rf "$RUN_DIR"; log "purged run dir $RUN_DIR"
  [[ "$(readlink "$CURRENT_LINK" 2>/dev/null)" == "$RUN_DIR" ]] && rm -f "$CURRENT_LINK"
fi
ok "teardown complete"
