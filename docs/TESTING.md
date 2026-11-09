# TESTING

## Profile persistence and logged-weight feedback (2026-11-09)

Verified in Linux containers with an isolated ring-less database:

- Dashboard tests: 56 passed; shared summary tests: 74 passed.
- `cargo check --locked -p oura-core` and locked release CLI build passed.
- `scripts/smoke-dashboard.sh` passed.
- Chromium through the local nginx HTTPS proxy saved decimal-comma fallback
  weight `82,5`, age and height; reopening, page reload, and server restart
  retained the values without ring data.
- With the manual panel configured for pounds, canonical entries of 90 kg and
  94 kg produced a read-only 92 kg profile average. Saving age retained the
  original fallback; deleting both observations restored 82.5 kg.
- Desktop and 375×667 dialog screenshots checked; phone dialog had no horizontal
  overflow. No native iOS runtime or populated production ring data was exercised.

## Dashboard changes: required Docker verification loop

Production runs in LXC; the checkout is local only. Verify every dashboard change
in a Docker instance, not only a native process. Use an isolated manual database;
never mount production health data into a smoke run. Rebuild after asset changes,
because release binaries embed the web files.

In a Linux Rust container with the repository at `/w`, persistent Cargo registry
and target volumes, and pkg-config, libsqlite3-dev, libdbus-1-dev installed:

```sh
cargo test --locked -p oura-cli dashboard::
cargo build --release --locked -p oura-cli
PORT=8094 bash scripts/smoke-dashboard.sh
```

Start an isolated dashboard, then run framing probes in the same container:

```sh
mkdir -p /tmp/dashboard-check
cd /tmp/dashboard-check
/w/target/release/oura --db /tmp/dashboard-check/oura.db dashboard \
  --port 8092 --manual-db /tmp/dashboard-check/manual.db \
  --public-origin https://localhost:18443
# In another container shell:
python3 /w/scripts/check-dashboard-framing.py localhost 8092 https://localhost:18443
```

For browser verification, put a TLS proxy in the dashboard's network namespace;
`scripts/dashboard-proxy-test.conf` expects disposable certificate/key files at
`/tmp/health-proxy.crt` and `/tmp/health-proxy.key`. Publish TLS port 8443 as 18443.
Exercise create/edit/delete and decimal-comma input through the proxy, then check
independent disclosures and dialog bounds at phone, tablet, desktop and short
landscape sizes. Follow `DEPLOY_LXC.md` for production proxy configuration.

Observed on 2026-11-09, using `health-build-1109` and `health-runtime-1109` Docker
containers: locked release build passed; API smoke passed; all 15 framing probes
passed, including delayed/split headers and bodies, chunked bodies, size limits,
ambiguous framing, and public/foreign origins. Commands:

```sh
docker exec health-build-1109 sh -c "cargo test --locked -p oura-cli dashboard:: && cargo build --release --locked -p oura-cli"
docker exec -e PORT=8094 health-build-1109 bash /w/scripts/smoke-dashboard.sh
docker exec health-runtime-1109 python3 /w/scripts/check-dashboard-framing.py localhost 8092 https://localhost:18443
```

Raw final output:

```text
test result: ok. 50 passed; 0 failed; 0 ignored; 0 measured; 7 filtered out; finished in 0.16s
== result: PASS ==
PASS public origin with foreign Host: HTTP/1.1 403 Forbidden
PASS all framing probes
```

All commands exited 0. Chromium through nginx HTTPS saved `75,5` as 75.5 kg,
saved calories and profile weight, and rejected malformed `75,5.0`. Viewports
320, 375, 390, 768 and 1280 pixels wide plus 667×375 landscape had no horizontal
overflow; dialog/date bounds and independent disclosure state were checked.
Native iPhone Safari, its date-picker popup/software keyboard, and populated ring
reports still require device/data acceptance; this run used synthetic manual data.


Test inventory, mapped to phases. Status is only what was actually run.

## Executed so far

