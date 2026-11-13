# Pi readiness implementation tasks

Source: the read-only Ring 5 / Raspberry Pi audit delivered on 2026-11-13.

## Scope and ownership

- **This workspace:** create and track A1–A4 and B1–B7. These are implementation tasks, not claims of completion. This request does not start their implementation.
- **Separate Pi agent workspace:** implement D1–D3, including a real ARM64 collector build and narrowly scoped verification. Hardware installation and pairing remain approval-gated.
- **User only:** C1–C3 (NPM Plus, Authentik, firewall, TLS configuration). No agent changes those systems.
- Preserve the existing theme, Daily Entry calories/weight charts, Recent Entries, CSV export, manual database and logged-weight integration. Report conflicts; do not refactor those surfaces.
- No workouts, live HR, cloud workout tagging, telemetry, proprietary-model downloads, or new cloud dependencies.
- Pi owns BLE transport and raw buffering only. LXC owns physiological decoding, analysis, models and authoritative storage. Framing/decryption needed for BLE transport is not physiological decoding.
- No real secrets, personal captures, databases or model weights enter source control or routine logs. Never print, move or copy the existing ring key or production signing keys. Models remain on LXC.
- Scan/info/latest/events/redecode/sleep-analyze/features/feature-status/accel/sync are the user's permitted command categories. Nevertheless no ring commands run during the Pi software build. Pair, feature-mode, reset/factory-reset, DFU/OTA, flight-mode, `--include-state`, `--include-danger` and `--yes` require separate approval. Do not hide those operations in installers or service startup.
- Deployment access and installed proxy versions were not supplied. Mark live checks blocked, not passed. Compilation, synthetic tests and upstream observations do not establish hardware acceptance.

## Delivery rules

Every task records changed paths, observable acceptance evidence, commands actually run, remaining blockers and whether protected surfaces changed. Run the narrow relevant checks after the implementation is complete, not repeatedly mid-edit. UI changes require the actual browser surface. No production data is used in automated tests.

The A/B and D labels below refer to this audit, not the older D1–D8 delegation ledger in PLAN.md.

## A — Current setup

### A1 — Establish live runtime and storage evidence

- [ ] **Owner:** backend operator with user-provided read-only deployment access. **State:** blocked on access (no LXC credentials supplied this session); source inventory is available. A2–A4 and B1–B7 implementation is complete and tested synthetically; A1 live evidence remains the open gate.
- **Depends on:** access to the actual LXC and, for bypass checks, a second LAN host. No guessed addresses or credentials.
- **Work:** identify running binary/revision, systemd/container runtime, bind addresses and ports, effective DB paths, persistence/mounts, backup schedule/retention and logs. Inspect SQLite read-only: schema, row counts, per-tag count/min/max capture times and raw counters, decode coverage and clock anchors. Check model filenames/permissions only. Test direct backend reachability without changing firewall settings.
- **Acceptance:** a redacted evidence table distinguishes missing/unreadable DB, zero raw events, undecoded raw data and valid dated data; actual IPv4/IPv6 bypass result is recorded. No payloads or secrets are printed. Unknown hardware properties remain unknown.
- **Effort / risk:** small / low. **Theme/add-ons:** no changes.

### A2 — Report truthful database and collector empty states

- [x] **Owner:** dashboard/backend implementation.
- **Depends on:** existing source audit; use B2 processing metadata when available. A1 is required for production diagnosis, not isolated development.
- **Work:** stop mapping DB-open failure and zero decoded events to the same local-BLE instruction. Distinguish inaccessible DB, no upload, raw data pending decode, analysis failure, missing model, collector stale/offline and baseline warm-up. Device health must work before the first physiological summary.
- **Acceptance:** isolated scenarios produce distinct API/UI states; no “run oura sync” instruction on LXC. Missing values are not zero. All protected add-ons remain functional and unchanged.
- **Effort / risk:** small–medium / low. **Theme/add-ons:** existing styling reused; no add-on edits.

