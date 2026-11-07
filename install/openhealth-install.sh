#!/usr/bin/env bash
# Copyright (c) 2026 Th0rgal
# License: MIT | https://github.com/ma2skollar/open_health/raw/main/LICENSE
# Source: https://github.com/ma2skollar/open_health
#
# Runs INSIDE the LXC container. The engine fetches this file as
# install/openhealth-install.sh (named after APP="Open Health") from
# COMMUNITY_SCRIPTS_URL and pipes it into bash.
#
# The engine's container-side framework is sourced first and its setup steps run
# before ours: setting_up_container, network_check and update_os apply the
# container timezone and fix the base image, and motd_ssh/customize at the end
# apply the remaining dialog choices — root password or automatic login, console
# mode, SSH keys, MOTD — and create the container's `update` command. Skipping
# those steps is why an earlier revision still asked for a login after
# "Automatic Login" had been selected.

source /dev/stdin <<<"$FUNCTIONS_FILE_PATH"
color
verb_ip6
catch_errors
setting_up_container
network_check
update_os

REPO_URL="${REPO_URL:-https://github.com/ma2skollar/open_health.git}"
BRANCH="${BRANCH:-main}"
SRC_DIR=/root/oura
CONF=/etc/oura/oura.env

# ── Build dependencies ────────────────────────────────────────────────────────
msg_info "Installing Build Dependencies"
$STD apt install -y build-essential pkg-config libsqlite3-dev libdbus-1-dev libdbus-1-3 git curl ca-certificates
msg_ok "Installed Build Dependencies"

msg_info "Installing Rust Toolchain"
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y >/dev/null 2>&1
# shellcheck disable=SC1091
source "$HOME/.cargo/env"
msg_ok "Installed Rust Toolchain"

# ── Source ────────────────────────────────────────────────────────────────────
msg_info "Fetching Source"
if [[ -d "$SRC_DIR/.git" ]]; then
  git -C "$SRC_DIR" fetch --all --quiet || true
  git -C "$SRC_DIR" reset --hard "origin/$BRANCH" --quiet || true
else
  git clone --branch "$BRANCH" "$REPO_URL" "$SRC_DIR"
fi
msg_ok "Fetched Source"

# ── Build ─────────────────────────────────────────────────────────────────────
msg_info "Building oura-cli and oura-backup (this takes several minutes)"
cd "$SRC_DIR" || exit 1
$STD cargo build --release -p oura-cli -p oura-backup
msg_ok "Built oura-cli and oura-backup"

# ── Install ───────────────────────────────────────────────────────────────────
msg_info "Installing Dashboard"
# A clone made on a Windows host can lose the executable bit.
chmod +x ./deploy/install.sh ./deploy/health.sh 2>/dev/null || true
# install.sh writes the units, then enables the dashboard; that start cannot
# succeed while oura.env still holds __PLACEHOLDER__ paths, so its exit code is
# not the signal here — the units are.
bash ./deploy/install.sh --bin ./target/release/oura --backup-bin ./target/release/oura-backup || true
if [[ ! -f /etc/systemd/system/oura-dashboard.service ]]; then
  msg_error "install.sh did not install the systemd units"
  exit 1
fi
if [[ ! -f "$CONF" ]]; then
  msg_error "install.sh did not create ${CONF}"
  exit 1
fi

# OURA_TZ_OFFSET follows the timezone chosen in the dialog, which
# setting_up_container has just applied to the container.
tz_offset="$(awk -v z="$(date +%z)" 'BEGIN {
  sign = substr(z, 1, 1); h = substr(z, 2, 2) + 0; m = substr(z, 4, 2) + 0
  v = h + m / 60; if (sign == "-") v = -v; printf "%g", v
}')"
sed -i \
  -e 's|__RING_DB__|/srv/oura/data/oura.db|g' \
  -e 's|__MANUAL_DB__|/srv/oura/data/manual.db|g' \
  -e "s|__TZ_OFFSET__|${tz_offset}|g" \
  -e 's|__HUB_STATE_DIR__|/srv/oura/data/hub|g' "$CONF"

systemctl daemon-reload
systemctl enable oura-dashboard.service >/dev/null 2>&1 || true
systemctl restart oura-dashboard.service
sleep 2
if ! systemctl is-active --quiet oura-dashboard.service; then
  journalctl -u oura-dashboard.service -n 30 --no-pager || true
  msg_error "oura-dashboard.service did not start"
  exit 1
fi
msg_ok "Installed Dashboard"

# ── Verify ────────────────────────────────────────────────────────────────────
msg_info "Checking the Dashboard"
if ./deploy/health.sh; then
  msg_ok "Dashboard answers on [IP_ADDRESS]:8090 (loopback only)"
else
  msg_warn "Health check reported a problem — see the output above"
fi

motd_ssh
customize
cleanup_lxc
