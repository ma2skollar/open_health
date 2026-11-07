# OPERATIONS

Placeholders are marked `⟨…⟩`. Nothing here has been deployed.

## Runtime dependencies

| Component | Requirement | Evidence |
|---|---|---|
| Linux toolchain (build) | Rust ≥ 1.85 (verified with 1.99.1) | Rust 1.83 fails on `clap_lex 1.1.0` (`edition2024`) |
| Build packages | `build-essential`, `pkg-config`, `libdbus-1-dev` (BlueZ/D-Bus headers) | `btleplug → bluez-async → libdbus-sys` |
| Runtime library | `libdbus-1-3` — the dashboard aborts without it | observed: `error while loading shared libraries: libdbus-1.so.3` |
| SQLite | bundled in the binary on Linux; no system package needed | `vendor/oura-store/Cargo.toml` |
| TLS | **none in the binary** | the only TLS user was `ureq`, removed with the DNA/blood panels (ADR 0006); `Cargo.lock` no longer contains `ureq`, `rustls`, `webpki`, or `ring`. TLS is provided by the reverse proxy in front of the loopback listener |

## ARM64 (Raspberry Pi 4) build — checked, not hardware-verified

`cargo check --locked -p oura-cli` succeeds for `aarch64` (run under
`--platform linux/arm64`). That covers every Rust crate, the bundled C SQLite
build script, and the BlueZ/D-Bus crates. It does **not** produce a linked ARM64
binary and it says nothing about Bluetooth. Pi-side instructions and the native
dependency list are in `deploy/README.md`.

## Build-time vs runtime network

Build/install downloads (expected, not a runtime dependency):
crates.io, the pinned `open_oura` git dependency, distribution packages for the
build image.

Runtime network: must be exactly the collector upload/control channel plus
browser traffic on the LAN. The DNA PGS download and the
`agent-backend.thomas.md` blood normalizer are being removed
(`DECISIONS/0006`). **Offline operation is not yet tested** and must not be
claimed until it is.

## Distribution model (assumed — to be confirmed)

- **LXC**: one unprivileged Proxmox VE container, `⟨distro⟩`, `⟨cores/RAM/disk⟩`,
  reached over `⟨LAN/VPN⟩` at `⟨hostname⟩` / `⟨IP⟩`.
- **Persistent storage**: `⟨data path⟩`, e.g. `/srv/oura` with
  `manual.db`, `ring/<epoch>/<gen>.db`, `hub state`, `backups/`.
- **Access**: SSH to the LXC is **not yet discovered** — no SSH config, no
  known_hosts entry, and no Orca connected server or saved host exists on this
  machine. Confirmed by `orca host list` (local only) and an empty
  `~/.ssh/known_hosts` scan for a Proxmox host.

## Process layout (target)

| Unit | User | Purpose |
|---|---|---|
| `oura-hub.service` | `oura` | collector ingest, generations, control WebSocket, audit |
| `oura-dashboard.service` | `oura` | browser UI + API, manual metrics |
| `oura-backup.timer` | `oura` | consistent backup of `manual.db` + active ring generation |
| reverse proxy | `⟨caddy/nginx⟩` | TLS termination, browser authentication, LAN-only listen |

Services run without root. Privileged steps are limited to package installation
and service-unit placement, and are listed explicitly in the install script.

## Backup, restore, continuity

- Back up `manual.db` and the active ring generation with a consistent method
  (SQLite backup API / `VACUUM INTO`), never a raw copy of a live WAL database.
- Record the active-generation pointer with the backup so a restore is
  unambiguous.
- Restore order: `manual.db` first (never replaceable by ring data), then the
  ring generation, then the pointer.
- A Proxmox VE snapshot alone does **not** guarantee application consistency;
  do not treat it as the backup.
- The Pi may buffer during outages; on restore, a rebuilt collector must not
  replace complete server history with partial history.

## Observability

Structured, redacted logs: never log the ring auth key, collector tokens, or
health payload bodies. Rotate by size and age. Track: pending delivery queue
depth, disk free, last successful collection, last accepted upload, newest
measurement, last control contact, recent actionable errors.

## Upgrade / rollback

