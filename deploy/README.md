# deploy

Deployment for one **unprivileged** Proxmox LXC: native systemd, SQLite, no
Docker nesting. The dashboard binds loopback only; a reverse proxy terminates TLS
in front of it. **The proxy is not chosen yet** — nothing here configures one.

## Layout

| Path | Purpose |
|---|---|
| `/srv/oura/bin/oura` | the binary, root-owned, `0755` |
| `/srv/oura/data/` | databases, owned `oura:oura`, `0750` |
| `/srv/oura/backups/` | backup destinations, owned `oura:oura`, `0750` |
| `/etc/oura/oura.env` | configuration, root:oura, `0640`, **no secrets** |
| `/etc/systemd/system/oura-*.service` | units |

## Install

```sh
cargo build --release -p oura-cli -p oura-backup
sudo ./deploy/install.sh --bin ./target/release/oura --backup-bin ./target/release/oura-backup
sudo ./deploy/install.sh --dry-run         # print the actions, change nothing
```

The script is idempotent: a second run reports what already exists, never
overwrites `/etc/oura/oura.env`, and skips an unchanged binary or unit.

### Privileged steps

Everything requiring root is in the marked section of `install.sh`:
creating the `oura` system user and group, creating `/srv/oura` and `/etc/oura`,
installing the binary and unit files, and `systemctl daemon-reload` /
`enable --now`. Nothing else needs root. No step deletes anything.

## Configuration

Copy `oura.env.example` to `/etc/oura/oura.env` (the installer does this) and
replace every `__PLACEHOLDER__`. The ring authentication key is **never** stored
in this file, in this repository, or in a unit file: it lives on the collector,
`0600`, and only there.

`--manual-db` is not yet on `ExecStart`; add it when the manual-metrics routes
land (D3), because `oura dashboard` does not accept the flag yet.

## Health

```sh
./deploy/health.sh
```

Checks service state, that the dashboard answers on its loopback port, free disk
against a floor, that the manual database opens read-only and passes
`quick_check`, and — once ingestion exists — the age of the newest accepted
upload. Exit 0 healthy, 1 otherwise. Output is redacted; no token, key, or health
payload is ever printed.

## Backup

`oura-backup` creates a timestamped directory holding a consistent copy of each
database plus a manifest with the size, SHA-256 and schema version of every file.
Copies use the SQLite online backup API, so a live database with an uncommitted
WAL is captured correctly — a plain file copy would not be.

```sh
oura-backup create  --dest /srv/oura/backups --db /srv/oura/data/oura.db --manual-db /srv/oura/data/manual.db
oura-backup list    --dest /srv/oura/backups
oura-backup verify  --dir  /srv/oura/backups/<timestamp>-scheduled
oura-backup restore --dir  /srv/oura/backups/<timestamp>-scheduled --dest /srv/oura/data --force
```

`restore` verifies first — manifest shape, size, checksum and
`PRAGMA integrity_check` on every file — and refuses an existing destination
without `--force`. Each file is renamed into place, so a refused or failed
restore leaves the previous file untouched. **It never deletes a backup.**

`oura-backup.timer` runs `create` daily with a random delay and `Persistent=true`.

Restore ordering, and the fact that a Proxmox VE snapshot alone is **not** a
backup, are in `docs/OPERATIONS.md`.

## Upgrade and rollback

Upgrade: install the new binary, `systemctl restart oura-dashboard.service`,
then re-run `health.sh`. Migrations are forward-only.

Rollback: stop the service, restore the previous binary, restore `manual.db` and
the previously active ring generation from a verified backup, then restart and
re-check. Restore *before* restart, never after.

## ARM64 (Raspberry Pi 4, hardware acceptance pending)

Build the collector with the real transport on the Pi:

```sh
rustup target add aarch64-unknown-linux-gnu
cargo build --release -p oura-collector --features ble
```

Native dependencies: `build-essential`, `pkg-config`, `libsqlite3-dev`,
`libdbus-1-dev` (build), `libdbus-1-3` (runtime), BlueZ and a D-Bus session.
Do not enable debug/trace logging: upstream link frames can contain authentication
material. The real transport compiles out debug/trace events.

