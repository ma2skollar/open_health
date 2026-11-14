# Raspberry Pi history-only collector

This is the D1–D3 Pi deployment path. The Pi owns BLE transport, framing/decryption required by transport, and a durable raw outbox. The LXC owns physiological decoding, clock reconstruction, analysis, models, and authoritative storage. No model belongs on the Pi.

Software builds and synthetic checks are not hardware acceptance. Pairing, ring contact, installation, and production activation need separate approval. Nothing in this runbook authorizes changes to the LXC, NPM Plus, Authentik, firewall, TLS deployment, or network.

## Platform and access prerequisites

Use a Pi 4 or Pi 5 with a supported **64-bit** Raspberry Pi OS or Debian ARM64, systemd, BlueZ, and the system D-Bus. A 32-bit OS is not supported. The packaged service uses BlueZ over the system bus, not direct Bluetooth device access. The dedicated `oura-collector` account has no login shell and belongs to `bluetooth`. Verify the installed BlueZ D-Bus policy permits this account; never solve permission failures by running the collector as root or granting broad capabilities.

Before deployment, the user must apply OS security updates, configure key-only SSH for administrators, restrict SSH to trusted administration hosts, and review unattended-update/reboot policy. Keep the collector account out of SSH and sudo. Use a DHCP reservation and working DNS. NTP synchronization must be healthy; Linux `adjtimex` reports unsynchronized clocks and the collector fails closed for signing. An ordered `time-sync.target` alone is not evidence of synchronization.

Use reliable local storage, not NFS/SMB. Reserve more space than the outbox budget and reserve combined, plus OS and bounded journal needs. Place the Pi beside the bed with a stable power supply, clear radio placement, and a short unobstructed path to the ring. RF range, adapter compatibility, battery use, and overnight coverage remain unverified until measured on the actual hardware.

The Pi makes outbound HTTPS requests only to the configured ingest host. No inbound collector port, WebSocket, control channel, or browser credential is needed. A LAN name still needs a valid hostname-matching certificate chain. The user supplies private CA trust when required; there is no insecure mode.

## Reproducible ARM64 build

Build the actual production transport, not the default synthetic-only feature set:

```sh
# Public tool downloads and extraction only; no OS packages are installed.
bash deploy/pi/prepare-build-tools.sh
source target/pi-tools/env.sh
# Record source identity without reading secrets or requiring a Git checkout.
export OURA_COLLECTOR_BUILD="pi-d1-d3-$(
  { find crates/oura-collector crates/oura-ble crates/oura-protocol-v1 -type f \( -name '*.rs' -o -name Cargo.toml \) -print0 |
      sort -z | xargs -0 sha256sum; sha256sum Cargo.lock; } | sha256sum | cut -c1-16
)"
cargo build --locked --release -p oura-collector --features ble --target aarch64-unknown-linux-musl
file target/aarch64-unknown-linux-musl/release/oura-collector
readelf -h target/aarch64-unknown-linux-musl/release/oura-collector
readelf -l target/aarch64-unknown-linux-musl/release/oura-collector
sha256sum target/aarch64-unknown-linux-musl/release/oura-collector
```

The bootstrap pins Rust 1.100.0 (2026-11-12) and Zig 0.14.1 for an x86_64 Linux build host. It checks Rust component hashes from the public distribution and the pinned Zig SHA-256. All tools, caches, downloads, and build output stay under ignored `target/`. Zig supplies host C compilation and the ARM64 musl C compiler/linker; Rust supplies the ARM64 standard library. SQLite and D-Bus are vendored by the collector's dependencies. BlueZ and the system D-Bus daemon remain runtime prerequisites. Rustls performs HTTPS verification; no system OpenSSL cross dependency is needed.

`env.sh` exports `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C link-self-contained=no"`. Zig's
musl already supplies crt objects; without this flag Rust's self-contained crt duplicates `_start` and
the link fails with `ld.lld: error: duplicate symbol: _start`.

Require an ELF64 little-endian AArch64 executable. Inspect the program headers as well as the architecture; a static musl build should not need an ARM64 dynamic interpreter on the build host. Save the checksum with the released artifact outside source control. Do not substitute `cargo check`, a script, or a renamed host executable for the linked artifact.

If an isolated `qemu-aarch64-static` is available, run only the non-collecting CLI surface:

```sh
QEMU=target/pi-tools/qemu/usr/bin/qemu-aarch64-static
BIN=target/aarch64-unknown-linux-musl/release/oura-collector
"$QEMU" "$BIN" --help
"$QEMU" "$BIN" --version
"$QEMU" "$BIN" validate deploy/pi/collector.toml.example
```

