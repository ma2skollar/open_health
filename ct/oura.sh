#!/usr/bin/env bash
# Copyright (c) 2026 Th0rgal
# License: MIT | https://github.com/Th0rgal/open_health/raw/main/LICENSE
# Source: https://github.com/Th0rgal/open_health
#
# Community-scripts style LXC installer for the Open Health dashboard.
# Run on the Proxmox VE host as root:
#
#   bash -c "$(curl -fsSL https://raw.githubusercontent.com/Th0rgal/open_health/main/ct/oura.sh)"
#
# The script sources the community-scripts engine (build.func), creates an
# unprivileged Debian LXC, and runs install/oura.sh inside it. The install
# script builds the Rust binaries and starts the systemd units.

export COMMUNITY_SCRIPTS_URL="${COMMUNITY_SCRIPTS_URL:-https://raw.githubusercontent.com/Th0rgal/open_health/main}"

_cs_boot="${COMMUNITY_SCRIPTS_CORE_DIR:-$(dirname "${BASH_SOURCE[0]}")/../../core}/core/build.func"
source "$_cs_boot" 2>/dev/null || source <(curl -fsSL "${COMMUNITY_SCRIPTS_CORE_URL:-https://raw.githubusercontent.com/community-scripts/core/main}/core/build.func")

APP="Open Health"
var_tags="${var_tags:-health;dashboard;oura}"
var_cpu="${var_cpu:-2}"
var_ram="${var_ram:-2048}"
var_disk="${var_disk:-8}"
var_os="${var_os:-debian}"
var_version="${var_version:-12}"
var_unprivileged="${var_unprivileged:-1}"
var_install="${var_install:-oura}"

header_info "$APP"
variables
color
catch_errors

function update_script() {
  header_info
  check_container_storage
  check_container_resources

  if [[ ! -d /root/oura ]]; then
    msg_error "No ${APP} Installation Found!"
    exit
  fi

  msg_info "Updating ${APP}"
  cd /root/oura
  git pull --ff-only
  . "$HOME/.cargo/env"
  cargo build --release -p oura-cli -p oura-backup
  msg_ok "Updated ${APP}"

  msg_info "Restarting service"
  systemctl restart oura-dashboard
  msg_ok "Restarted service"
  exit
}

start
build_container
description

msg_ok "Completed Successfully!\n"
