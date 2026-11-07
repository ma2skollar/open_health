# Handoff D4 — consistent backup and verified restore

## Objective

Give the LXC a reproducible, consistent backup of `manual.db` and the ring
database, and a restore that verifies before it replaces anything. A raw file
copy of a live WAL database is not acceptable.

## Context (decided — do not relitigate)

- Fork of `Th0rgal/open_health`. `crates/oura-manual` owns `manual.db`
  (`docs/DECISIONS/0003`); manual history must survive every ring-side operation.
- `docs/OPERATIONS.md` already states the recovery ordering and that a Proxmox
  VE snapshot alone is not a backup. Implement that, do not restate it.
- Generation-addressed ring storage (`ring/<epoch>/<gen>.db`) does not exist yet
  (that is D5). Backup/restore must work today against the single ring database
  path passed with `--db`, and must not assume a layout that does not exist.

## Files you own

New `crates/oura-cli/src/backup.rs`; `crates/oura-cli/src/main.rs`;
`docs/OPERATIONS.md`; `docs/TESTING.md`; `scripts/backup-restore-smoke.sh`.

Do **not** edit any `Cargo.toml` — the integrator has already enabled the
`rusqlite` `backup` feature and added the `oura-manual` dependency.

## CLI

```
oura backup  --dest <DIR> [--db oura.db] [--manual-db manual.db] [--label <TEXT>]
oura restore --from <DIR> --dest <DIR> [--force]
oura backups --dest <DIR>
```

- `backup` creates `<DIR>/<UTC timestamp>-<label>/` containing
  `manual.db`, `oura.db` (only when `--db` exists) and `manifest.json`.
- Snapshot each database with the SQLite online backup API
  (`rusqlite::backup::Backup`), never `std::fs::copy` on a live file. Checkpoint
  or back up through the API so WAL contents are included.
- `manifest.json`: `{"format_version":1,"created_unix":…,"label":…,
  "files":[{"name":"manual.db","bytes":…,"sha256":"…","schema_version":…},…]}`.
  `schema_version` is the value the owning crate reports (read it from the copied
  file, do not guess).
- `restore` is **offline and explicit**: verify the manifest shape, every file's
  size and SHA-256, and `PRAGMA integrity_check` on each copied database; only
  then replace the destination files. Write to a temp file in the destination
  directory and rename into place, so a crash cannot leave a half-written
  database. Refuse without `--force` when a destination file already exists, and
  print exactly what will be replaced.
- `backups` lists directories with created time, label, sizes and whether each
  verifies.
- Non-zero exit and a clear message on any failure. Never delete a backup.

## Acceptance / required evidence

1. `cargo test -p oura-cli` passes, with tests covering:
   - backup then restore round trip restores identical row contents
   - a backup taken while a writer has uncommitted WAL data includes that data
   - a tampered file (one byte flipped) fails checksum verification and nothing
     is replaced
   - a truncated database fails `integrity_check` and nothing is replaced
   - `restore` without `--force` refuses when the destination exists
   - `backups` reports a valid backup as verifying and a corrupted one as not
   - a restore failure leaves the previous destination file intact
2. `scripts/backup-restore-smoke.sh` runs the real binary end to end in Docker:
   seed a `manual.db` with entries, back it up, delete a row, restore, and show
   that the deleted row is back; then corrupt the backup and show the restore is
   refused. The script must be safe to re-run and must clean up after itself.
3. Paste the raw output of both runs.

## Out of scope

Scheduling (a systemd timer is D8), encryption at rest, remote/off-site copies,
ring generation pointers (D5), and any change to `crates/oura-manual`.