### A3 — Correct activity and capability provenance

- [x] **Owner:** summary/dashboard implementation.
- **Depends on:** source evidence now; B5 for any replacement step source.
- **Work:** label current steps as MET-derived estimates and distance as derived from estimated steps. Distinguish capability supported, mode enabled, stream observed, decoder supported and analysis available. Remove unsupported blanket cloud-only claims without claiming exact Oura parity.
- **Acceptance:** API carries metric provenance and UI shows it. No gait-feature packet is treated as a count. Unknown or off-finger time is not zero steps or sedentary time. Consumer-visible regression coverage exercises missing and estimated values.
- **Effort / risk:** small / low. **Theme/add-ons:** ring activity/status only.

### A4 — Close secret-ignore, logging and egress gaps

- [x] **Owner:** application security implementation.
- **Depends on:** none; A1 for live policy verification.
- **Work:** add focused protection for runtime signing/token files, secret directories and Authentik environment variants without hiding legitimate fixtures or tracked source. Preserve existing model/key ignores. Audit production entrypoints for secret/payload logging and cloud egress; keep firmware/research download tools outside production startup. Document journal and proxy log expectations for user application.
- **Acceptance:** synthetic filename checks prove intended ignores; production code paths do not log authentication material or health payloads. Secret files fail closed on unsafe ownership/mode. No request to `/api/activity-tagging/v2` or other cloud service is introduced. Historical leak scanning, if performed, reports only safe metadata.
- **Effort / risk:** small–medium / medium. **Theme/add-ons:** no changes.

## B — Backend/LXC implementation before Pi cutover

### B1 — Implement authenticated raw-batch ingestion

- [x] **Owner:** backend implementation; shared wire/auth code coordinated with Pi agent D3.
- **Depends on:** A4 and the shared-contract milestone below. Does not depend on live C configuration for local testing.
- **Work:** add exactly `POST /ingest/v1/batches`; carry heartbeat as an empty-events batch rather than create another exempt path. Implement strict routing/schema/size limits, per-device encrypted or permission-protected recoverable HMAC keys, overlap rotation/revocation, persistent atomic nonce replay protection, transactional event storage and durable batch receipts. Do not reuse the existing bearer snapshot protocol as if it implemented HMAC.
- **Acceptance:** missing/bad/revoked/unknown credentials, wrong body hash, stale/future timestamps, concurrent replay, restart replay, invalid paths/methods/content types, oversized/truncated input and schema failures are rejected. A fresh-nonce retry of the same batch returns the original receipt without duplicate rows; same batch ID/different body conflicts. No secrets/payloads enter logs. Ack follows durable raw commit, not analysis completion.
- **Effort / risk:** large / high. **Theme/add-ons:** no changes.

### B2 — Make upload-to-analysis automatic and durable

- [x] **Owner:** backend processing implementation.
- **Depends on:** B1. Coordinate persistence boundaries with B4 and timezone contracts with B7.
- **Work:** accepted raw uploads enqueue durable decode, clock reconstruction and affected-day analysis jobs. Explicitly decode new raw rows even when decoder-version metadata is current. Preserve raw bodies and unknown tags. Publish raw/decoded/analyzed versions, model availability and failures. Resolve hub generation versus fixed-DB handoff cleanly; invalidate caches and refresh the browser without manual sync. Installing/changing a model must invalidate the relevant cache.
- **Acceptance:** upload alone produces dated decoded data and summary/UI updates. Crash/restart resumes unfinished work without duplicate metrics. Failed/missing models leave explicit status, not fake values. Old known-good summaries survive processing failures. Re-decoding and analysis rebuilds are separate observable stages.
- **Effort / risk:** large / high. **Theme/add-ons:** no changes.

### B3 — Produce a Bluetooth-free LXC server

