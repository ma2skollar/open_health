# Raspberry Pi Deployment Guide — the collector host

Target: one Raspberry Pi 4 (64-bit OS) running the collector that talks to the
Oura Ring over Bluetooth and delivers snapshots to the LXC hub. **No hardware has
been validated.** Compilation and simulator success are not hardware proof; see
`docs/HARDWARE_ACCEPTANCE.md`.

Until the ring and Pi exist, use the simulator in `docs/OPERATIONS.md` or the
`oura-collector-sim` crate to exercise the same contracts.

## 0. What the Pi owns

| Item | Path | Notes |
|---|---|---|
| Ring authentication key | `/etc/oura/ring.key` | `0600`, root only. Never copied to the LXC. |
| Collector credential | `/etc/oura/collector.token` | `0600`, separate file from the key. |
| Ring database | `/var/lib/oura-collector/ring.db` | local storage only, never SMB/NFS |
| Collector state/outbox | `/var/lib/oura-collector/` | local storage only |
| Config | `/etc/oura/collector.toml` | no secret bytes in this file |
| Unit | `/etc/systemd/system/oura-collector.service` | |

The collector opens the authenticated outbound WebSocket to the hub. No inbound
port on the Pi is required. Do not share credentials with a browser.

## 1. Prepare the OS

Flash a 64-bit Raspberry Pi OS (or Debian ARM64) image. Boot, connect to Wi-Fi,
and set a stable LAN address (DHCP reservation preferred).

```sh
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libsqlite3-dev libdbus-1-dev \
  libdbus-1-3 bluez
```

BlueZ and a D-Bus session are required at runtime. The real transport compiles out
debug/trace events because upstream link frames can contain authentication
material; do not enable debug/trace logging on this host.

## 2. Build the collector

Build natively on the Pi for the real transport:

```sh
rustup target add aarch64-unknown-linux-gnu   # only if cross-building elsewhere
cargo build --release -p oura-collector --features ble
```

Cross-compilation from the LXC or a desktop works for compilation checks, but it
does not prove Bluetooth works. A native build on the Pi is the honest path for
hardware acceptance. The binary lands at
`target/release/oura-collector`.

## 3. Install the binary and configuration

```sh
sudo install -d -m 0750 -o root -g root /etc/oura
sudo install -d -m 0750 -o oura -g oura /var/lib/oura-collector
sudo install -m 0755 target/release/oura-collector /usr/local/bin/oura-collector
sudo install -m 0640 -o root -g oura deploy/collector.toml.example /etc/oura/collector.toml
```

Place the ring key and collector token yourself, both `0600` and root-owned. The
collector never pairs, resets, enters DFU/flight mode or clears ring data
automatically. Provision the ring key explicitly.

## 4. Configure `/etc/oura/collector.toml`

Replace every placeholder. Never copy authentication key bytes into this file.

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

Matching `OURA_*` environment variables override file fields. Use HTTPS for the
hub. The collector opens the authenticated outbound WebSocket itself.

An epoch change requires hub continuity review; it never replaces complete
history automatically. Keep the ring key and collector credential in separate
`0600` files outside the database and outbox.

## 5. Run as a service

Install a unit matching the existing dashboard pattern:

```ini
[Unit]
Description=Oura Ring collector
After=network-online.target bluetooth.target
Wants=network-online.target

[Service]
Type=simple
User=oura
Group=oura
ExecStart=/usr/local/bin/oura-collector /etc/oura/collector.toml
Restart=on-failure
RestartSec=10
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/lib/oura-collector

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now oura-collector.service
```

SIGINT and SIGTERM request local live-session teardown before the actor exits.

## 6. Collection behaviour

- One actor serializes all BLE operations. It owns scheduled sync, interactive
  sync, device queries, feature queries and live measurements. Do not launch
  competing Bluetooth clients.
- A configurable morning wake window runs a bounded number of reconnect attempts
  per window. `retry_max_secs` also sets the normal successful collection
  interval. Transient upload retries continue at the capped backoff until the hub
  returns.
- No overlapping collection jobs. Persistent collection state survives reboot.
- History uses the transport's 120-second deadline and a 125-second coordinator
  watchdog, so checkpointed partial progress is returned rather than lost to an
  earlier outer timeout.
- Live capture is capped at 60 seconds. A separate bounded 15-second SDK
  setup/restoration grace applies; stop and cancellation still interrupt.
- A stale `CONNECTED_LIVE` mode is reconciled before another start and never
  restored after completion, preventing an orphaned live re-enable after a crash.
- Collection continues independently of the control channel. Control-channel
  disconnects do not stop scheduled collection. An upload may delay the next BLE
  operation by at most its 30-second request timeout.

## 7. Outbox and storage pressure

Snapshots are retained rather than superseded. A full outbox or low disk refuses
new snapshots explicitly and retains pending ones. Resolve storage pressure before
collection can enqueue another snapshot. Upload failures persist bounded retry
state across restart. Monitor queue size and last-success time; Pi connectivity
does not prove ring availability. Control results distinguish `outbox_full` from
`low_disk`. Committed but not yet snapshotted history sets `pending_snapshot` in
the collector-status command result; its fsynced on-disk marker survives restart.

## 8. Demo without Bluetooth

```sh
cargo run -p oura-collector-sim -- demo
cargo run -p oura-collector-sim -- reset-demo
```

Demo identity is explicit and all data lives in a private temporary sandbox.
`reset-demo` resets only demo state and does not accept a production path. Demo
runs never prove ring compatibility.

## 9. Hardware acceptance

Before relying on collection, complete `docs/HARDWARE_ACCEPTANCE.md` on real
hardware: pairing, authentication, a real history sync, a bounded live HR capture,
battery/device queries, and recovery from a ring-absent window. Until then, every
ring metric remains source-supported but hardware-unverified.