`--help`, `--version`, and `validate CONFIG.toml` do not read keys, open the outbox, or contact a ring. `validate` checks configuration only; it does not establish operational readiness. Emulator execution does not establish BlueZ, D-Bus authorization, RF, pairing, or Pi compatibility. Run narrow synthetic tests and installer/service checks only after edits are complete and the integration owner approves the verification gate.

## Inspect installation without applying it

```sh
bash deploy/pi/install.sh --dry-run --binary target/aarch64-unknown-linux-musl/release/oura-collector
systemd-analyze verify deploy/pi/oura-collector.service
```

The installer defaults to dry-run and prints shell-escaped commands. It does not require root for dry-run. Missing artifacts are reported; `--apply` will refuse them. Neither mode reads or installs a key, pairs a ring, changes features, changes networking, or starts/enables the service. The apply path requires root on ARM64 Linux and validates the ELF header before changing the host. It installs a dedicated service account, directories, binary, config example, runbook, and unit, then reloads systemd. Existing config is preserved. Existing enabled/running service state is not changed by the installer; stop the service manually before an approved upgrade.

Only after separate deployment approval, the user may run:

```sh
sudo bash deploy/pi/install.sh --apply --binary /approved/path/oura-collector
```

Do not run this command on the build host. No installer application was authorized during implementation.

## Configuration and secrets

Edit `/etc/oura-collector/collector.toml` from `deploy/pi/collector.toml.example`. Replace every `CHANGE_ME` placeholder. All fields are top-level TOML; unknown fields fail validation. There are no environment-variable overrides or demo identities.

| Field | Meaning / default |
| --- | --- |
| `device_id` | Required registered ASCII identifier, agreed with backend operator |
| `ring_name` | Required BLE ring name; not an authorization credential |
| `ring_key_file` | Required absolute external ring key path |
| `state_dir` | Required absolute private service-owned local directory |
| `ingest_url` | Required exact `https://HOST[:PORT]/ingest/v1/batches`; no userinfo, query, or fragment |
| `key_id` | Required backend-registered active HMAC key identifier |
| `signing_key_file` | Required absolute external signing-key path |
| `ca_certificate_file` | Optional absolute PEM private-CA path; chain and hostname verification remain required |
| `max_storage_bytes` | 1 GiB total DB/WAL/SHM budget |
| `min_free_bytes` | 256 MiB filesystem free-space reserve |
| `history_max_events` | 256 events per bounded collection |
| `history_max_bytes` | 131072 raw body bytes per bounded collection |
| `history_max_pages` | 16 transport history pages per attempt |
| `collection_interval_secs` | 300 seconds |
| `heartbeat_interval_secs` | 900 seconds |
| `operation_timeout_secs` | 30 seconds |
| `retry_min_secs`, `retry_max_secs` | 5 and 900 seconds; bounded backoff with jitter |

The config directory is root-owned, group `oura-collector`, mode `0750`; config is root-owned, group `oura-collector`, mode `0640`. Both external key files must be regular non-symlink files, owned by the collector's effective user (`oura-collector` under systemd), with mode **exactly `0600`**. The signing key is exactly 32 raw bytes, not hex text. The ring key is 16 raw bytes or 32 hexadecimal characters. Unsafe ownership/modes fail closed. Never put key bytes in TOML, environment variables, command arguments, shell history, source control, logs, or troubleshooting reports. The LXC never receives the ring key.

The installer deliberately does not generate, read, print, copy, or move any key. Have the user provision approved credentials through a private process after establishing backend registration. Do not touch existing credentials as part of build or testing. Provision a private CA certificate separately if needed; certificates are not signing keys.

## Approved-only onboarding

1. Keep pairing blocked until the user explicitly approves it for the actual Pi/ring. Building the collector does not approve pairing.
2. Use one central device. The user must coordinate the phone/app and any other BLE tools so they do not race the Pi. Never run a second collector against the same ring or state directory.
3. After approval, use a separately reviewed standalone provisioning tool manually on the Pi to pair/authenticate and create the Pi-owned ring key. Inspect that tool's exact command help first. The production collector exposes only `validate` and `run`; it has no provisioning command and never falls back to pairing.
4. A reset/factory reset may destroy unsynchronized ring data and pairing state. It needs its own explicit approval, not merely approval to pair. Never reset to work around an authentication, connection, or storage error.
5. No startup, reconnect, shutdown, installer, or health check changes feature modes, starts live HR/workouts, invokes DFU/OTA, changes flight mode, or performs destructive recovery. Do not add `--include-state`, `--include-danger`, or `--yes` to onboarding recipes.
6. Keep all model files and physiological processing on the LXC. Do not run research/model-download tools from service startup.

### Manual pairing command, only after explicit approval

