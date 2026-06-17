#!/usr/bin/env bash
# Shared config + helpers for the Keysat onboarding harness.
# Sourced by the stage scripts; not run directly.

set -euo pipefail

HARNESS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# onboarding-harness/ -> licensing-service-startos/ -> workspace root
WORKSPACE="$(cd "$HARNESS_DIR/../.." && pwd)"
DAEMON_DIR="$WORKSPACE/licensing-service-startos/licensing-service"
DAEMON_BIN="$DAEMON_DIR/target/release/keysat"
DOCS_DIR="$WORKSPACE/keysat-docs"
TEMPLATE_DIR="$HARNESS_DIR/sandbox-template"

# Per-run scratch lives under runs/ (gitignored). The agent's sandbox copy
# lives under /tmp/onboarding-tester/ per the onboarding-tester guide.
RUNS_DIR="$HARNESS_DIR/runs"
SANDBOX_BASE="/tmp/onboarding-tester"

# The active run's state file is pointed to by runs/current.
CURRENT_LINK="$RUNS_DIR/current"

log()  { printf '\033[1;34m[harness]\033[0m %s\n' "$*" >&2; }
ok()   { printf '\033[1;32m[ ok ]\033[0m %s\n' "$*" >&2; }
warn() { printf '\033[1;33m[warn]\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[fail]\033[0m %s\n' "$*" >&2; exit 1; }

# state_set KEY VALUE  — append/update a KEY=VALUE line in the run state file.
# Not concurrency-safe (uses a fixed temp suffix); the stages call it serially.
state_set() {
  local f="$1" k="$2" v="$3"
  touch "$f"
  # strip any existing line for this key, then append
  grep -v "^${k}=" "$f" > "$f.tmp" 2>/dev/null || true
  mv "$f.tmp" "$f"
  printf '%s=%s\n' "$k" "$v" >> "$f"
}

# state_get FILE KEY
state_get() { grep "^${2}=" "$1" | head -1 | cut -d= -f2-; }

# free_port — echo an unused TCP port on 127.0.0.1.
free_port() {
  node -e 'const s=require("net").createServer();s.listen(0,"127.0.0.1",()=>{console.log(s.address().port);s.close();});'
}

# wait_http URL TRIES — poll until URL returns 2xx/3xx, or die.
wait_http() {
  local url="$1" tries="${2:-50}" i
  for i in $(seq 1 "$tries"); do
    if curl -fsS -o /dev/null "$url" 2>/dev/null; then return 0; fi
    sleep 0.2
  done
  return 1
}

require() { command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1"; }
