#!/usr/bin/env bash
# Kept for existing links. The setup script now lives at ct/oura.sh, in the
# community-scripts layout the container's `update` command expects.
#
#   bash -c "$(curl -fsSL https://raw.githubusercontent.com/ma2skollar/open_health/main/deploy/pve-setup.sh)"
#
# Overrides: CTID=222, var_cpu, var_ram, var_disk, var_brg, MODE=default|advanced.
set -euo pipefail

BASE="${OURA_SCRIPTS_BASE:-https://raw.githubusercontent.com/ma2skollar/open_health/main}"
exec bash -c "$(curl -fsSL "${BASE}/ct/oura.sh")"
