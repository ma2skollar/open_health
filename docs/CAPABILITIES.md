# CAPABILITIES

Evidence-based capability matrix for the pinned pair
(`open_health@8f616ec`, `open_oura@4f7957d`). Every row carries a
classification. Full evidence with `path:line` lives in the audit trail
reproduced in `DECISIONS/0002` and in the notes below each table.

Classification legend:

- **SRC** — source-supported, hardware-unverified.
- **TEST** — verified by an automated test executed in this project.
- **HW** — verified on real hardware.
- **EST** — estimated by the source, no accuracy claim.
- **N/A** — not available.
- **?** — unknown.

Nothing in this project is **HW** yet: no Oura Ring 5 and no Raspberry Pi exist.

## 1. Build and runtime (no hardware)

| Capability | Status | Evidence |
|---|---|---|
| Rust workspace builds with no proprietary model files | SRC | `build.rs:12-18` returns early without `torch`; all five crates have model-free default paths |
| Dashboard runs with no ring and no dataset | **TEST** | `scripts/smoke-dashboard.sh`: `GET /` 200, `GET /app.js` 200, `GET /styles.css` 200, `GET /api/summary` 200 |
| Missing DB produces an error state, not fake data | **TEST** | `{"error":"opening DB"}` (`dashboard.rs:703-705`); observed verbatim in the smoke run |
| Valid schema, zero decoded events produces a clear empty state | SRC | `"no decoded events in {} — run \`oura sync\` first"` (`oura-summary/src/lib.rs:2092-2097`) |
| Linux x86_64 build | **TEST** | `scripts/docker-build-linux.sh`: `cargo build --release --locked -p oura-cli` succeeded, `Cargo.lock` unchanged, artifact 10,845,328 bytes. Rust **≥1.85 required** (Rust 1.83 fails: `clap_lex 1.1.0` needs `edition2024`); verified with `rustc 1.99.1` |
| Linux ARM64 (Pi 4) compile | **TEST (check level)** | `cargo check --locked -p oura-cli` passes for `aarch64` under `--platform linux/arm64` (emulated), including the bundled C SQLite build script and the BlueZ/D-Bus crates. **No ARM64 binary was linked and no Bluetooth was tested** |
| Frontend build step | N/A (none needed) | vanilla HTML/CSS/JS, embedded via `include_str!` |
| UI assets served locally, no CDN | **TEST** | `GET /`, `/app.js`, `/styles.css` all served 200 from local files; no external reference in the served assets |
| Native Linux deps for the dashboard | **TEST** | runtime needs `libdbus-1-3` (process aborts without it); build needs `pkg-config` + `libdbus-1-dev`; SQLite bundled; TLS is rustls |
| Unprivileged LXC run | ? | not tested; no LXC access discovered |

## 2. Ring protocol and hardware path

| Capability | Status | Evidence |
|---|---|---|
| Ring 5 explicitly handled (not just Gen3 fallback) | SRC | `is_ring5 = hw.rsplit('_').next() == Some("05")` present at the pinned revision (`oura-link/src/client.rs:390-397`) |
| Ring 5 pairing / authentication / sync | SRC | documented live success on Ring 5 (`docs/ring-5-observations.md:68-73`); **not reproduced here** |
| BLE stack | SRC | `btleplug` 0.11.8; Linux backend is BlueZ over D-Bus |
| Linux BLE adapter access, D-Bus perms, bonding | ? | unproven; no adapter access in an unprivileged LXC tested |
| History collection with raw event preservation | SRC | `events.body BLOB` stored undecoded; `decoded_json` separate; `oura redecode` re-decodes without refetch |
| Ring auth key at rest | SRC | plaintext 16-byte hex, `0600` on Unix; open_health writes **before** chmod (`main.rs:364-370`) |
| Destructive ring commands in open_health CLI | N/A | no `factory-reset` variant in open_health's `Command`; it exists only in open_oura's CLI behind `--yes` |

## 3. Measurement and metrics

