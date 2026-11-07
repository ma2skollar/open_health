#!/usr/bin/env bash
# The engine's update entrypoint is named after APP ("Open Health" -> openhealth),
# so /usr/bin/update inside the container fetches this path from the fork. The
# real script is ct/oura.sh; this only forwards to it.
#
#   bash -c "$(curl -fsSL https://raw.githubusercontent.com/ma2skollar/open_health/main/ct/openhealth.sh)"
set -euo pipefail

BASE="${OURA_SCRIPTS_BASE:-https://raw.githubusercontent.com/ma2skollar/open_health/main}"
exec bash -c "$(curl -fsSL "${BASE}/ct/oura.sh")"
