# LXC Deployment Guide — the authoritative dashboard host

Target: one unprivileged Proxmox VE LXC running the dashboard and the long-term
stores. The Raspberry Pi is not involved yet. Everything here runs without a ring,
a Pi, or a Bluetooth adapter.

## 0. What this host owns

| Store | Path | Owner |
|---|---|---|
| Ring database (read-only for the dashboard) | `/srv/oura/data/oura.db` | `oura:oura` |
| Manual calorie/weight database | `/srv/oura/data/manual.db` | `oura:oura` |
| Backups | `/srv/oura/backups/` | `oura:oura` |
| Binary | `/srv/oura/bin/oura` | root, `0755` |
| Config | `/etc/oura/oura.env` | `root:oura`, `0640` |
| Units | `/etc/systemd/system/oura-*.service` | root |

The dashboard binds **loopback only**. Reaching it from an iPhone or a desktop
requires one of the access paths in step 6 — do not expose the port to the
internet.

## 1. Create the LXC

### Automated (recommended)

The installer is a [community-scripts](https://github.com/community-scripts/ProxmoxVE)
style script, so the setup dialog is the one Proxmox users already know:
**Default Install**, or **Advanced Install** to choose the container ID (for
example `222`), hostname, template, storage and resources. On the host:

```sh
bash -c "$(curl -fsSL https://raw.githubusercontent.com/ma2skollar/open_health/main/ct/oura.sh)"
```

`deploy/pve-setup.sh` is a thin wrapper around the same script, kept for older
links.

The engine creates the container and runs `install/openhealth-install.sh` inside
it (the engine derives that name from `APP`): build
dependencies, the Rust toolchain, a clone of this repository, `cargo build` for
`oura-cli` and `oura-backup`, then `deploy/install.sh` and the systemd units.

Every choice made in the dialog is applied inside the container: root password or
automatic login, console mode, timezone, SSH keys and the MOTD are handled by the
engine's own `setting_up_container` / `motd_ssh` / `customize` steps, which
`install/openhealth-install.sh` runs. `OURA_TZ_OFFSET` follows the container
timezone rather than a fixed value.

### Resources

| Resource | Value | Why |
|---|---|---|
| vCPU | 2 | Parallel `rustc` during the build; idle afterwards |
| RAM | 4 GiB | The build is the peak (rustup + release build with LTO) |
| Disk | 8 GiB | Toolchain plus build artefacts; 8 GiB completed a full build |

2 GiB RAM also completes a build but leaves no headroom, and it is the minimum
quoted below. The dashboard itself is idle at runtime. Override with `var_cpu`,
`var_ram` or `var_disk`; `var_version` and `CTID` work the same way. The dialog is
skipped when stdin is not a terminal.

### Manual

On the Proxmox host, create an **unprivileged** container from a Debian or Ubuntu
template. Suggested minimum: 2 vCPU, 2 GiB RAM, 8 GiB disk. Give it a static LAN
address or a DHCP reservation so its address is stable.

No Bluetooth passthrough, no privileged container, no Docker nesting.

## 2. Install build and runtime dependencies

Inside the container as root:

```sh
apt-get update
apt-get install -y build-essential pkg-config libsqlite3-dev libdbus-1-dev \
  libdbus-1-3 curl git
```

The Rust toolchain:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
```

Build downloads and the Rust toolchain are the only internet access needed. At
runtime nothing here reaches the internet: no telemetry, no CDN, no third-party
health API.

## 3. Build the binaries

Clone the repository at a recorded commit and build:

```sh
cargo build --release -p oura-cli -p oura-backup
```

This produces `target/release/oura` and `target/release/oura-backup`.

## 4. Install

```sh
sudo ./deploy/install.sh --bin ./target/release/oura --backup-bin ./target/release/oura-backup
```

The installer is idempotent. It creates the `oura` system user and group, the
`/srv/oura` tree, installs the binary and units, copies `oura.env.example` to
`/etc/oura/oura.env` only if absent, and enables the units. Re-running reports
what already exists and never overwrites your configuration.

`--dry-run` prints every action without changing anything. Run it first.

## 5. Configure

Edit `/etc/oura/oura.env` and replace every `__PLACEHOLDER__`:

| Variable | Meaning |
|---|---|
| `OURA_RING_DB` | ring database path, `/srv/oura/data/oura.db` |
| `OURA_MANUAL_DB` | manual database path, `/srv/oura/data/manual.db` |
| `OURA_PORT` | loopback port, default `8090` |
| `OURA_BIND` | listen address; unset keeps loopback, set it to the LAN address (step 6, option B) |
| `OURA_TZ_OFFSET` | your UTC offset in hours, e.g. `1` or `5.5` |
| `OURA_HUB_STATE_DIR` | hub state directory, `/srv/oura/data/hub` |

No secret belongs in this file. The ring authentication key lives only on the
future collector, in its own `0600` file.

The manual database is authoritative for calories and weight. Ring ingestion
never replaces it.

## 6. Make the dashboard reachable from an iPhone

The dashboard binds `127.0.0.1` on purpose. Pick one access path. Do **not**
publish the port directly to the internet.

The installer prints the tunnel command for the container it built. To check the
dashboard from the container itself:

```sh
pct exec <ctid> -- /root/oura/deploy/health.sh
```

The loopback listener cannot be reached through the LXC LAN address directly.
The Host guard rejects unrecognized authorities. A reverse proxy must rewrite
the upstream Host to `localhost:8090`, and the dashboard must explicitly allow
the browser's external origin through `OURA_PUBLIC_ORIGIN` (option C). Rewriting
Host alone does not authorize a browser's HTTPS Origin on writes.

### Option A — SSH tunnel (fastest, no extra service)

On the iPhone, use an SSH client (for example Termius) to forward a local port to
the LXC's loopback port:

```sh
ssh -N -L 8090:127.0.0.1:8090 root@<lxc-lan-ip>
```

Then open `http://[IP_ADDRESS]:8090` on the phone. Nothing listens on the LAN
address, and the browser talks to `[IP_ADDRESS]:8090` — the only `Host` the
dashboard accepts. Log in as `root`: the `oura` service account has a nologin
shell.

### Option B — serve the LAN (simplest, least isolation)

The installer asks. After the setup wizard it offers:

```
  Loopback    inside the container only; reach it over an SSH tunnel.
  On the LAN  this container's interfaces; open http://<container-ip>:8090.
```

Answering "On the LAN" writes `OURA_BIND` into `/etc/oura/oura.env`, so
`http://<container-ip>:8090` opens directly from any machine that can reach the
LXC. Choosing loopback — or cancelling — leaves `OURA_BIND` unset and the
dashboard on loopback.

To change it afterwards, edit the value and restart:

```sh
sudoedit /etc/oura/oura.env     # OURA_BIND=0.0.0.0, or a specific address
sudo systemctl restart oura-dashboard.service
```

`OURA_BIND` takes any address the dashboard should listen on. Leave it unset or
empty for loopback; an address that is not loopback stops answering on loopback,
which `health.sh` accounts for.

This drops the isolation the loopback bind provides, so restrict port 8090 to your
LAN or VPN subnet with the LXC firewall, and never forward it from the internet.
The `Host` guard still applies: it accepts only the address the request actually
arrived on, so a name pointed at the LXC (`Host: oura.lan`) is refused with `403`
unless you reach it through a proxy that rewrites `Host` to that address and port.

### Option C — reverse proxy on the LXC (LAN/VPN only)

Run a TLS proxy on the LXC, bound to its LAN address and restricted to your LAN
or VPN subnet. The dashboard has no authentication; do not publish it openly.
Configure the exact browser origin, with scheme and non-default port if used:

```sh
# /etc/oura/oura.env; restart oura-dashboard.service after editing
OURA_PUBLIC_ORIGIN=https://health.example.com
# Direct CLI equivalent:
oura dashboard --public-origin https://health.example.com
```

Example nginx server (replace hostname and certificate paths):

```nginx
server {
    listen 443 ssl;
    server_name health.example.com;
    ssl_certificate /etc/nginx/tls/health.crt;
    ssl_certificate_key /etc/nginx/tls/health.key;
    location / {
        proxy_pass http://localhost:8090;
        proxy_set_header Host localhost:8090;
        proxy_set_header X-Forwarded-Proto https;
        proxy_set_header X-Forwarded-Host $http_host;
    }
}
```

Forwarded headers do not independently grant access. Keep `X-Oura-Dash` and
`Origin` intact; the dashboard validates the configured public origin. Test a
real save through the HTTPS URL, not only `GET /`. Also test a foreign Origin
and bad Host, which must remain rejected. Rebuild the binary after changing web
assets so deployments outside the checkout use the fresh embedded copy.

Before iPhone rollout, check Safari with its software keyboard open: date field
width, modal scrolling, landscape height, both decimal separators, and independent
bottom disclosures. Chromium viewport checks cannot certify native Safari UI.

### Option D — VPN into the LAN

Reach the LXC over your existing VPN and use option A's tunnel, or set `OURA_BIND`
to the VPN interface's address. Alone, a VPN changes who can connect, not what the
dashboard listens on.

## 7. Start and verify

```sh
sudo systemctl start oura-dashboard.service
./deploy/health.sh
```

`health.sh` checks the service state, that the dashboard answers on its loopback
port, free disk against a floor, and that the manual database opens read-only and
passes `quick_check`. Exit 0 means healthy. Output is redacted; no token, key, or
health payload is printed.

## 8. Import the historical PulsHealth export (one time)

The export is a long-format CSV: `timestamp,metric,value,unit,source_bundle_id`
with three metrics — `HKQuantityTypeIdentifierStepCount`,
`HKQuantityTypeIdentifierDietaryEnergyConsumed` (kcal) and
`HKQuantityTypeIdentifierBodyMass` (kg).

Always dry-run first. It changes nothing:

```sh
/srv/oura/bin/oura --db /srv/oura/data/oura.db import \
  pulshealth-export-since-2026-09-14.csv --dry-run
```

Then apply:

```sh
/srv/oura/bin/oura --db /srv/oura/data/oura.db import \
  pulshealth-export-since-2026-09-14.csv
```

The adapter sums per-sample steps into one daily total, keeps the day's last
calorie and weight sample, and records the whole run as one import batch so it can
be rolled back without touching later manual edits. Re-running the same file is a
no-op. Ring steps outrank imported steps in precedence, so a later ring day is
never overwritten by an imported day.

Verify the result in the dashboard's manual panel and charts. Manual entry from
here on replaces the export as the source of truth for calories and weight.

## 9. Backups

```sh
oura-backup create --dest /srv/oura/backups --db /srv/oura/data/oura.db --manual-db /srv/oura/data/manual.db
oura-backup list   --dest /srv/oura/backups
oura-backup verify --dir  /srv/oura/backups/<timestamp>-scheduled
```

Copies use the SQLite online backup API, so a live database with an uncommitted
WAL is captured correctly. `oura-backup.timer` runs `create` daily with a random
delay. A Proxmox VE snapshot alone is **not** a backup; see `docs/OPERATIONS.md`.

## 10. Upgrade and rollback

Upgrade with the engine's entrypoint, which pulls the tracked branch, rebuilds
and reinstalls the units and the service:

```sh
update                       # inside the container
pct exec <ctid> -- update    # from the Proxmox host
```

By hand: `git pull`, `cargo build --release -p oura-cli -p oura-backup`,
`deploy/install.sh --bin ./target/release/oura --backup-bin
./target/release/oura-backup`, `systemctl restart oura-dashboard.service`, then
re-run `health.sh`. Migrations are forward-only.

Rollback: stop the service, restore the previous binary and the databases from a
verified backup, then restart. Restore before restart, never after.
