#!/usr/bin/env bash
# Copyright (c) 2026 Th0rgal
# License: MIT | https://github.com/ma2skollar/open_health/raw/main/LICENSE
# Source: https://github.com/ma2skollar/open_health
#
# Community-scripts style LXC installer for the Open Health dashboard.
# Run on the Proxmox VE host as root:
#
#   bash -c "$(curl -fsSL https://raw.githubusercontent.com/ma2skollar/open_health/main/ct/oura.sh)"
#
# Sources the community-scripts engine (build.func), which opens the usual
# Default/Advanced dialog (Advanced picks the container ID), creates an
# unprivileged Debian LXC, and runs install/openhealth-install.sh inside it.
#
# The script base points at this fork: a ct script fetched with curl cannot tell
# the engine where it came from, and install/openhealth-install.sh only exists
# here.

export COMMUNITY_SCRIPTS_URL="${COMMUNITY_SCRIPTS_URL:-https://raw.githubusercontent.com/ma2skollar/open_health/main}"

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
# No var_install here: the engine's variables() always sets it to
# "<lowercased APP>-install". With APP="Open Health" that is
# install/openhealth-install.sh. Rename that file if APP ever changes.

# The engine's variables() sets DIAGNOSTICS=yes, and its TELEMETRY_URL default is
# https://telemetry.community-scripts.org/telemetry. This repository promises no
# telemetry (docs/SECURITY.md, docs/DEPLOY_LXC.md), so the value the caller gave
# is captured here and forced back after variables() has run — the only point
# where it sticks. Default is off; DIAGNOSTICS=yes opts in.
_diag_opt_in="${DIAGNOSTICS:-no}"

header_info "$APP"
variables
DIAGNOSTICS="$_diag_opt_in"
export DIAGNOSTICS
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
