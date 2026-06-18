#!/usr/bin/env bash
# prepare.sh — bootstrap a clean Debian/Ubuntu box to build the Keysat s9pk.
#
# Start9's build-from-source flow clones this repo onto a fresh box, then runs a
# bootstrap script followed by `make`. This installs every HOST prerequisite that
# `make` needs (npm → wrapper bundle; start-cli s9pk pack → Docker image build).
# It mirrors the official StartOS 0.4.0.x environment-setup page:
#   https://docs.start9.com/packaging/0.4.0.x/environment-setup.html
#
# Note: `prepare.sh` is a 0.3.5.x community-submission convention; the 0.4.x docs
# don't mention it, so the 0.4.x submission flow may not invoke it. This file is
# still the runnable, single-source record of what a clean build box needs.
#
# The Rust daemon is NOT built on the host — it compiles inside this package's
# Dockerfile (FROM rust:1.88-slim-bookworm), so no rustup/cargo is installed here.
#
# Idempotent: re-running skips tools already present. Targets apt-based distros.

set -euo pipefail

# Use sudo only when not already root (Start9's build box may run either way).
SUDO=""
if [ "$(id -u)" -ne 0 ]; then
  SUDO="sudo"
fi

NODE_MAJOR=22

log() { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }

# --- apt prerequisites -------------------------------------------------------
# build-essential → make/gcc; squashfs-tools(-ng) → start-cli s9pk packing;
# jq → used by s9pk.mk's build summary; git → the s9pk embeds the commit hash.
log "Installing apt prerequisites (make, jq, git, squashfs, curl)"
$SUDO apt-get update
$SUDO apt-get install -y --no-install-recommends \
  build-essential \
  ca-certificates \
  curl \
  git \
  jq \
  squashfs-tools \
  squashfs-tools-ng

# --- Node.js 22 --------------------------------------------------------------
# The wrapper (@start9labs/start-sdk + @vercel/ncc bundle) needs Node 22. We
# install it system-wide via NodeSource so it's on PATH for the non-interactive
# `make` that follows (the docs' nvm method would need a shell rc sourced first).
if command -v node >/dev/null 2>&1 && node -v | grep -q "^v${NODE_MAJOR}\."; then
  log "Node.js $(node -v) already present — skipping"
else
  log "Installing Node.js ${NODE_MAJOR} (NodeSource)"
  curl -fsSL "https://deb.nodesource.com/setup_${NODE_MAJOR}.x" | $SUDO -E bash -
  $SUDO apt-get install -y nodejs
fi

# --- Docker (+ buildx) -------------------------------------------------------
# start-cli s9pk pack builds the daemon image from the Dockerfile via Docker
# buildx. get.docker.com is Docker's official installer and bundles the buildx
# plugin.
if command -v docker >/dev/null 2>&1; then
  log "Docker $(docker --version | awk '{print $3}' | tr -d ,) already present — skipping"
else
  log "Installing Docker (official get.docker.com installer)"
  curl -fsSL https://get.docker.com | $SUDO sh
fi

# Cross-architecture builds (`make universal` / `make arm` on an x86 host) need
# QEMU binfmt handlers registered. Best-effort: requires the Docker daemon to be
# running. Harmless to skip if you only build the host's native arch (`make x86`).
if $SUDO docker info >/dev/null 2>&1; then
  log "Registering QEMU binfmt handlers for cross-arch builds (best-effort)"
  $SUDO docker run --privileged --rm tonistiigi/binfmt --install all ||
    echo "  (binfmt registration skipped — only native-arch builds will work)"
else
  echo "  (Docker daemon not reachable yet — skipping binfmt setup; start Docker"
  echo "   and re-run this script if you need cross-arch/universal builds.)"
fi

# --- start-cli (StartOS 0.4.x SDK) -------------------------------------------
# Official installer: fetches the latest prebuilt binary into ~/.local/bin.
# For a reproducible build, pin a release instead, e.g.:
#   curl -fsSLo ~/.local/bin/start-cli \
#     https://github.com/Start9Labs/start-os/releases/download/<tag>/start-cli_x86_64-linux
#   chmod +x ~/.local/bin/start-cli
if command -v start-cli >/dev/null 2>&1; then
  log "start-cli $(start-cli --version 2>/dev/null | awk '{print $2}') already present — skipping"
else
  log "Installing start-cli (StartOS 0.4.x SDK)"
  curl -fsSL https://start9.com/start-cli/install.sh | sh
fi

# The installer drops start-cli in ~/.local/bin and appends it to your shell rc.
# Persist it to .profile for future shells (only if not already recorded, so
# re-runs don't pile up duplicates), and export it for the rest of THIS session.
if ! grep -qsF '.local/bin' "${HOME}/.profile"; then
  echo 'export PATH="$HOME/.local/bin:$PATH"' >>"${HOME}/.profile"
fi
export PATH="${HOME}/.local/bin:${PATH}"

log "Done. Initialise your signing key with 'start-cli init', then run 'make' (or 'make x86')."