| Check | Command | Result |
|---|---|---|
| Reproducible Linux build, locked dependencies | `scripts/docker-build-linux.sh` | **PASS** — `cargo build --release --locked -p oura-cli` succeeded; `Cargo.lock` unchanged after build; artifact `target/release/oura` 10,845,328 bytes |
| Dashboard runs with no ring, no DB, no models | `scripts/smoke-dashboard.sh` | **PASS** — `GET /` 200, `GET /app.js` 200, `GET /api/summary` 200 with `{"error":"opening DB"}`; `profile.json` written next to the DB; no fabricated data |
| DNA/blood surface removed (ADR 0006) | `scripts/smoke-dashboard.sh` | **PASS** — `/dna`, `/dna.js`, `/blood`, `/blood.css`, `/api/dna/files`, `/api/blood/report`, `/icons/dna.svg`, `/icons/drop.svg` all HTTP **404**; `/icons/act-default.svg` still **200** |
| No third-party endpoints in the built binary | `strings -a target/release/oura` | **PASS** — none of `agent-backend.thomas.md`, `ftp.ebi.ac.uk`, `pgscatalog.org` present |
| Locked rebuild after dependency removal | `cargo build --release --locked -p oura-cli` | **PASS** — lock re-resolved once (icu/idna/ureq dropped), artifact 7,657,840 bytes; a later `--locked` build succeeds |
| Manual metric store (D1) | `cargo test -p oura-manual` | **PASS** — 19 tests: round trip, absent != zero, both kinds on one date, upsert keeps provenance, 11 validation rejections write nothing, inclusive range, export, newer schema refused untouched, reopen persistence, `manual.db` is `0600` |
| Import provenance and rollback (D7, part 1) | `cargo test -p oura-manual` | **PASS** — 9 `tests/batch.rs`: batch round trip and newest-first listing, `batch_entries` excludes hand-written rows, rollback removes unchanged rows it created, rollback **keeps** an entry edited after the import and counts it, rollback **keeps** a row the import only overwrote, other batches untouched, disappeared entries counted, empty batch, steps round trip + range validation |
| CSRF/origin guard (S2) | `cargo test -p oura-cli` | **PASS** — 8 `request_guard_tests`: a body line cannot satisfy the guard, a foreign `Origin` is refused, a matching one accepted |
| Import framework (D7, part 2) | `cargo test -p oura-import` | **PASS** — 9 tests: parse/reject counts with source lines, dry run writes nothing and creates no batch, apply then re-import is a no-op, refused rows are never written (blank / `-` / `NA` are not 0), impossible dates refused, dry run reports changed vs stored, rollback undoes the import and keeps a later edit, step precedence is respected and never summed, unmapped column is an error, missing file names its path |
| Ring key file permissions (S4) | `cargo test -p oura-cli` | **PASS** — 3 `secret_file::tests`: exact bytes, new file `0600`, pre-existing wide file tightened on rewrite |
| Manual helpers — dates, series, trend, lb conversion, CSV, POST shape (D3) | `cargo test -p oura-cli manual_api` | **PASS** — 9 `dashboard::manual_api_tests`: epoch→civil date conversion incl. a leap day, empty store → empty arrays, a trend emitted only with a full 7-entry window, calories/weight independent series, kg→lb display rounding, RFC 4180 quoting and header, `kind` aliases, POST body stamping and rejection, unit parsing |
| Manual metrics API + Daily entry UI, ring-less (D3) | `scripts/smoke-dashboard.sh` in `rust:1-bookworm` | **PASS** — `== result: PASS ==` (exit 0). Empty store → `{"entries":[]}`; POST 2100 kcal read back as `"value":2100.0`; re-POST → `"outcome":"updated"` with exactly one row; weight coexists with calories on one date; `-5` kcal → HTTP **400** and nothing written; `X-Oura-Dash` **only in the request body** → HTTP **403**; DELETE → `{"deleted":true}` then the entry is gone; `GET /api/manual/export` → `text/csv`, `filename="manual-entries.csv"`, RFC 4180 header; `GET /api/manual/series?days=30` → `"unit":"kg"`; a `lb` run returns `"unit":"lb"` with a converted value; `manual.db` created `0600` next to the ring DB |
| Deploy units and scripts (D8) | `systemd-analyze verify`, `shellcheck`, `install.sh --dry-run` | **PASS** — `verify` exit 0 for all three units (installed at `0644`, with an `ExecStart` binary present at `/srv/oura/bin/oura`); `shellcheck` clean; dry-run changes nothing (`/etc/oura/oura.env` not created) and a second run prints the same actions |
| Backup, verify, restore (D4) | `cargo test -p oura-backup` | **PASS** — 10 tests: UTC timestamp formatting incl. a leap day, create/verify round trip with schema version in the manifest, a WAL-state backup a plain copy would lose, tampered file fails its checksum, truncated file refused, missing file named, restore refuses an existing destination without `--force`, a successful restore reproduces the backed-up rows, a failed restore leaves the destination byte-identical, no-source and list-ordering behaviour |
| Health probe (D8) | `deploy/health.sh` | **PASS** — against a running dashboard all five checks pass → `healthy`, exit 0; against a port with nothing listening → exit 1; with the data dir missing → `1 check(s) failed` |
| ARM64 compile | `cargo check --locked -p oura-cli` under `--platform linux/arm64` | **PASS** — aarch64 (emulated), 2m43s. **Check level only**: no ARM64 binary linked, no Bluetooth tested |
| TLS egress removed (ADR 0006) | `grep -E '^name = "(rustls\|ureq\|webpki\|ring)"$' Cargo.lock` | **PASS** — zero matches; the CLI now has no TLS stack at all, so TLS must come from the reverse proxy |