The collector owns its local SQLite ring store and a separate durable state/outbox
directory. Keep both on local storage, never SMB/NFS. Configure `collector_id`,
`epoch`, `ring_name`, `key_file`, `database_path`, `hub_url`, `token_file`,
`wake_start`, `wake_end`, `timezone` (IANA), retry bounds, outbox limits,
`state_dir`, `build_revision` and `identity = "production"` in TOML. Matching
`OURA_*` environment variables override file fields. Use HTTPS for the hub; the
collector opens the authenticated outbound WebSocket itself. No inbound Pi port
is required. An epoch change requires hub continuity review, not automatic
replacement of complete history.

Example `/etc/oura/collector.toml` (replace host, ring name, revision and epoch;
never copy authentication key bytes into configuration):

```toml
collector_id = "pi-01"
epoch = "pi-01-2026-11-07-1"
ring_name = "Oura"
key_file = "/etc/oura/ring.key"
database_path = "/var/lib/oura-collector/ring.db"
hub_url = "https://oura.example.invalid"
token_file = "/etc/oura/collector.token"
wake_start = "06:00"
wake_end = "10:00"
timezone = "Europe/Stockholm"
retry_min_secs = 5
retry_max_secs = 300
max_attempts = 5
outbox_max_files = 32
outbox_max_bytes = 268435456
min_free_bytes = 67108864
identity = "production"
state_dir = "/var/lib/oura-collector"
build_revision = "deployed-build-revision"
```

`retry_max_secs` also sets the normal successful collection interval. The
scheduler spends at most `max_attempts` reconnect attempts per wake window;
transient upload retries continue at the capped backoff until the hub returns.

Start with `target/release/oura-collector /etc/oura/collector.toml`. The real
binary refuses demo identity; simulator runs use their own executable. SIGINT
and SIGTERM request local live-session teardown before the actor exits.

Keep the ring key and collector credential in separate `0600` files outside the
database and outbox. Provision the ring key explicitly; the collector never
pairs, resets, enters DFU/flight mode or clears ring data automatically. Do not
share credentials with a browser. Never place secrets in example configuration.

Full outbox or low disk raises an explicit error and retains pending snapshots.
Resolve storage pressure before collection can enqueue another snapshot. Upload
failures persist bounded retry state across restart. Monitor queue size and
last-success time; Pi connectivity does not prove ring availability.
Control results distinguish `outbox_full` from `low_disk`. Committed but not yet
snapshotted history sets `pending_snapshot` in the collector-status command
result. Its fsynced on-disk marker survives restart; recovery does not require
another successful ring connection.
The same actor serializes outbox mutations with collection. An upload may delay
the next BLE operation by at most its 30-second request timeout; control-channel
disconnects do not stop scheduled collection. Snapshots are retained rather than
superseded, avoiding any unproven history-containment assumption.

Live capture is capped at 60 seconds. The watchdog allows a separate, bounded
15-second SDK setup/restoration grace; stop and cancellation still interrupt
the operation. A stale `CONNECTED_LIVE` mode is reconciled before another
start and is never restored after completion. History uses the transport's
120-second deadline and a 125-second coordinator watchdog, allowing the
transport to return checkpointed partial progress instead of losing it to an
earlier outer timeout.

For a Bluetooth-free demo, run `cargo run -p oura-collector-sim -- demo`.
Demo identity is explicit and all data lives in a private temporary sandbox.
`cargo run -p oura-collector-sim -- reset-demo` resets only demo state; it does
not accept a production path.

**Compilation and simulator success are not hardware verification.** Nothing
here proves Ring 5 pairing, Bluetooth operation or any measured metric. Complete
`docs/HARDWARE_ACCEPTANCE.md` on real hardware before relying on collection.

## Deviations from the handoff

`docs/handoffs/D8-deployment.md` asked the installer to "refuse to run as root
for the non-privileged steps". Every step of an installer *is* privileged, so the
script instead requires root explicitly and confines all privileged actions to
one marked section.
