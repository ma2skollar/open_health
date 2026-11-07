# Local modifications

This repository is a fork of [`Th0rgal/open_health`](https://github.com/Th0rgal/open_health),
MIT licensed. Upstream is vendored as a remote (`open_health`) and as a branch
(`upstream-base`) at the revision below; `git log` retains upstream history.

- Fork base: `8f616ec5bdf569e7faecbcefa2b8eff6f69ae2bc` (2026-10-25)
- `open_oura` dependency revision required by that base:
  `4f7957d3304c4084f0e1e703f3d84606bf10b781` (2026-10-07)
  — pinned in `crates/*/Cargo.toml` and `Cargo.lock`; `vendor/oura-store`
  carries upstream's `[patch]` unchanged.

Everything below is a change relative to that revision.

## Removed: DNA and blood panels (ADR 0006)

Not in scope for this project, and they carried the only runtime outbound
integrations. Removed outright — no feature flag, no default-off route.

| Removed | Reason |
|---|---|
| `crates/oura-cli/src/blood.rs`, `crates/oura-cli/src/dna.rs` | contained the third-party AI upload and the PGS FTP download |
| `crates/oura-dna/` (whole crate) and the workspace member | only served the DNA panel |
| `dna/` (catalog + directory placeholders) | only served the DNA panel |
| `dashboard/web/{dna,blood}.{html,js,css}` and the `dna.svg` / `drop.svg` icons | served the removed panels |
| routes: `/dna*`, `/blood*`, `/api/dna/*`, `/api/blood/*` | endpoint surface for the removed panels |
| CLI flags `--dna-files`, `--blood-files` | configuration for the removed panels |
| dependency `ureq` | used only for those two outbound calls |
| `.gitignore` entries for `dna/files/*`, `dna/scores/*` | paths no longer exist |
| `README.md`, `CLAUDE.md` sentences describing blood/DNA analysis | documentation of removed functionality |

Effect: the built binary makes no outbound request other than the future
collector channel. Offline operation is still **not** claimed — that requires a
deployment test (`OPERATIONS.md`).

## Added

| Added | Purpose |
|---|---|
| `docs/PLAN.md`, `docs/ARCHITECTURE.md`, `docs/CAPABILITIES.md`, `docs/SECURITY.md`, `docs/TESTING.md`, `docs/OPERATIONS.md`, `docs/HARDWARE_ACCEPTANCE.md`, `docs/LOCAL-MODIFICATIONS.md`, `docs/DECISIONS/*`, `docs/handoffs/*` | project planning, audit record, decisions, task handoffs |
| `scripts/docker-build-linux.sh` | reproducible locked Linux build in Docker |
| `scripts/smoke-dashboard.sh` | model-free, no-ring dashboard smoke test |
| `crates/oura-manual/` | new crate: application-owned manual calorie/weight/step store (`manual.db`), ADR 0003 |
| `crates/oura-import/` | new crate: import framework (dry run, explicit mapping, batch provenance, rollback, step precedence) |
| `crates/oura-backup/` | new crate + `oura-backup` binary: consistent SQLite backups with a manifest, verification and a safe restore |
| `crates/oura-cli/src/secret_file.rs` | owner-only file creation for the ring key (security fix S4) |
| `NOTICE` | fork attribution |
| `.gitignore` additions: `upstream/`, `/out/`, `/dist/` | local reference clones and build output |

## Security fixes applied to upstream code

| Fix | Change |
|---|---|
| CSRF guard bypass (S2) | `handle()` now splits the request once and evaluates the same-origin guard on the header block only, so a body line can no longer satisfy `X-Oura-Dash`; when `Origin` is sent it must equal `Host`. `docs/SECURITY.md` S2 |
| Ring key file permissions (S4) | `secret_file::write_private` creates the file with mode `0600` in the creating syscall and errors if the mode cannot be enforced, instead of writing first and ignoring a failed `chmod` |

## Upstream files edited

- `Cargo.toml` — one workspace member removed
- `crates/oura-cli/Cargo.toml` — two dependencies removed
- `crates/oura-cli/src/main.rs` — module declarations, two CLI flags, wiring
- `crates/oura-cli/src/dashboard.rs` — asset constants, route arms, CSRF list
- `dashboard/web/index.html` — two navigation links
- `docs/clients-web-and-ios.md` — fork note marking the two removed panels
- `.gitignore`, `README.md`, `CLAUDE.md`

## Not changed

Nothing under `apps/ios/`, `crates/oura-summary/`, `crates/oura-core/`,
`crates/oura-ffi/`, `vendor/oura-store/`, `tools/`, or `ci_scripts/`.
Upstream `LICENSE` is unchanged; see `NOTICE`.
