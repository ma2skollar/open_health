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
# Recommended resources: 2 vCPU, 4096 MiB RAM, 8 GiB disk.
#
# The build is the peak, not the dashboard: rustup plus a release build with
# LTO. 2048 MiB also completed a build, with no headroom to spare, and 2 GiB is
# what docs/DEPLOY_LXC.md lists as the bare minimum. At runtime the service is
# idle. The Advanced dialog overrides all three (var_cpu, var_ram, var_disk).
var_cpu="${var_cpu:-2}"
var_ram="${var_ram:-4096}"
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
  cd /root/oura || exit 1
  git pull --ff-only
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
  cargo build --release -p oura-cli -p oura-backup
  # cargo build alone leaves /srv/oura/bin/oura on the previous revision, so the
  # built binaries still have to be installed.
  ./deploy/install.sh --bin ./target/release/oura --backup-bin ./target/release/oura-backup || true
  systemctl restart oura-dashboard.service
  if ! systemctl is-active --quiet oura-dashboard.service; then
    msg_error "oura-dashboard.service did not come back up"
    exit 1
  fi
  msg_ok "Updated ${APP}"
  msg_ok "Updated successfully!"
  exit
}

start
build_container
description

msg_ok "Completed successfully!\n"
echo -e "${CREATING}${GN}${APP} setup has been successfully initialized!${CL}"
echo -e "${INFO}${YW}The dashboard binds loopback only. Reach it from your workstation with:${CL}"
echo -e "${GATEWAY}${BGN}ssh -N -L 8090:[IP_ADDRESS]:8090 root@${IP}${CL}"
echo -e "${INFO}${YW}then open http://[IP_ADDRESS]:8090 in a browser.${CL}"
echo -e "${INFO}${YW}Health check: pct exec ${CTID} -- /root/oura/deploy/health.sh${CL}"
