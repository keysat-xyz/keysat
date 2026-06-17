#!/usr/bin/env bash
# Boot a fresh, disposable Keysat daemon on a throwaway SQLite DB.
# Creates a new run dir, writes its state file, points runs/current at it.
# Echoes the run id on success.
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

require curl; require openssl; require node

# Build the daemon if the release binary is missing.
if [[ ! -x "$DAEMON_BIN" ]]; then
  log "release binary missing; building (cargo build --release)…"
  ( cd "$DAEMON_DIR" && cargo build --release >/dev/null ) || die "daemon build failed"
fi

RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-$$"
RUN_DIR="$RUNS_DIR/$RUN_ID"
mkdir -p "$RUN_DIR"
STATE="$RUN_DIR/state.env"
: > "$STATE"

PORT="$(free_port)"
MASTER="$(openssl rand -hex 32)"
DB_DIR="$RUN_DIR/data"
mkdir -p "$DB_DIR"

state_set "$STATE" RUN_ID "$RUN_ID"
state_set "$STATE" RUN_DIR "$RUN_DIR"
state_set "$STATE" PORT "$PORT"
state_set "$STATE" BASE_URL "http://127.0.0.1:$PORT"
state_set "$STATE" MASTER_KEY "$MASTER"

log "booting keysat fixture on 127.0.0.1:$PORT (db: $DB_DIR/keysat.db)"
KEYSAT_BIND="127.0.0.1:$PORT" \
KEYSAT_DB_PATH="$DB_DIR/keysat.db" \
KEYSAT_ADMIN_API_KEY="$MASTER" \
BTCPAY_URL="http://127.0.0.1:1" \
KEYSAT_PUBLIC_URL="http://127.0.0.1:$PORT" \
KEYSAT_OPERATOR_NAME="Onboarding Fixture" \
  nohup "$DAEMON_BIN" >"$RUN_DIR/daemon.log" 2>&1 &
DAEMON_PID=$!
state_set "$STATE" DAEMON_PID "$DAEMON_PID"

if ! wait_http "http://127.0.0.1:$PORT/healthz" 75; then
  warn "daemon did not become healthy; last log lines:"
  tail -20 "$RUN_DIR/daemon.log" >&2 || true
  kill "$DAEMON_PID" 2>/dev/null || true
  die "fixture failed to start"
fi

ln -sfn "$RUN_DIR" "$CURRENT_LINK"
ok "fixture healthy (pid $DAEMON_PID) at http://127.0.0.1:$PORT"
echo "$RUN_ID"