Toolchain used: `rustc 1.99.1 (58cf8f9c3 2026-10-13)` in `rust:1-bookworm`.
Rust 1.83 fails (`clap_lex 1.1.0` requires `edition2024`); the minimum is 1.85.

Runtime dependency discovered by running the binary: `libdbus-1-3`. Without it
the process aborts with `error while loading shared libraries: libdbus-1.so.3`.

## Ingestion (D5)

Run only the two ingestion crates, with isolated temporary directories and
synthetic SQLite fixtures generated by `rusqlite`:

```sh
docker run --rm --platform linux/arm64 -v C:/Users/kolla/Desktop/oura:/w -v oura-cargo:/usr/local/cargo/registry -v oura-target:/w/target -w /w rust:1-bookworm bash -c "apt-get update -qq && apt-get install -y -qq build-essential pkg-config libsqlite3-dev && rustup component add rustfmt && cargo fmt -p oura-hub -p oura-protocol-v1 && cargo test -p oura-hub -p oura-protocol-v1"
```

Executed in `rust:1-bookworm` under emulated Linux ARM64: **PASS**, 26 hub
integration tests and 6 current shared-protocol unit tests; both doc-test targets
passed (0 tests). The command exited 0. Continuity regressions prove that appending
unanchored history may move projected timestamps without losing raw events, and
that a larger row count cannot disguise removal of earlier raw history.
`rustup component add rustfmt` is required because the image does not include
the formatter. Use `bash -c`, not a login shell that resets Cargo's PATH.

The acceptance suite covers all 14 cases in `handoffs/D5-ingestion.md`:
activation/readability, checksum and both size mismatches, truncated/non-SQLite
files, missing tables, unsupported versions, corrupt-page integrity failure,
duplicates, stale generations, restart convergence, explicit epoch promotion,
authentication before writes, actual/declared byte limits, and status redaction.
Additional tests cover history regression, path confinement, request deadlines,
and rate limiting. Every test owns its temporary data root; none reads a local
ring database or requires hardware.

Evidence convention: report the exact Docker command and copy its raw final
test-output lines, including individual acceptance results, counts, doc-test
results, and process exit status. Do not replace raw output with a claimed pass
or infer Docker results from a native run. Refresh this evidence after changes.

## Collector and simulator (D6)

The collector uses one owner for all Bluetooth commands. Simulator tests use
the same coordinator, durable SQLite backup/outbox and D5 ingestion code, with
a scripted ring and fake clock. No scenario requires Bluetooth or a ring.

Run the acceptance suite and all simulator scenarios:

```sh
docker run --rm --platform linux/amd64 -v C:/Users/kolla/Desktop/oura:/w -v oura-cargo:/usr/local/cargo/registry -v oura-target:/w/target -w /w rust:1-bookworm bash -c "apt-get update -qq && apt-get install -y -qq build-essential pkg-config libsqlite3-dev libdbus-1-dev && cargo test -p oura-collector -p oura-collector-sim && cargo test -p oura-ble --features ble && cargo run -p oura-collector-sim -- demo && cargo run -p oura-collector-sim -- reset-demo"
```

Executed the command above in `rust:1-bookworm`: **PASS**, exit 0. Required
collector/simulator suite: 63 tests (46 collector, 17 simulator), all passing;
both doc-test targets passed. Additional BLE-feature suite: 13 tests passed.
All 13 scripted demo scenarios and the safe demo reset printed `result: PASS`.
The final run followed the D5 raw-event-retention continuity fix; no fake time
anchors or narrowed scenarios were used to bypass continuity validation.

Executed additional hardware-free boundary verification:
`cargo test -p oura-ble --features ble` passed all 13 tests and its doc-test target.
`cargo check -p oura-collector --features ble` also passed on Linux x86_64.
The scripted transport tests do not open a Bluetooth adapter.

For one full scenario log, use
`cargo run -p oura-collector-sim -- scenario normal-sync`.
Every run prints `identity: demo` and owns a private temporary sandbox. The
simulator accepts no database-root argument and cannot target manual or
production ring data. `reset-demo` operates only on an owned demo sandbox;
each invocation starts with fresh data, never a shared production store.

Scenarios cover normal sync, absent-ring backoff, offline buffering/recovery,
lost-ack duplicate delivery, stale generations, new-epoch continuity review,
partial-history resume, live-HR stop/timeout/disconnect/shutdown, busy/cancelled
commands, and command expiry after reconnect. Persistence tests additionally
cover wake windows/timezones, reboot recovery, WAL-aware snapshots, queue
limits, disk reserve, and guarded demo reset. Rejected snapshots remain visible
for operator review; they are never silently discarded.