Application versions are pinned by `Cargo.lock` and the recorded upstream
revisions. Rollback = restore the previous binary plus its `manual.db` and the
previously active ring generation. Migration of `manual.db` is forward-only and
versioned; downgrade requires a restored backup.

## Ingestion

Build the hub with `cargo build --release -p oura-hub`. It is a separate
process; it never changes `manual.db`, rewrites a ring database in place, or
starts a control-channel listener. A reverse proxy must terminate TLS and
restrict collector access. The hub refuses non-loopback bind addresses.

Set `OURA_HUB_DATA_DIR` to a dedicated writable directory and
`OURA_COLLECTOR_TOKEN_FILE` to a private file containing the collector token.
Keep that file outside the data directory and protect it with mode `0600`.
The token is never returned in API responses or persisted in operational state.
Start `oura-hub` with no arguments to serve HTTP. Configure optional limits with
`OURA_HUB_MAX_BYTES` (default 536870912), `OURA_HUB_TIMEOUT_SECONDS` (120),
`OURA_HUB_RATE_LIMIT` (60 requests per collector per 60 seconds), and
`OURA_HUB_BIND` (default IPv4 loopback, port 8040). Only one hub process may own a data
root at a time. Stop it before running an operator subcommand.

Upload `POST /api/v1/collector/snapshot` with `Authorization: Bearer <token>`,
`Content-Type: application/octet-stream`, and `X-Oura-Metadata` containing the
single-line JSON `UploadMetadata` object. The body is exactly the SQLite file,
without multipart boundaries, leading lengths, or trailing bytes. Metadata
includes `protocol_version: 1`; acknowledgements and errors also carry version 1.
Use `GET /api/v1/hub/status` with the same bearer credential for redacted
operational state. Do not put credentials in URLs or shell command history.

The hub stages restricted `0600` files under `staging/<collector_id>/<uuid>`.
Collector IDs allow ASCII letters, digits, hyphens, underscores and dots, but
not `.` or `..`. Epochs are opaque strings and may contain `/`; on disk their
UTF-8 bytes are encoded as lowercase hexadecimal. Immutable files live at
`ring/<collector_id>/<hex-epoch>/<generation>.db`. Each collector's
`active.json` records active and previous known-good generations. Read this
pointer afresh when opening a database; existing read-only readers may keep
their old generation open. Files and directories are synced before a successful
acknowledgement. Never edit a generation or pointer manually.

A new epoch is durably stored but returns `message: "pending_review"` and does
not replace the active epoch. Review its history, stop the running hub, then run:

```sh
oura-hub accept-epoch --collector pi-01 --epoch 'replacement-epoch'
```

This explicit action permits a later starting history. Uploads within the
active epoch may not regress their earliest event. History timestamps use the
existing ring-clock resolver, including its download-time fallback when no UTC
anchor exists. A later projected start is not alone proof of history loss:
the hub accepts it only if the candidate retains every active raw event,
including serial, tag, ring counter, body bytes, and capture provenance.
This permits new clock observations without permitting partial replacements;
a larger event count alone never proves continuity. `device.mac` is ordinary
device metadata, not proven secret key material, and is neither rejected nor
exposed in status.

The proof streams active rows with constant extra row-buffer memory. Canonical
ring-store databases use their unique event index for O(N log M) membership
lookups, for N active and M candidate events. SQLite progress callbacks enforce
the upload deadline even if an uploaded database lacks that index. It checks all
serials, so every serial's earliest raw events survive, even when clock anchors
cannot establish a reliable global ordering. No full-store payload copies are
loaded into memory for this comparison.

`hub-state.json` contains contact/acceptance times, generation, byte count,
demo/production identity, newest measurement time, pending-review flag, and at
most 16 redacted error codes per collector. It contains no event bodies or device
records. Rate counters are process-local and reset on restart. Startup removes
abandoned staging files while holding the data-root lock; immutable orphan
generations are reused on byte-identical retry, never overwritten. A generation
file is committed before its pointer, so a crash cannot publish a missing file.
Keep the entire data root in backups, including active pointers and pending
epoch records. No automatic generation deletion is performed.
