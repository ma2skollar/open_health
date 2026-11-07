#!/usr/bin/env bash
# Render the dashboard in headless Chromium and capture screenshots plus the
# post-JavaScript DOM.
#
# Everything runs inside one container: the dashboard binds loopback, so the
# browser must share the container's network namespace (a published Docker port
# cannot reach a loopback-only listener).
#
# Usage (Git Bash, from the repo root):
#   MSYS_NO_PATHCONV=1 docker run --rm --platform linux/amd64 \
#     -v "C:/path/to/oura:/w" -v oura-target-lead:/w/target -w /w \
#     rust:1-bookworm bash /w/scripts/ui-check.sh
set -uo pipefail

PORT="${PORT:-8090}"
OUT="${OUT:-/w/out}"
RUN=/tmp/oura-ui
ROOT="$(pwd)"

echo "== deps =="
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq libdbus-1-3 curl chromium >/dev/null

rm -rf "$RUN"; mkdir -p "$RUN" "$OUT"
[ -x "$ROOT/target/release/oura" ] || { echo "FAIL: build the CLI first"; exit 1; }

echo "== start dashboard (no ring database at all) =="
( cd "$RUN" && "$ROOT/target/release/oura" --db "$RUN/oura.db" dashboard --port "$PORT" >"$RUN/srv.log" 2>&1 ) &
APP=$!
sleep 3

echo "== seed four days of manual entries =="
for day in 2026-11-03 2026-11-04 2026-11-05 2026-11-06 2026-11-07; do
    curl -s -X POST -H 'X-Oura-Dash: 1' -H 'Content-Type: application/json' \
        -d "{\"local_date\":\"$day\",\"kind\":\"calories_eaten\",\"value\":2100}" \
        "http://localhost:$PORT/api/manual/entries" >/dev/null
    curl -s -X POST -H 'X-Oura-Dash: 1' -H 'Content-Type: application/json' \
        -d "{\"local_date\":\"$day\",\"kind\":\"weight_kg\",\"value\":75.5}" \
        "http://localhost:$PORT/api/manual/entries" >/dev/null
done
curl -s "http://localhost:$PORT/api/manual/series?days=30" | head -c 300; echo

shot() {
    local w="$1" h="$2" name="$3"
    chromium --headless --no-sandbox --disable-gpu --hide-scrollbars \
        --window-size="$w,$h" --virtual-time-budget=5000 \
        --screenshot="$OUT/$name" "http://localhost:$PORT/" >>"$RUN/chromium.log" 2>&1
    echo "  $name: $(stat -c%s "$OUT/$name" 2>/dev/null || echo MISSING) bytes"
}

echo "== screenshots =="
shot 375 900 ui-375.png
shot 1280 900 ui-1280.png

echo "== post-JavaScript DOM =="
chromium --headless --no-sandbox --disable-gpu --virtual-time-budget=5000 \
    --dump-dom "http://localhost:$PORT/" >"$OUT/ui-dom.html" 2>>"$RUN/chromium.log"

echo "  manual-panel present: $(grep -c 'manual-panel' "$OUT/ui-dom.html")"
echo "  recent rows rendered: $(grep -o 'manual-trow' "$OUT/ui-dom.html" | wc -l)"
echo "  chart svgs rendered:  $(grep -o 'manual-svg' "$OUT/ui-dom.html" | wc -l)"
echo "  ring error shown:     $(grep -c 'opening DB' "$OUT/ui-dom.html")"

echo "== chromium log (tail) =="
tail -5 "$RUN/chromium.log"
echo "== server log =="
cat "$RUN/srv.log"
kill "$APP" 2>/dev/null
echo "== done =="