Implementation notes: snapshot filenames use a SHA-256 epoch prefix rather
than the literal epoch, because v1 epochs are opaque and may contain `/`.
Metadata preserves the exact epoch. `feature_mode` addresses upstream's
`DAYTIME_HR` mode, not a fabricated global feature bitmask. Older pending
snapshots are retained rather than superseded. Transient uploads retry with a
capped interval even after the scheduler's reconnect-attempt budget is spent.
Delivery shares the actor's queue ownership and attempts at most one bounded
upload between scheduler turns.
A D5 `accepted: true` acknowledgement with `message: "pending_review"` confirms
durable receipt without changing the active epoch. Its `active_generation`
belongs to the old epoch and may be lower than the new generation; the collector
must not mistake that for a failed upload.

The additional BLE-feature tests use upstream `oura-link::Transport` with
scripted packets, not an adapter. Cross-check the real transport on ARM64:

```sh
docker run --rm --platform linux/arm64 -v C:/Users/kolla/Desktop/oura:/w -v oura-cargo:/usr/local/cargo/registry -v oura-target-arm64:/w/target -w /w rust:1-bookworm bash -c "apt-get update -qq && apt-get install -y -qq build-essential pkg-config libsqlite3-dev libdbus-1-dev && cargo check -p oura-collector --features ble"
```
Executed the ARM64 command above: **PASS**. This is an emulated compilation
check, not a linked Pi runtime or hardware test.

**Compilation and simulator success are not hardware verification.** Nothing
here proves Ring 5 pairing, Bluetooth operation, or any measured metric. Real
hardware checks remain pending in `HARDWARE_ACCEPTANCE.md`.

## Required test matrix (from the project brief)

Rows remain **pending** unless executed evidence above records a result. Tests
use synthetic fixtures only; no personal health data is committed.

### Phase 2 — manual metrics
- [x] manual CRUD: create/view/edit/delete on historical dates (HTTP layer proven by
      `scripts/smoke-dashboard.sh`; range bounds and provenance by `cargo test -p oura-manual`)
- [x] units: kg canonical, display conversion, rounding (storage stays kg)
- [x] missing value is not zero; calorie and weight independent
- [x] duplicate-day semantics; provenance and timestamps
- [ ] timezone/DST correctness for local-calendar dates (deliberately out of scope for D3:
      `local_date` is stored verbatim, never shifted)
- [x] export completeness (`export_all` returns every row; RFC 4180 header and quoting)
- [ ] manual data survives ring snapshot activation
- [x] backup/restore round trip (`cargo test -p oura-backup`)

### Phase 3 — import
- [ ] dry run / preview; explicit mapping; rejected-row report
- [ ] rerun safety; rollback preserving later unrelated edits
- [ ] before/after reconciliation; overlapping step-source precedence

### Phase 4 — ingestion
- [ ] upload auth failure; size and rate limits
- [ ] wrong checksum; wrong size; truncated/corrupt database; unexpected schema
- [ ] duplicate generation; stale generation cannot roll data backward
- [ ] collector epoch change; partial replacement history is refused without proof
- [ ] restart during upload; restart during activation
- [ ] demo/production identity separation enforced server-side
- [x] demo reset cannot delete real records (D6 owned temporary sandbox and guarded reset)

### Phase 5 — controls
- [x] command expiry; duplicate result; busy; cancel; timeout
- [x] receipt acknowledgement vs execution completion
- [x] uncertain outcome reconciliation for non-idempotent actions (software journal and orphaned live-mode reconciliation; exactly-once physical execution is not claimed)
- [x] live-session cleanup on stop/completion/timeout/disconnect/shutdown
- [x] expired interactive command does not execute after reconnect

### Phase 6 — collector
- [x] BLE trait fake: scheduling, no overlapping jobs, bounded reconnects,
      backoff when absent, reboot recovery, timezone/schedule config
- [ ] snapshot creation under WAL with a concurrent writer
      (WAL-backed consistent copies are covered; continuous concurrent-writer stress remains pending.)

### Cross-cutting
- [ ] dashboard with no ring and no data (partially proven by smoke run)
- [ ] no proprietary model present; every metric labelled measured/estimated/unavailable

## Explicitly not testable now

Recorded in `HARDWARE_ACCEPTANCE.md`: Pi 4 Bluetooth operation, Ring 5
pairing/authentication, feature configuration, real history collection, real
clock correctness, real metric availability, step aggregation, live HR and
teardown, out-of-range recovery, missed-day retention, battery impact,
real-world storage growth.
