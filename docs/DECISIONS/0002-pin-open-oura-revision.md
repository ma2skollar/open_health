# ADR 0002 — Pin open_oura to the revision open_health requires

Status: accepted (2026-11-07)

## Context

`open_health` depends on `open_oura` crates by git revision, not by path:

```
oura-protocol = { git = "https://github.com/Th0rgal/open_oura", rev = "4f7957d3304c4084f0e1e703f3d84606bf10b781" }
```

`crates/oura-cli/Cargo.toml:14-16`, `crates/oura-core/Cargo.toml:15-20`,
`crates/oura-ffi/Cargo.toml:14` all repeat that revision, and `Cargo.lock`
resolves it identically. `open_health/Cargo.toml:20-21` additionally patches
`oura-store` to the in-tree `vendor/oura-store`.

Measured relationship between revisions:

- pinned `4f7957d` (2026-10-06) is an **ancestor** of main `e57c43c`
  (2026-10-07);
- the distance is exactly **1 commit**
  (`fix(sleep): sync Gen 4 early-cycle N3 staging and local circadian HR baseline`).

`vendor/oura-store/README.md` claims provenance from revision
`b1b52a0da36a592d5fe8967b0024dcc40d5c746f` (2026-08-31). That claim is
**stale**: SHA-256 of every file in `vendor/oura-store/src/` matches the
`4f7957d` copy byte-for-byte, and differs from `b1b52a0d`. The vendor patch is
therefore: pinned revision source + a standalone `Cargo.toml`
(no `*.workspace` keys, absolute `oura-protocol` git dep) + a README.

## Decision

Build against the pinned pair (`open_health@8f616ec`, `open_oura@4f7957d`) and
vendor `oura-store` exactly as upstream does. Do not track either project's
`main`.

Adopting `e57c43c` is a separate, deliberate change: it requires re-locking
three git dependencies and re-running the storage/decoder tests, and it changes
sleep staging for Gen 4. Not adopted now.

## Consequences

- Reproducible builds offline after one fetch; no surprise upstream drift.
- Two sources of truth for "which open_oura is live" (`Cargo.toml` pins,
  `Cargo.lock`) must be updated together.
- A discrepancy in the vendor README is documented here rather than silently
  trusted.
