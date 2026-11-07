#!/usr/bin/env bash
# Copyright (c) 2026 Th0rgal
# License: MIT | https://github.com/Th0rgal/open_health/raw/main/LICENSE
# Source: https://github.com/Th0rgal/open_health
#
# Runs INSIDE the LXC container. Fetched by build_container from
# COMMUNITY_SCRIPTS_URL/install/oura.sh. Builds the Rust binaries and installs
# the systemd units. Idempotent: re-running skips existing binaries.

set -euo pipefail

say()  { printf '%s\n' "$*"; }
ok()   { printf '✓ %s\n' "$*"; }
warn() { printf '! %s\n' "$*"; }
err()  { printf '✗ %s\n' "$*" >&2; }

# ── Dependencies ──────────────────────────────────────────────────────────────
say "Installing build dependencies..."
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq \
  build-essential pkg-config libsqlite3-dev libdbus-1-dev libdbus-1-3 \
  curl git ca-certificates
ok "Build dependencies installed"

# ── Rust toolchain ────────────────────────────────────────────────────────────
if ! command -v cargo >/dev/null 2>&1; then
  say "Installing Rust toolchain..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  . "$HOME/.cargo/env"
  ok "Rust toolchain installed"
else
  say "Rust toolchain already present"
fi

# ── Clone or update the repository ────────────────────────────────────────────
if [ ! -d /root/oura/.git ]; then
  say "Cloning repository..."
  git clone https://github.com/Th0rgal/open_health.git /root/oura
  ok "Repository cloned"
else
  say "Repository already present"
fi

cd /root/oura

# ── Build ─────────────────────────────────────────────────────────────────────
say "Building oura-cli and oura-backup (this takes several minutes)..."
cargo build --release -p oura-cli -p oura-backup
ok "Build complete"

# ── Install ───────────────────────────────────────────────────────────────────
say "Installing binaries and units..."
./deploy/install.sh --bin ./target/release/oura --backup-bin ./target/release/oura-backup
ok "Installed"

# ── Configure ─────────────────────────────────────────────────────────────────
CONF=/etc/oura/oura.env
if grep -q '__PLACEHOLDER__' "$CONF" 2>/dev/null; then
  say "Configuring ${CONF}..."
  sed -i 's|__RING_DB__|/srv/oura/data/oura.db|g' "$CONF"
  sed -i 's|__MANUAL_DB__|/srv/oura/data/manual.db|g' "$CONF"
  sed -i 's|__TZ_OFFSET__|0|g' "$CONF"
  sed -i 's|__HUB_STATE_DIR__|/srv/oura/data/hub|g' "$CONF"
  ok "Placeholders replaced — review ${CONF}"
else
  say "oura.env already configured — left untouched"
fi

# ── Enable and start ──────────────────────────────────────────────────────────
say "Starting oura-dashboard.service..."
systemctl daemon-reload
systemctl enable --now oura-dashboard.service
ok "Service started"

say "Open Health installation complete."
