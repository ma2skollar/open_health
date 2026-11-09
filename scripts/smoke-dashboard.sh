#!/usr/bin/env bash
# Start the model-free dashboard with no ring, no dataset, and probe its state.
# Also exercises the ring-less manual metrics API (calories + weight, its own
# --manual-db), asserts the removed DNA/blood surface is gone, and checks the
# binary carries no third-party endpoints (ADR 0006).
# Usage (Git Bash, from the repo root):
#   MSYS_NO_PATHCONV=1 docker run --rm -v "C:/path/to/oura:/w" \
#     -v oura-cargo:/usr/local/cargo/registry -v oura-target:/w/target \
#     -w /w rust:1-bookworm bash /w/scripts/smoke-dashboard.sh
set -uo pipefail

PORT="${PORT:-8091}"
HOST=localhost
RUN=$(mktemp -d /tmp/oura-smoke.XXXXXX)
ROOT="$(pwd)"
BIN="$ROOT/target/release/oura"


echo "== runtime deps (dashboard-only; BLE would need a D-Bus session) =="
if ! command -v curl >/dev/null || ! command -v strings >/dev/null || ! ldconfig -p | grep -q 'libdbus-1.so.3'; then
  apt-get update -qq
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq libdbus-1-3 curl binutils >/dev/null
fi

[ -x "$BIN" ] || { echo "FAIL: $BIN missing - run scripts/docker-build-linux.sh first"; exit 1; }

status=0
fail() { echo "  FAIL: $1"; status=1; }

echo "== static: no third-party endpoints in the binary (ADR 0006) =="
for needle in "agent-backend.thomas.md" "ftp.ebi.ac.uk" "pgscatalog.org"; do
  if strings -a "$BIN" | grep -qF -- "$needle"; then
    fail "binary still references $needle"
  else
    echo "  ok: no $needle"
  fi
done

echo "== start dashboard (no DB, no ring key, no models) =="
( cd "$RUN" && exec "$BIN" --db "$RUN/oura.db" dashboard --port "$PORT" --manual-db "$RUN/manual.db" >"$RUN/srv.log" 2>&1 ) &
PID=$!
trap 'kill "$PID" 2>/dev/null || true' EXIT
sleep 3
kill -0 "$PID" 2>/dev/null || { cat "$RUN/srv.log"; exit 1; }

check_body() {
  local path="$1" expect="$2" got
  got=$(curl -s "http://$HOST:$PORT$path")
  echo "GET $path -> ${got:0:90}"
  if printf '%s' "$got" | grep -qF -- "$expect"; then echo "  ok"; else fail "expected body to contain: $expect"; fi
}

check_code() {
  local path="$1" expect="$2" got
  got=$(curl -s -o /dev/null -w "%{http_code}" "http://$HOST:$PORT$path")
  echo "GET $path -> HTTP $got"
  if [ "$got" = "$expect" ]; then echo "  ok"; else fail "expected HTTP $expect"; fi
}

check_code "/"             200
check_body "/app.js"      "api/summary"
check_body "/styles.css"  "body"
check_body "/api/summary" "No ring data yet"

echo "== removed surface must be gone (ADR 0006) =="
check_code "/dna"             404
check_code "/dna.js"          404
check_code "/blood"           404
check_code "/blood.css"       404
check_code "/api/dna/files"   404
check_code "/api/blood/report" 404
check_code "/icons/dna.svg"   404
check_code "/icons/drop.svg"  404

echo "== kept surface still served =="
check_code "/icons/act-default.svg" 200

echo "== manual metrics API (works with no ring and no ring DB) =="
D=$(date +%F)
BAD='2099-01-01'
CT='-H Content-Type:application/json'
CSRF='-H X-Oura-Dash:1'
api() { curl -s "$@"; }

got=$(api "http://$HOST:$PORT/api/manual/entries")
echo "GET /api/manual/entries (empty DB) -> ${got:0:120}"
printf '%s' "$got" | grep -qF '"entries":[]' && echo "  ok: empty" || fail 'expected {"entries":[]} on an empty DB'

r=$(api $CSRF $CT -X POST -d "{\"local_date\":\"$D\",\"kind\":\"calories_eaten\",\"value\":2100,\"note\":\"smoke\"}" "http://$HOST:$PORT/api/manual/entries")
echo "POST calorie -> $r"
printf '%s' "$r" | grep -qF '"outcome":"inserted"' && echo "  ok: inserted" || fail "expected outcome inserted"