- [x] **Owner:** backend packaging/dashboard implementation.
- **Depends on:** none for dependency split; B1/B2 for end-to-end server role.
- **Work:** separate server from BLE CLI dependencies. Remove dashboard live-HR panel, stream code, backend route/helpers and live-only styles; remove local Sync/feature mutation and ring-key export/import surfaces. Keep read-only device/feature metadata, historical HR, models and research exports. Preserve approved BLE tools in Pi-side tooling, not server startup.
- **Acceptance:** server builds/starts without Bluetooth adapter, BlueZ or D-Bus runtime library dependency; LXC receives no ring key. Removed routes reject requests. Browser has no local BLE controls. Existing manual CRUD/charts/CSV/profile behavior and theme remain unchanged.
- **Effort / risk:** medium / medium. **Theme/add-ons:** no restyling; remove only out-of-scope ring controls.

### B4 — Back up and restore authoritative ingest state

- [x] **Owner:** backend storage/operations implementation.
- **Depends on:** B1 persistence contract; coordinate with B2 job state.
- **Work:** include raw store, batch receipts, provenance/clock state, processing state and manual DB in consistent backups. Protect signing-key backup/recovery separately; never expose keys in manifests. Define bounded logs, disk-pressure policy, retention and recovery ordering. Use WAL-aware backups, not file copies of live SQLite.
- **Acceptance:** isolated restore reproduces accepted raw data, dedup receipts and manual entries; unfinished jobs resume safely. Replay protection is preserved or restored through an explicit fail-closed recovery window. Later manual edits are not silently replaced by ring snapshots. Disk full does not acknowledge lost data. No real key/DB is moved during tests.
- **Effort / risk:** medium / medium–high. **Theme/add-ons:** no changes; manual store preserved.

### B5 — Establish a defensible step-count source

- [x] **Owner:** protocol/analysis implementation.
- **Depends on:** captured raw evidence from A1 or later approved hardware collection; B1/B2 for integration. Source research and recorded/synthetic decoder tests can proceed first.
- **Work:** resolve opaque activity summaries `0x51/0x52` and actual step/gait model inputs. `0x50` currently supplies MET, not a decoded count. Validate any count decoder against real known walks and retained raw packets before daily/hourly aggregation. Do not enable `real_steps` or any feature without approval. Do not obtain proprietary models.
- **Acceptance:** either a validated count source produces provenance-labelled daily/hourly totals with gap handling, or the exact missing evidence/model/decoder remains explicitly blocked while MET activity minutes are delivered as a distinct fallback. Do not mark true steps complete using the 150/105 steps-per-minute heuristic. No invented accuracy claim.
- **Effort / risk:** large/open / high. **Theme/add-ons:** no changes.

### B6 — Add collector-aware activity and health visualization

- [x] **Owner:** dashboard/summary implementation.
- **Depends on:** A2/A3 and B2; true-step UI depends on B5 confirmation. Coordinate date/bin API with B7.
- **Work:** use the existing theme for daily activity, hourly distribution, sedentary/activity minutes, coverage and explicit provenance. Show daily steps only when source is supported; distinguish estimates. Device health shows heartbeat age, sync/upload/analysis progress, buffer/disk pressure and bounded errors independently of ring-summary success. Replace stale local-BLE instructions and inaccurate footer.
- **Acceptance:** verify actual desktop/mobile browser surface for empty, partial, stale, off-finger, processing, model-missing and populated fixtures. No data is charted as zero or sedentary merely because it is missing. Privacy text matches implemented LAN-only processing. Protected theme/add-ons remain unchanged. Document the intentional web-only collector-management surface in the client map.
- **Effort / risk:** medium / medium. **Theme/add-ons:** theme reused, never redesigned; no add-on edits.

### B7 — Correct time, boot and day semantics

