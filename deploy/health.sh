#!/usr/bin/env bash
# Readiness probe for the Oura dashboard host.
#
#   ./deploy/health.sh
#
# Exit 0 when everything checked is healthy, 1 otherwise. Output is redacted:
# it never prints a token, a key, or a health payload.
set -uo pipefail

# The service reads /etc/oura/oura.env; read the same file so the probe uses the
# port and listen address the dashboard was actually given.
CONF="${OURA_CONF:-/etc/oura/oura.env}"
# shellcheck disable=SC1090
[ -r "$CONF" ] && . "$CONF"

PORT="${OURA_PORT:-8090}"
DATA_DIR="${OURA_DATA_DIR:-/srv/oura}"
MIN_FREE_MB="${MIN_FREE_MB:-500}"
MANUAL_DB="${OURA_MANUAL_DB:-$DATA_DIR/data/manual.db}"
STALE_UPLOAD_HOURS="${STALE_UPLOAD_HOURS:-48}"

failures=0
ok() { printf 'ok    %s\n' "$*"; }
bad() { printf 'FAIL  %s\n' "$*"; failures=$((failures + 1)); }

# 1. Service state, when systemd is present.
if command -v systemctl >/dev/null 2>&1; then
    state="$(systemctl is-active oura-dashboard.service 2>/dev/null || true)"
    if [ "$state" = "active" ]; then ok "oura-dashboard.service is active"; else bad "oura-dashboard.service is ${state:-unknown}"; fi
else
    ok "systemctl not present — service state not checked"
fi

# 2. The dashboard answers on the address it is configured to listen on. A
# specific bind does not serve loopback, so "localhost" alone would report a
# false failure; a wildcard bind does serve loopback, so it is tried first and
# the configured address is the fallback.
probe() {
    curl -s -o /dev/null -w '%{http_code}' --max-time 5 "http://$1:${PORT}/api/summary" 2>/dev/null || true
}
code="$(probe localhost)"
probed="localhost"
if [ "$code" != 200 ] && [ -n "${OURA_BIND:-}" ] && [ "$OURA_BIND" != localhost ]; then
    code="$(probe "$OURA_BIND")"
    probed="$OURA_BIND"
fi
case "$code" in
    200) ok "dashboard answers on ${probed}:${PORT} (HTTP 200)" ;;
    000|"") bad "dashboard did not answer on ${probed}:${PORT}" ;;
    *) bad "dashboard answered HTTP ${code} on ${probed}:${PORT}" ;;
esac

# 3. Free disk above the floor.
if [ -d "$DATA_DIR" ]; then
    free_mb="$(df -Pm "$DATA_DIR" | awk 'NR==2 {print $4}')"
    if [ -n "$free_mb" ] && [ "$free_mb" -ge "$MIN_FREE_MB" ]; then
        ok "free space ${free_mb} MiB (floor ${MIN_FREE_MB} MiB)"
    else
        bad "free space ${free_mb:-unknown} MiB is below the ${MIN_FREE_MB} MiB floor"
    fi
else
    bad "data directory ${DATA_DIR} is missing"
fi

# 4. The manual database opens and is not corrupt.
if [ ! -e "$MANUAL_DB" ]; then
    ok "manual database not created yet (${MANUAL_DB})"
elif command -v sqlite3 >/dev/null 2>&1; then
    if [ "$(sqlite3 "file:${MANUAL_DB}?mode=ro" 'PRAGMA quick_check;' 2>/dev/null || true)" = "ok" ]; then
        ok "manual database opens read-only and passes quick_check"
    else
        bad "manual database failed quick_check or could not be opened read-only"
    fi
else
    ok "sqlite3 not installed — manual database not checked"
fi

# 5. Newest accepted collector upload, once ingestion exists.
STATE="${OURA_HUB_STATE_DIR:-}/hub-state.json"
if [ -n "${OURA_HUB_STATE_DIR:-}" ] && [ -f "$STATE" ] && command -v jq >/dev/null 2>&1; then
    age_h="$(jq -r '[.collectors[]?.last_accepted_unix] | max // 0 | if . == 0 then "none" else ((now - .) / 3600 | floor) end' "$STATE" 2>/dev/null || echo none)"
    if [ "$age_h" = "none" ]; then
        ok "no accepted upload yet"
    elif [ "$age_h" -le "$STALE_UPLOAD_HOURS" ]; then
        ok "newest accepted upload is ${age_h} h old"
    else
        bad "newest accepted upload is ${age_h} h old (limit ${STALE_UPLOAD_HOURS} h)"
    fi
else
    ok "hub state not configured yet — upload freshness not checked"
fi

if [ "$failures" -eq 0 ]; then
    printf '\nhealthy\n'
    exit 0
fi
printf '\n%d check(s) failed\n' "$failures"
exit 1