got=$(api "http://$HOST:$PORT/api/manual/entries?from=$D&to=$D&kind=calories_eaten")
echo "GET it back -> $got"
printf '%s' "$got" | grep -qF '"value":2100.0' && echo "  ok: exact value" || fail "expected the 2100 kcal value back"

r=$(api $CSRF $CT -X POST -d "{\"local_date\":\"$D\",\"kind\":\"calories_eaten\",\"value\":1950}" "http://$HOST:$PORT/api/manual/entries")
echo "POST same date+kind again -> $r"
printf '%s' "$r" | grep -qF '"outcome":"updated"' && echo "  ok: updated" || fail "expected outcome updated"
n=$(api "http://$HOST:$PORT/api/manual/entries?from=$D&to=$D&kind=calories_eaten" | grep -o '"local_date"' | wc -l)
[ "$n" -eq 1 ] && echo "  ok: exactly one row for the date+kind" || fail "expected exactly 1 row, got $n"

r=$(api $CSRF $CT -X POST -d "{\"local_date\":\"$D\",\"kind\":\"weight_kg\",\"value\":75.5}" "http://$HOST:$PORT/api/manual/entries")
echo "POST weight on the same date -> $r"
printf '%s' "$r" | grep -qF '"outcome":"inserted"' && echo "  ok: weight inserted" || fail "expected weight inserted"
got=$(api "http://$HOST:$PORT/api/manual/entries?from=$D&to=$D")
printf '%s' "$got" | grep -qF 'calories_eaten' && printf '%s' "$got" | grep -qF 'weight_kg' && echo "  ok: both kinds coexist" || fail "expected both kinds on one date"

code=$(api -o "$RUN/bad.json" -w "%{http_code}" $CSRF $CT -X POST -d "{\"local_date\":\"$BAD\",\"kind\":\"calories_eaten\",\"value\":-5}" "http://$HOST:$PORT/api/manual/entries")
echo "POST invalid (-5 kcal) -> HTTP $code $(cat "$RUN/bad.json")"
[ "$code" = "400" ] && echo "  ok: rejected with 400" || fail "expected HTTP 400 for a negative value"
n=$(api "http://$HOST:$PORT/api/manual/entries?from=$BAD&to=$BAD" | grep -o '"local_date"' | wc -l)
[ "$n" -eq 0 ] && echo "  ok: nothing written" || fail "the invalid entry was written"

code=$(api -o "$RUN/csrf.json" -w "%{http_code}" -X POST --data-binary 'X-Oura-Dash: 1' "http://$HOST:$PORT/api/manual/entries")
echo "POST with X-Oura-Dash only inside the body -> HTTP $code"
[ "$code" = "403" ] && echo "  ok: 403 (a body line cannot satisfy the guard)" || fail "expected HTTP 403 for a body-only CSRF header"

r=$(api $CSRF -X DELETE "http://$HOST:$PORT/api/manual/entries?date=$D&kind=calories_eaten")
echo "DELETE the calorie entry -> $r"
printf '%s' "$r" | grep -qF '"deleted":true' && echo "  ok: deleted" || fail "expected deleted true"
got=$(api "http://$HOST:$PORT/api/manual/entries?from=$D&to=$D&kind=calories_eaten")
printf '%s' "$got" | grep -qF 'calories_eaten' && fail "expected the deleted entry to be gone" || echo "  ok: gone"

hdr=$(api -D - -o "$RUN/manual-export.csv" "http://$HOST:$PORT/api/manual/export" | tr -d '\r')
printf '%s' "$hdr" | grep -qiF 'content-type: text/csv' && echo "  ok: text/csv" || fail "expected text/csv"
printf '%s' "$hdr" | grep -qiF 'filename="manual-entries.csv"' && echo "  ok: attachment name" || fail "expected the manual-entries.csv attachment"
head -n 1 "$RUN/manual-export.csv" | grep -qF 'local_date,kind,value,note,source,created_at,updated_at' && echo "  ok: CSV header line" || fail "expected the RFC 4180 header line"

got=$(api "http://$HOST:$PORT/api/manual/series?days=30")
echo "GET /api/manual/series?days=30 -> ${got:0:160}"
printf '%s' "$got" | grep -qF '"unit":"kg"' && echo "  ok: series unit" || fail "expected unit kg in the series"

echo "== server log =="
cat "$RUN/srv.log"
echo "== files written next to the DB =="
ls -la "$RUN"

kill "$PID" 2>/dev/null
echo "== result: $([ $status -eq 0 ] && echo PASS || echo FAIL) =="
exit "$status"