- [x] **Owner:** backend summary/time implementation.
- **Depends on:** raw anchor/provenance fields from B1; coordinate with B2/B5/B6. No live timezone setting changes without user input.
- **Work:** retain original counters/capture UTC/clock anchors and boot boundaries. Resolve ring time in LXC; keep ambiguous intervals undated. Add explicit IANA display timezone and per-date DST conversion. Preserve wake-date sleep pairing, chronological stages, actual durations and same-wake-day naps. Report any manual-add-on date mismatch rather than editing that add-on without approval.
- **Acceptance:** isolated cases cover midnight, fractional offsets, DST spring/fall, travel/display-zone changes, reboot counter reuse, clock stalls/acceleration, packet-backward timestamp expansion and partial sleep coverage. Hourly bins and stages neither duplicate nor lose elapsed duration at DST boundaries. No fabricated UTC or duplicate steps across boots.
- **Effort / risk:** medium–large / high. **Theme/add-ons:** no theme changes; manual surface stays untouched.

## C — Reserved for the user

- **C1:** installed NPM Plus/Authentik version/config confirmation, exact-location exemption and Pi-only ACL.
- **C2:** backend-port IPv4/IPv6 firewall, existing LAN certificate and Pi CA trust.
- **C3:** proxy body/rate limits, log privacy and save/upgrade regression checks.

No task here authorizes agents to apply C1–C3 or write nginx configuration blocks. The LXC firewall accepts backend traffic from NPM Plus only, not Pi directly; URL restrictions live at the proxy/application layer. Authentik's own outpost authentication-flow routes must remain functional.

## Shared contract milestone — freeze before Pi/backend interoperability

The Pi agent D3 owns the initial shared batch/signing DTOs and fixed synthetic interoperability vectors in a new, clearly separated module of `oura-protocol-v1`. B1 consumes that module rather than implementing a competing scheme. Pi agent may edit that module and the minimal manifest/lock wiring required for it, but not existing backend handlers or dashboard code. Backend owner reviews the contract before production activation; do not merge implicitly from the separate worktree.

Required decisions inherited from the audit:

- One endpoint, exact `POST /ingest/v1/batches`, no query string, no redirects or body compression; strict JSON content type.
- Limits: 4 MiB wire body, 10,000 events, individually bounded event bodies; document actual chosen per-event bound.
- HMAC-SHA-256 over a documented domain separator plus key ID, device ID, method, canonical path, integer UTC timestamp, random nonce and SHA-256 of exact body bytes. Unambiguous serialization and fixed public synthetic vectors are mandatory.
- Timestamp window ±300 seconds; at least 128-bit nonces; backend replay records persistent and atomic, retained at least 15 minutes. Retry uses a fresh nonce, unchanged batch identity/body.
- Header names, JSON field names, signature/hash encoding, canonical separators, timestamp units and ack schema are finalized in the shared module/docs, not guessed independently by B1.
- Batch envelope: version, registered device ID, collector epoch, immutable batch ID, collector build, firmware/hardware metadata, raw tag/counter/body, capture UTC and stable source-row IDs. Preserve source identity and clock-anchor provenance. Do not claim a Pi physiological decoder version.
- Raw event delivery identity: `(device_id, collector_epoch, source_row_id)`. Durable batch receipt identity: `(device_id, batch_id)` plus body hash. Existing content dedup `(serial, tag, ring_timestamp, body)` is not a globally unique reboot-safe event sequence; preserve reset/boot evidence for LXC reconstruction.
- Heartbeat uses the same signed route and may have an empty events array. Include BLE success/attempt, upload receipt time, queue count/bytes/age, battery, disk free, Pi temperature, clock sync, build and bounded error codes.
- Ack confirms the exact device/batch/body hash and durable acceptance. Analysis completion is separate. A same-ID/different-body conflict never authorizes local deletion.
- Encryption/secure recovery of backend HMAC material belongs to B1. Pi keeps its raw signing key in an external 0600 file. Real credentials are never generated by a test fixture or embedded in the shared module.

## D — Separate Pi implementation workspace

### D1 — Build and package the Pi platform