| Metric | Model-free | Status | Notes |
|---|---|---|---|
| HRV RMSSD | yes | SRC | IBI differences (`ported/hrv.rs`) |
| Resting HR / nightly HR | yes | SRC | lowest firmware `hrv_event.hr_bpm` |
| Skin temperature (+ deviation) | yes | SRC | **skin**, not core body temperature (`ported/temperature.rs:1-5`) |
| SpO2 % | yes | SRC | `105.2 − 5.1R − 13.4R²`, clamped 85–100; raw `r` is a ratio-of-ratios, not a % |
| Sleep staging | yes | SRC | Rust heuristic stager from HR/HRV/motion when the Python model is absent; NSSA needs `sleepstaging_2_6_0.pt` |
| Sleep architecture + research sleep score | yes (needs stages) | SRC | duration/efficiency/latency/WASO/awakenings/architecture → 0–100 |
| Sleep need / sleep debt | yes | SRC | history-based, clamped 7–9 h |
| Respiratory rate | yes | SRC | ring `sleep_period_information_2.breath`, median |
| Daily steps | yes | **EST** | MET cadence bands: `MET≥7 → 150`, `MET≥2.5 → 105`, else 0 steps/min; rounded to nearest 100. Not real step counts |
| Walking distance | yes | EST | steps × 0.762 m |
| Active / total calories | yes | SRC | `Σ max(MET−1,0) × kg / 60` plus Schofield BMR |
| VO2max estimate | yes | EST | Jackson non-exercise formula |
| Readiness / Sleep / Activity scores (Oura-calibrated) | **no** | N/A | require proprietary `.pt` files; model-free substitutes are not parity |
| Cardiovascular age, stress, illness, BDI/apnea | **no** | N/A | proprietary `.pt` runners |

Fabrication guardrails already present upstream: the UI gates steps on real-step
event presence while the number itself is MET-derived (`oura-summary/src/lib.rs:2952-2955`
vs `2747-2777`) — this inconsistency must be labelled, not inherited silently.

## 4. Storage and time semantics

| Capability | Status | Notes |
|---|---|---|
| SQLite schema (5 tables) | SRC | `store_meta`, `device`, `sync_state`, `events`, `readings`; 4 indexes including a partial index |
| Duplicate event suppression | SRC | `UNIQUE(serial, tag, ring_timestamp, body)` + `INSERT OR IGNORE` |
| Clock anchors / correction | SRC | `0x42` time-sync and `0x85` RTC beacon events; `anchor_unix + (ds − anchor_ds)/10` |
| Timezone handling | SRC | **numeric fixed UTC offset only**, no IANA rules, no DST transitions |
| Day boundary semantics | SRC | corrected UTC + fixed offset, ÷ 86400; sleep uses **wake-date** semantics |
| Boot/session/epoch tracking | **N/A** | no boot or session table; events keyed by serial/tag/counter/body only |
| Reboot ambiguity detection | SRC | >6 h backward jump starts a new epoch; stalls can yield `AmbiguousRebootStall` |
| Reader-safe generation switching | SRC | store supports a read-only open that changes neither schema nor journal mode |
| Dated manual records | **N/A** | profile is a single-value `profile.json`; no dated record store exists |

## 5. Dashboard and security

| Capability | Status | Notes |
|---|---|---|
| Existing JSON API | SRC | `/api/summary`, `/api/hourly-hr`, `/api/profile`, `/api/bedtime`, `/api/ring-key`, `/api/sync`, `/api/live-hr`, `/api/feature`, `/api/research-export` |
| Browser authentication | **N/A** | none; only a Host check and a same-origin `X-Oura-Dash` header presence check |
| Bind address / TLS | SRC | IPv4 loopback only, plain HTTP, default port 8090 |
| CSRF protection | SRC | presence-only `X-Oura-Dash` header (not a secret comparison) |
| Ring control from the dashboard | SRC | shells out to itself: `oura sync`, `live-hr --seconds 60`, `feature-mode`; **local Bluetooth assumed** |
| Single BLE operation at a time | SRC | shared BLE mutex → concurrent ops return busy (`dashboard.rs:532-545`) |
| Live HR duration cap | **N/A** | `--seconds` is `u64` with no maximum; no signal handler → no guaranteed teardown |
| Internal network exposure | SRC | loopback binding blocks direct LXC-IP browser access; loopback-only is a deployment constraint, not a defence |
| Outbound runtime network use | **TEST — removed** | DNA and blood modules deleted (ADR 0006); `strings -a target/release/oura` contains none of `agent-backend.thomas.md`, `ftp.ebi.ac.uk`, `pgscatalog.org` |
| Telemetry / analytics | N/A | none found |

## 6. Outbound network audit summary

Build/install downloads (allowed, documented): crates.io, the `open_oura` git
dependency, apt packages for the build image.

Runtime outbound (must be removed or disabled): the DNA PGS score download and
the `pgscatalog.org` outbound links. Everything else found is local. This does
**not** yet establish offline operation — that is a deployment test
(`OPERATIONS.md`), not a source claim.

## 7. Hardware gates (all pending)

Pi 4 Bluetooth operation, Ring 5 pairing/authentication, feature configuration,
real history collection, real clock/timestamp correctness, actual metric
availability and quality, step aggregation, live HR and teardown, out-of-range
recovery, missed-day retention/recovery, battery impact, real-world storage
growth. See `HARDWARE_ACCEPTANCE.md`.
