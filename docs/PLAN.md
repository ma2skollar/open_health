# PLAN

Local-first Oura health dashboard. Ledger of phases, gates, and ownership.
Status vocabulary for every capability: **Implemented / Automated-tested /
Hardware-verified / Deployed / User-accepted**.

Revision pinning (recorded):

| Source | Revision | Date | Note |
|---|---|---|---|
| `Th0rgal/open_health` | `8f616ec5bdf569e7faecbcefa2b8eff6f69ae2bc` | 2026-10-25 | fork base |
| `Th0rgal/open_oura` | `4f7957d3304c4084f0e1e703f3d84606bf10b781` | 2026-10-06 | revision required by open_health `Cargo.lock` / `Cargo.toml` |
| `Th0rgal/open_oura` (main) | `e57c43ce5fe0cf9a646afa790bda3731be767507` | 2026-10-07 | pinned rev + **1** commit; not used until separately validated |

Audit clones live in `upstream/` and are **gitignored**; they are reference
material, not part of the fork.

## Phase 1 — Audit and plan

Deliverables: this plan, `ARCHITECTURE.md`, `CAPABILITIES.md`,
`DECISIONS/`, `TESTING.md` skeleton.

Gate: pinned revisions recorded; capability matrix classified; interfaces
frozen before parallel implementation starts.

## Phase 2 — Useful LXC application (no ring required)

Model-free dashboard, explicit no-ring state, manual calories + weight CRUD,
charts, export, backup/restore.

Gate: dashboard serves on Linux with an empty database; manual CRUD proven by
automated tests and a manual smoke run.

## Phase 3 — Historical migration

Import framework (dry run, preview, rejection report, provenance, rollback),
then one source adapter once an anonymized sample is supplied.

Gate: rerun-safe import; rollback that preserves later unrelated edits.

## Phase 4 — Ingestion and simulated collector

Snapshot validate/activate, generation and continuity protection, simulator,
outage/retry/failure tests.

Gate: duplicates harmless, stale generations cannot roll data back, restart
during upload/activation is safe.

## Phase 5 — Remote controls

Versioned allowlisted control protocol, simulator-backed execution, lifecycle,
expiry, serialization, live-HR stream handling.

Gate: expired commands never execute after reconnect; live session teardown
verified on stop/timeout/disconnect/shutdown.

## Phase 6 — Future collector preparation

BLE abstraction, compatible ARM64 build, systemd/install package, hardware
runbook.

Gate: ARM64 artifact builds; compile success is explicitly **not** hardware
verification.

## Phase 7 — Hardware validation

**Blocked**: no Oura Ring 5, no Raspberry Pi 4. Checklist in
`HARDWARE_ACCEPTANCE.md`; run when hardware exists.

## Phase 8 — Production acceptance

**Blocked** on Phase 7. Parallel run against the existing dashboard, security
and restore review, explicit user approval before cutover or retirement.

## Ownership (parallel work separation)

| Area | Owner module(s) |
|---|---|
| Upstream compatibility, collector, BLE | `crates/oura-collector`, `crates/oura-ble` |
| Protocol, ingestion, storage | `crates/oura-protocol-v1`, `crates/oura-hub` |
| Dashboard, manual metrics | `crates/oura-cli` (dashboard), `crates/oura-manual`, `dashboard/web/` |
| Import, testing, deploy, ops | `crates/oura-import`, `deploy/`, `docs/` |

Integration and verification are owned by the lead agent. No two agents edit the
same file concurrently.

## Delegation handoffs

Interfaces are frozen in `ARCHITECTURE.md`. Each handoff below carries objective,
pinned files, interface constraints, acceptance, tests, out-of-scope, and
deliverable. The lead owns integration and re-runs the verification itself.

| # | Objective | Editable | Depends on | Acceptance | Status |
|---|---|---|---|---|---|
| D1 | `crates/oura-manual` — manual calorie/weight store, units, dated CRUD, export | `crates/oura-manual/**`, workspace `members` in root `Cargo.toml` | ADR 0003 interfaces | `cargo test -p oura-manual` green in Docker; absent != zero; duplicate-day upsert semantics; schema-version refusal | **done** — 20 tests pass |
| D2 | Remove DNA/blood panels and all outbound integrations (ADR 0006) | `crates/oura-cli/src/{dashboard,main}.rs`, `crates/oura-cli/Cargo.toml`, `dashboard/web/*`, `dna/**`, root `Cargo.toml` | none | no `ureq` in the built binary; no `/api/dna/*`, `/api/blood/*`, `/dna*`, `/blood*` routes; build + smoke pass | **done** — satisfied by the lead; smoke asserts every removed route 404s and the binary carries no third-party endpoint |
| D3 | Manual metrics HTTP API + mobile-friendly UI + charts + export (wires D1 into the dashboard) | `crates/oura-cli/src/dashboard*.rs`, `dashboard/web/**` | D1 interfaces, D2 (owns `dashboard.rs`) | routes exercised by the smoke script; charts render with empty and populated data | **done, integrated** — 10 `manual_api_tests`, smoke PASS on the integrated tree; UI rendering verified separately with headless Chromium |
| D4 | Backup/restore for `manual.db` + the ring database | `crates/oura-backup/**`, `deploy/**`, `docs/OPERATIONS.md` | D1 | restore test proves a round trip; documented ordering | **done** — delivered as `crates/oura-backup` (library + `oura-backup` binary) rather than an `oura-cli` subcommand, so it never contended for `main.rs` with D3. 10 tests plus an end-to-end CLI run |
| D5 | `crates/oura-protocol-v1` + `crates/oura-hub` ingestion (validate, generations, continuity) | `crates/oura-protocol-v1/**`, `crates/oura-hub/**` | ADR 0004 interfaces | upload auth/limits/checksum/schema/stale-generation/restart tests | not started |
| D6 | `crates/oura-collector` (BLE behind a trait) + `crates/oura-collector-sim` | `crates/oura-ble/**`, `crates/oura-collector/**`, `crates/oura-collector-sim/**` | D5 protocol | simulator exercises sync/battery/live-HR/offline/busy/cancel/expiry; deterministic scenarios | not started |
| D7 | Import framework (dry run, mapping, provenance, rollback) | `crates/oura-import/**` | D1 | rerun-safe; rollback preserves later unrelated edits | **done** (framework + a CSV adapter); the real source adapter is blocked on an anonymized sample |
| D8 | Deploy: systemd units, install script, example config, LXC and ARM64 runbooks | `deploy/**`, `docs/OPERATIONS.md` | D2, D3 | idempotent install; privileged steps listed; units validated with `systemd-analyze verify` | **done** — `verify` exit 0 for all three units, `shellcheck` clean, `--dry-run` changes nothing |

D2 and D3 share `dashboard.rs` and are therefore sequenced, not parallel.

**Integration note (D3).** D3 branched before `MetricKind::Steps` existed, so its
exhaustive `match` over `MetricKind` did not compile once merged. Fixed on
integration: `/api/manual/series` now serves imported steps unconverted, in their
own array, and `imported_steps_are_served_unconverted_and_stay_out_of_the_other_series`
covers it. The lesson for the next handoff: give every new enum variant to the
worker that will match on it, or state in the handoff that the enum is growing.

## Open inputs (blocking specific tasks only)

1. LXC distribution / resources / access method.
2. Hostname/IP, persistent storage path.
3. Reverse proxy, TLS, authentication choice.
4. IANA timezone.
5. Weight display units.
6. Anonymized export sample (blocks Phase 3 adapter only).