No pairing is needed if an eligible service-owned key already exists in place on the Pi. Do not copy, move, print, or remove an existing ring key. The separate `oura` provisioning CLI is not included in the collector artifact; use a reviewed build of that tool without installing a server or models.

For a fresh approved provisioning on the actual Pi, the user may create a private key-only directory. The root-owned config directory remains unwritable by the collector account:

```sh
sudo install -d -m 0700 -o oura-collector -g oura-collector /etc/oura-collector/keys
```

After confirming that the ring is eligible and pairing is approved, the user may run the following command manually as the service user. It creates a missing fresh key in place with mode `0600`. Output is suppressed because the standalone tool prints device/authentication diagnostics; do not capture these streams in a log:

```sh
sudo -u oura-collector env RUST_LOG=off oura --key-file /etc/oura-collector/keys/ring.key pair >/dev/null 2>&1
```

An existing key causes reuse, not regeneration. A newly created key can persist even when installation/authentication fails. Never remove or regenerate it after a failure; stop and obtain a separate recovery decision. Reset remains separately approval-gated and may lose all unsynchronized history. Provision the signing key separately through the backend-approved private process, with the same owner and exact `0600` mode. Neither command above ran during implementation.


## Manual activation gate

Backend B1 must implement and review the shared HMAC ingest contract first. The user must complete C1–C3: exact ingest-route proxy policy, Pi-only access control, backend IPv4/IPv6 bypass prevention, TLS/CA trust, body/rate limits, and private logs. No agent supplies or applies nginx/firewall configuration here.

After credentials, private CA, backend contract, NTP, and hardware permissions are ready, validate config without contacting the ring:

```sh
sudo -u oura-collector /usr/local/bin/oura-collector validate /etc/oura-collector/collector.toml
```

Only after separate approval for real collection:

```sh
sudo systemctl enable --now oura-collector.service
sudo systemctl status oura-collector.service
```

The service has an empty capability set, no login privileges, private temporary/devices namespaces, a read-only system/home view, restricted address families, and one writable state directory. It needs the system D-Bus socket and outbound DNS/HTTPS, so `PrivateNetwork=yes` is intentionally absent. `ProtectClock=yes` is intentionally absent because even the read-only `adjtimex` clock check would be blocked. Failed startup retries are rate-limited. Shutdown is history-only and does not start or restore live capture.

Logs contain bounded safe error codes, not ring frames, bodies, key bytes, signatures, credentials, or response payloads. Keep normal logging; never enable upstream debug/trace authentication-frame logging. The user must set bounded journal retention and ensure proxy/backend access logs omit authentication headers and request bodies. Report error codes and timestamps only.

## Outbox, recovery, and key rotation

`state_dir/outbox.sqlite3` stores the persistent epoch, monotonically allocated source IDs, cursor, immutable batch bodies, durable receipt metadata, and retry state. `state_dir/collector.lock` enforces one owner. The epoch is a store identity, not a process-boot identifier. Never delete or recreate state to recover from an outage, clock error, reboot, or ring absence.

Cursor and raw batches commit atomically in SQLite WAL mode with `synchronous=FULL`. Partial completed checkpoints are retained. Raw counters are **not UTC**. `captured_unix_ms` records collection time, not the historical event's physiological time. The collector emits `clock_anchor=null` unless a genuine clock observation exists; it never pairs an old event counter with download time as a fabricated anchor. Raw transport records preserve original timestamp encodings for later LXC reconstruction. Counter regression persists a fail-closed collection gate; it does not silently reset the cursor or erase post-reboot identities.

The bounded raw parser does not call upstream physiological event decoders. It preserves unknown history tags and exact legacy packets or extended records, including native timestamp varints. A potentially truncated page leaves its entire final decisecond group on the ring for the next fetch; it never fabricates unique timestamps. If a complete group cannot fit, `history_limit` stops collection without advancing over it. Increase a bound only after reviewing memory and disk capacity. An empty incremental fetch probes the preceding cursor marker; a missing marker or rejected cursor fails closed instead of silently resetting to zero. Counter-discontinuity recovery requires a separate operator decision.

Capacity is preflighted before BLE. The physical budget includes DB, WAL, and SHM. SQLite's page limit is `(max_storage_bytes - 1 MiB) / 3`, rounded down to pages, reserving WAL/checkpoint slack. Usable payload capacity is therefore below one third of the nominal 1 GiB. On low disk/full outbox, collection stops safely, keeps unacknowledged data, and reports pressure. Never evict pending data or manually advance the cursor. Restore connectivity, resolve unrelated disk pressure, or approve a larger budget before resuming. A larger configured budget needs adequate real free space.

