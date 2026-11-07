# Handoff D6 — collector, BLE abstraction, simulator

## Objective

The Pi-side collector and the simulator that exercises it without hardware. One
coordinator owns every BLE operation; delivery uses the D5 snapshot contract; the
simulator drives the same code with a fake transport.

## Context (decided — read before starting)

- `docs/DECISIONS/0004` fixes snapshot delivery; `docs/DECISIONS/0005` fixes the
  Pi-initiated authenticated WebSocket control channel.
- `docs/ARCHITECTURE.md` holds the v1 upload and control contracts. Implement
  them; if reality forces a change, change the doc and say so.
- `open_oura`'s `oura-link` already provides an async `Transport` trait and
  `OuraClient<T: Transport>` (the iOS build implements `Transport` itself). Reuse
  it; do not write a second BLE stack.
- The ring authentication key lives **only** on the Pi, `0600`. It is never
  uploaded, never logged, and never in a snapshot. Debug tracing must be off.
- Never run destructive ring procedures automatically. No factory reset, no DFU,
  no flight mode, no `rdata clear` outside an explicit, requested command.

## Files you own

`crates/oura-ble/**`, `crates/oura-collector/**`, `crates/oura-collector-sim/**`,
the workspace `members` array in the root `Cargo.toml`, `deploy/README.md` (only
the Pi section), `docs/TESTING.md`, `docs/HARDWARE_ACCEPTANCE.md`.

Do not edit `crates/oura-cli/**`, `crates/oura-hub/**`, `crates/oura-manual/**`.

## Part 1 — `oura-ble`

- A thin `RingTransport` boundary over `oura-link` so a fake can be swapped in
  without Bluetooth: connect/disconnect, `hardware_id`, `device_info`, `battery`,
  `drain_history(cursor)`, `feature_mode` get/set, bounded `live_hr(duration)`.
- The real implementation is feature-gated (`ble`) and compiles on
  `aarch64-unknown-linux-gnu`; the fake is always available.
- Timeouts on every operation; a connect or drain that exceeds its bound returns
  a typed error rather than hanging.

## Part 2 — `oura-collector`

One process, one owner of the link. No competing independent Bluetooth clients.

1. **Config** (file plus environment overrides): `collector_id`, `epoch`,
   ring name, key file path, database path, hub URL, collector token file,
   wake window (`HH:MM`–`HH:MM`), IANA timezone, retry/backoff bounds, outbox
   limits, `identity: demo|production`.
2. **Coordinator**: a single command queue draining one operation at a time.
   Commands: `sync_now`, `refresh_device_info`, `refresh_battery`,
   `collector_status`, `start_live_hr{max_secs}`, `stop_live_hr`. A second
   concurrent request returns `busy`, it is never queued twice. Queueing,
   cancellation, timeout, partial progress, and recovery all have defined,
   tested behaviour. Collection continues independently of the control channel.
3. **Scheduler**: a configurable wake window, no overlapping jobs, bounded
   reconnect attempts, backoff when the ring is absent, persistent state across
   reboot (last run, next run, consecutive failures, cursor). Never scans
   continuously without a measured reason.
4. **Snapshot**: a consistent copy of the local ring store via the SQLite backup
   API into `outbox/<epoch>-<generation>.db` with the D5 metadata (protocol
   version, schema version, collector id, epoch, generation, created time, size,
   SHA-256, build revision, identity). Generation is monotonic within the epoch
   and persisted.
5. **Deliverer**: authenticated upload with bounded exponential backoff; queue
   state persisted across reboot; queue limit and low-disk behaviour defined and
   **never a silent discard**; retry after the server returns; a newer snapshot
   may replace a queued older one **only** when it demonstrably contains
   everything the older one did (build it with the SQLite backup API from the
   same store, so it does). Duplicate delivery must be harmless.
6. **Control client**: outbound authenticated WebSocket. Receipt acknowledgement
   is separate from ring execution result. An expired interactive command must
   **not** execute after reconnect. Exactly-once physical execution across
   crashes is not claimed; uncertain outcomes are reported as uncertain, and a
   non-idempotent action reconciles actual state before any retry.
7. **Live HR**: bounded duration enforced locally as well as server-side, one
   session at a time, best-effort teardown on stop, completion, timeout,
   connection loss, and process shutdown. It must never block normal collection
   indefinitely.
8. **Status**: report Pi reachable / adapter present / ring recently seen / ring
   connected / operation running / ring unreachable / unknown as **distinct**
   states. Never infer ring availability from Pi connectivity.

## Part 3 — `oura-collector-sim`

Same contracts, no Bluetooth, deterministic.

- Fake transport with scriptable outcomes: present, absent, out-of-range, busy,
  failed, cancelled, expired, slow, partial history.
- Fake clock so scheduling and backoff are deterministic and fast.
- Scenarios: normal sync; ring absent with backoff; offline server then recovery
  with buffered snapshots; duplicate delivery; stale generation; collector
  re-provisioning with a new epoch; incomplete history; live HR stop, timeout,
  disconnect and shutdown; busy and cancelled commands; expired command after
  reconnect.
- A demo run must be clearly labelled (`identity: demo`), and the simulator must
  be unable to write into real manual or production ring data. Provide a safe
  demo reset that cannot delete real records.

## Acceptance / required evidence

`cargo test -p oura-collector -p oura-collector-sim` passes, plus a scripted
simulator run, covering every scenario above with deterministic assertions (the
fake clock makes these fast and repeatable). No test may require Bluetooth.

Run in Docker (`rust:1-bookworm`, `build-essential pkg-config libsqlite3-dev
libdbus-1-dev`). Report the exact command, the raw test tail, and one full
simulator scenario log.

Then state explicitly, in the report and in `docs/TESTING.md`: **compilation and
simulator success are not hardware verification.** Nothing here proves Ring 5
pairing, Bluetooth operation, or any measured metric.

## Out of scope

Any `oura-cli` or dashboard change, the hub itself (D5), packaging the Pi service
as a systemd unit (D8 documents it), and firmware or factory-reset handling.
