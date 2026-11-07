# HARDWARE ACCEPTANCE

**Status: nothing here has been executed.** No Oura Ring 5 and no Raspberry Pi 4
exist yet. Compilation and simulator success are explicitly **not** hardware
verification.

Fill one row per item with: date, operator, command, raw observation, verdict.
Diagnostics must be pasted, not paraphrased.

## D6 software evidence is not hardware acceptance

The deterministic simulator uses a scripted transport, fake time, synthetic
SQLite data and the real collector/ingestion code. It can establish software
state transitions, bounds and persistence only. Compilation and simulator
success are **not hardware verification**: nothing proves Ring 5 pairing,
Bluetooth operation, or any measured metric.

Still unverified: aarch64 runtime behavior on a Pi 4, adapter discovery, Linux
bonding and authentication, the real history cursor and completeness, actual
feature-mode support, live-HR sample validity and teardown on the ring, RF
disconnect recovery, real wake-window timing/DST, battery impact, storage
growth, snapshot/upload latency, and every metric's validity. All checklist
rows below remain pending until dated raw hardware observations are recorded.

## Preconditions

- [ ] Ring 5 charged; Pi 4 with a working built-in Bluetooth adapter
- [ ] LXC reachable; `oura-hub` running; collector credential issued
- [ ] Pi: `libdbus-1-3`, BlueZ, a D-Bus session for the collector service

## A. Bluetooth and pairing

| # | Check | Command | Expected observation | Verdict |
|---|---|---|---|---|
| A1 | Adapter visible | `bluetoothctl show` | adapter present, powered | pending |
| A2 | Ring discovered | `oura scan` | Oura service advertised; hardware_id ends `_05` | pending |
| A3 | Host pairing/bonding on Linux | `bluetoothctl` pair/trust | bond established; **unproven for Linux** — captures upstream are macOS | pending |
| A4 | Pair (requires factory-reset ring) | `oura pair --key-file ⟨path⟩` | tag `0x24` install accepted (`0x25` status 0) | pending |
| A5 | Auth on every connect | `oura info` | auth verdict success | pending |
| A6 | Key at rest | `ls -l ⟨key⟩` | `0600`, 32 hex chars, no permissions window on creation | pending |

## B. Collection

| # | Check | Command | Expected | Verdict |
|---|---|---|---|---|
| B1 | First sync | `oura sync` | events inserted; cursor advances; count reported | pending |
| B2 | Reconnect / duplicate | run `oura sync` twice | second run is a no-op; `UNIQUE(serial,tag,ring_timestamp,body)` absorbs duplicates | pending |
| B3 | Ring absent | run sync with the ring away | bounded reconnect attempts, backoff, non-zero exit, no spin | pending |
| B4 | Reboot recovery | reboot the Pi mid-window | cursor persists; no re-collection storm | pending |
| B5 | Missed days | leave the ring off 3+ days | history is retained and recovered on next sync; **do not** trust advertised retention limits | pending |
| B6 | Out of range | walk away during collection | clean timeout, no partial/corrupt row | pending |
| B7 | Clock correctness | compare `ring_timestamp` projection vs wall clock | correct relative ordering; measure actual error (no error bound is documented upstream) | pending |
| B8 | Overnight/wake-date | sleep across midnight, and across a DST change | sleep attributed to the wake date; DST handled by the configured IANA timezone, **not** a fixed offset | pending |
| B9 | Storage growth | `ls -l oura.db` before/after N days | measured bytes/day, WAL state after checkpoint | pending |
| B10 | Battery impact | Oura battery % across a week | measured drain vs baseline | pending |

## C. Metrics

| # | Check | Expected | Verdict |
|---|---|---|---|
| C1 | Steps | value present; confirm it is labelled **estimated**, derived from MET bands, not a real step count | pending |
| C2 | SpO2 | shown as a percentage derived from R; never a raw ratio shown as % | pending |
| C3 | Skin temperature | labelled skin, never core body temperature; deviation vs baseline plausible | pending |
| C4 | Sleep staging | heuristic stages present, labelled as a signal estimate, not NSSA parity | pending |
| C5 | HRV / resting HR | plausible vs a reference morning reading | pending |
| C6 | Unavailable metrics | no fabricated readiness/sleep/activity score, stress, or illness output without a model | pending |

## D. Controls

| # | Check | Expected | Verdict |
|---|---|---|---|
| D1 | `sync_now` | queued → receipt → completed with counts | pending |
| D2 | `refresh_battery` / `refresh_device_info` | fresh values returned | pending |
| D3 | `collector_status` | reports Pi, adapter, ring-seen, ring-connected as **distinct** states | pending |
| D4 | Busy | two commands at once → second is rejected as busy, not queued twice | pending |
| D5 | Expiry | command expired while the Pi was offline never executes on reconnect | pending |
| D6 | `start_live_hr` | bounded duration enforced server-side; samples stream; stops at the cap | pending |
| D7 | `stop_live_hr` | teardown on stop, timeout, disconnect, and service shutdown | pending |
| D8 | No destructive control | factory reset / DFU / flight mode are absent from the allowlist | pending |

## E. Delivery

| # | Check | Expected | Verdict |
|---|---|---|---|
| E1 | Snapshot creation | consistent under a concurrent writer (WAL); no naive file copy | pending |
| E2 | Snapshot size/time | measured bytes and seconds for N days of data | pending |
| E3 | Upload | checksum verified; ack only after durable acceptance | pending |
| E4 | Duplicate upload | re-upload of an accepted generation is a harmless no-op | pending |
| E5 | Stale generation | rejected; server data never rolls backward | pending |
| E6 | Corruption | truncated/tampered file rejected before activation | pending |
| E7 | Activation | readers never see a swapped database; rollback to the prior generation works | pending |
| E8 | Offline buffering | server down for a day → data delivered after recovery, nothing discarded | pending |