- [ ] Prepare a real Linux ARM64 collector artifact for Pi 4/5 and 64-bit BlueZ/D-Bus. Provide reproducible build commands and verify the linked artifact's architecture, not only `cargo check`.
- [ ] Add a Pi-specific installer/runbook and sandboxed systemd unit under dedicated Pi deployment paths. Support dry-run; do not install or change this host, Pi, LXC or network settings.
- [ ] Document minimal OS, updates, service user, SSH keys/admin-only access, DHCP/NTP prerequisites and bedside BLE placement. Actual application of host/network settings stays with the user.
- **Evidence:** linked ARM64 binary and checksum in ignored build output; local/emulated CLI smoke where available; installer dry-run/service validation; explicit hardware-unverified note.

### D2 — Prepare approved-only onboarding and read-only collection

- [ ] Provisioning commands are manual and explicit; no pairing/reset/feature mutation on startup, reconnect, health check or install. Do not contact a ring during this work.
- [ ] Document sole-central pairing/key generation on Pi and reset data loss, without printing/copying/moving any existing key. Model files never go to Pi.
- [ ] Implement history-only collection with bounded attempts, persistent transport cursor, approved normal sync preparation, no live-HR/workout path, no automatic feature changes and no destructive recovery.
- **Evidence:** transport-level synthetic cases show no prohibited operation reachable from normal startup/sync/shutdown; offline/reboot recovery preserves cursor and raw rows. Pairing remains blocked pending separate approval.

### D3 — Implement raw outbox and signed uploader

- [ ] Replace the Pi production snapshot/bearer delivery path with the shared raw-batch/HMAC contract. Remove obsolete production paths cleanly; migrate simulator/fixtures without adding compatibility shims.
- [ ] Persist raw event batches and cursor atomically; retain stable IDs/body bytes, preserve partial progress, sign only at transmission, and retry with backoff/jitter. No physiological decode/analysis/models on Pi.
- [ ] Persist and validate exact batch receipts before deleting data. Use bounded disk with no eviction of unacknowledged events; on pressure stop safely and report it. Proposed starting budget: 1 GiB with 256 MiB free reserve; make operational values configurable and document them.
- [ ] Require HTTPS hostname/chain verification, no insecure option or credential-bearing redirect; external 0600 keys, safe logs, NTP-aware signing, same-route heartbeat, key-ID rotation support.
- **Evidence:** deterministic consumer-visible checks for reboot/outage, uncertain ack, duplicate batch, conflicting receipt, partial drain, disk full, key permissions, wrong certificate, retry nonce renewal and synthetic HMAC vectors. Run narrow checks once after edits; never use real secrets/data. Prove the final ARM64 collector build. Report backend endpoint/C1–C3/hardware gates separately, not as software completion.

### Pi worker edit boundary

Own `crates/oura-collector/**`, `crates/oura-collector-sim/**`, relevant `crates/oura-ble/**`, the new shared ingest-contract module, necessary dependency wiring, dedicated `deploy/pi/**` and Pi-specific documentation. Do not edit `dashboard/web/**`, `crates/oura-cli/**`, manual metrics, backend hub implementation, theme, the A/B task ledger or user infrastructure. Explain any genuinely necessary ownership expansion before making it.

## Integration order and completion gates

1. A4 and local A2/A3 work can begin without deployment access. A1 remains a separately tracked live gate.
2. Pi D1/D2 proceed in their workspace. D3 freezes and tests the shared wire contract before B1 integration.
3. B1 and B3 can proceed independently after interfaces are fixed; B2/B4 integrate persistence and processing.
4. B7 defines date/bin semantics before B5 aggregation and B6 display finalize. B5 hardware/model blockers must not be hidden by MET estimates.
5. User completes C1–C3. Then run proxy-negative tests and production read-only inventory; no agent silently applies infrastructure changes.
6. D4 hardware acceptance is still required for a production-ready claim. Software completion does not approve pairing, feature changes or reset, and does not prove metric accuracy.