An existing outbox can reopen below the free-space reserve so uploads and exact acknowledgements can drain it; new collection and heartbeat admission remain blocked. First initialization is transactional; an empty interrupted store can initialize, but payload or receipts without their established identity fail closed. Backward wall-clock correction rebases persisted scheduling deadlines within configured delays without changing frozen bodies or capture timestamps. Normal backoff survives restart. Clock observation reads state without a per-second disk commit.

Upload retries preserve exact body bytes and batch identity across restart, but use a fresh nonce and timestamp at each attempt. An uncertain acknowledgement leaves data pending. Only an exact matching durable receipt authorizes transactional receipt persistence and local batch deletion. A conflicting receipt, wrong device/batch/hash/version, or non-durable response never permits deletion. A successful receipt proves durable raw ingestion, **not** decoding or analysis completion.

Do not copy a live SQLite file alone for backup. Stop the collector and use a WAL-aware backup procedure or a consistent offline copy of all required state. Protect outbox backups as health data. Retain the original state for recovery review; never create a new epoch to conceal continuity loss. Signing keys need separate protected recovery and must not be bundled with routine data diagnostics.

For planned HMAC rotation, coordinate overlapping accepted key IDs with backend B1. The collector rereads the selected external key for transmission. Provision the replacement as a private service-owned `0600` regular file without exposing bytes, update `key_id` and `signing_key_file` together, and restart the service under user control. Pending batches keep their identities and bytes; only transmission credentials change. Revoke the old backend key after successful receipt checks and the agreed overlap window. Never confuse the ring key with the signing key.

## Shared wire contract

The source of truth is `oura_protocol_v1::ingest`, including fixed public synthetic interoperability vectors. B1 must consume that module rather than invent another bearer/snapshot contract.

* Exact `POST /ingest/v1/batches`, no query, redirect, or compression; exact `Content-Type: application/json`.
* Maximum body 4 MiB, 10000 events, and 65536 decoded bytes per event body. Raw bodies use lowercase hex. Metadata and identifiers are bounded by the shared validator.
Batch fields: `version`, `device_id`, `collector_epoch`, `batch_id`, `collector_build`, `device`, `events`, `heartbeat`. Device fields are `serial`, `firmware`, `hardware`. Event fields are `source_row_id`, `tag`, `ring_counter`, `body_hex`, `captured_unix_ms`, `clock_anchor`, and optional `transport`. Transport provenance retains exact raw records, original counters, and explicit units. See [the complete shared contract](pi-ingest-contract.md) for field bounds, encodings, units, and fixed vectors. Anchor fields are `kind`, `ring_counter`, `captured_unix_ms`; supported kinds do not imply that the collector has observed such an anchor.
* Source identity is `(device_id, collector_epoch, source_row_id)`. Receipt identity is `(device_id, batch_id)` plus exact body hash. No content-based reboot deduplication.
* Heartbeat uses the same signed route, with an empty `events` array when appropriate. It carries BLE attempt/success times, upload receipt time, queue count/bytes/oldest capture, battery, disk free, Pi temperature, clock synchronization, and bounded safe error codes. Missing observations stay absent, not zero. Build is in the batch envelope.
* Header names: `x-oura-key-id`, `x-oura-device-id`, `x-oura-timestamp`, `x-oura-nonce`, `x-oura-body-sha256`, `x-oura-signature`.
* Hash exact transmitted bytes using SHA-256; encode hash/signature as lowercase hex. Nonce is 16 random bytes encoded as 32 lowercase hex characters. Timestamp is integer Unix UTC seconds, accepted within ±300 seconds. Backend replay records must be persistent and atomic for at least 900 seconds.

HMAC-SHA-256 canonical bytes are UTF-8 lines, each terminated by LF, **including the last line**:

```text
open-health-ingest-v1
{key_id}
{device_id}
POST
/ingest/v1/batches
{timestamp}
{nonce}
{body_sha256}
```

Acknowledgement fields are `version`, `device_id`, `batch_id`, `body_sha256`, `durable`, `accepted_unix_ms`. The collector validates all binding fields and requires `durable=true` before deletion. Analysis completion is separate.

## Evidence and remaining gates

Save actual build/check commands, linked architecture/program-header output, SHA-256, non-collecting CLI evidence, installer dry-run, service validation, and narrow synthetic outcomes. A failed or unavailable check is not a pass. Build-tool/container availability and emulator failures belong in the evidence, not hidden fallbacks.

Hardware acceptance remains separate: actual Pi OS/BlueZ permissions, approved sole-central pairing, transport authentication, bounded history collection, persistent cursor across reboot, offline/outage recovery, disk-pressure recovery, NTP loss, RF placement, battery behavior, and end-to-end durable receipts through the user-configured proxy. Backend B1/B2 and C1–C3 remain independent gates. No live-HR, workout, reset, or feature-change exercise is part of D1–D3 acceptance.
