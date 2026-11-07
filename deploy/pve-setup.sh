#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# PVE host setup script — creates a complete unprivileged LXC for the Oura
# dashboard and installs the application inside it.
#
# Run this on the Proxmox VE host as root:
#
#   bash -c "$(curl -fsSL https://raw.githubusercontent.com/ma2skollar/open_health/main/deploy/pve-setup.sh)"
#
# Or copy this file to the PVE host and run it there:
#
#   scp deploy/pve-setup.sh root@pve:/root/
#   ssh root@pve 'bash /root/pve-setup.sh'
#
# Override defaults with environment variables:
#
#   CTID=200 CT_HOSTNAME=oura CONTAINER_STORAGE=local-lvm \
#   TEMPLATE_STORAGE=local TEMPLATE=debian-12-standard_12.7-1_amd64.tar.zst \
#   MEMORY=2048 CORES=2 DISK=8 REPO_URL=https://github.com/ma2skollar/open_health.git \
#   BRANCH=main bash pve-setup.sh
#
# The script is idempotent: re-running it skips an existing container and only
# reinstalls changed binaries. It never deletes data or overwrites existing
# configuration without asking.
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

# ── Defaults ─────────────────────────────────────────────────────────────────
CTID="${CTID:-200}"
CT_HOSTNAME="${CT_HOSTNAME:-oura}"
MEMORY="${MEMORY:-2048}"
SWAP="${SWAP:-512}"
CORES="${CORES:-2}"
DISK="${DISK:-8}"
BRIDGE="${BRIDGE:-vmbr0}"
NET_MODE="${NET_MODE:-dhcp}"
CT_IP="${CT_IP:-}"
CT_GW="${CT_GW:-}"
CONTAINER_STORAGE="${CONTAINER_STORAGE:-local-lvm}"
TEMPLATE_STORAGE="${TEMPLATE_STORAGE:-local}"
TEMPLATE="${TEMPLATE:-debian-12-standard_12.7-1_amd64.tar.zst}"
REPO_URL="${REPO_URL:-https://github.com/Th0rgal/open_health.git}"
BRANCH="${BRANCH:-main}"
CT_USER="${CT_USER:-oura}"
CT_PASSWORD="${CT_PASSWORD:-}"
UNPRIVILEGED="${UNPRIVILEGED:-1}"
APP_PORT="${APP_PORT:-8090}"

# ── Colors ───────────────────────────────────────────────────────────────────
if [ -t 1 ]; then
    RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; CYAN='\033[0;36m'; NC='\033[0m'
else
    RED=''; GREEN=''; YELLOW=''; CYAN=''; NC=''
fi
msg()  { printf '%b%s%b\n' "$CYAN" "$*" "$NC"; }
ok()   { printf '%b✓%b %s\n' "$GREEN" "$NC" "$*"; }
warn() { printf '%b!%b %s\n' "$YELLOW" "$NC" "$*"; }
err()  { printf '%b✗%b %s\n' "$RED" "$NC" "$*" >&2; }

# ── Helpers ──────────────────────────────────────────────────────────────────
pct_cmd() {
    if command -v pct >/dev/null 2>&1; then
        pct "$@"
    else
        err "pct not found — this script must run on a Proxmox VE host"
        exit 1
    fi
}

pveam_cmd() {
    if command -v pveam >/dev/null 2>&1; then
        pveam "$@"
    else
        err "pveam not found — this script must run on a Proxmox VE host"
        exit 1
    fi
}

container_exists() {
    pct status "$CTID" >/dev/null 2>&1
}

container_running() {
    [ "$(pct status "$CTID" 2>/dev/null | awk '{print $2}')" = "running" ]
}

# ── Preflight ────────────────────────────────────────────────────────────────
if [ "$(id -u)" -ne 0 ]; then
    err "this script must run as root on the Proxmox VE host"
    exit 1
fi

msg "Oura LXC setup — CTID=${CTID}, hostname=${CT_HOSTNAME}, template=${TEMPLATE}"

# ── Template ─────────────────────────────────────────────────────────────────
if ! pveam list "$TEMPLATE_STORAGE" 2>/dev/null | grep -q "$TEMPLATE"; then
    msg "downloading template ${TEMPLATE} to ${TEMPLATE_STORAGE}..."
    pveam update
    # Extract the template name from the full path if the user supplied a path
    TEMPLATE_NAME="$(basename "$TEMPLATE")"
    if ! pveam download "$TEMPLATE_STORAGE" "$TEMPLATE_NAME"; then
        err "failed to download template ${TEMPLATE_NAME}"
        err "list available templates with: pveam available | grep debian"
        exit 1
    fi
fi
ok "template present"

# ── Create container ─────────────────────────────────────────────────────────
if container_exists; then
    ok "container ${CTID} already exists — skipping creation"
    if ! container_running; then
        msg "starting container ${CTID}..."
        pct start "$CTID"
    fi
else
    msg "creating container ${CTID}..."
    NET_OPTS="name=eth0,bridge=${BRIDGE}"
    if [ -n "$CT_IP" ]; then
        NET_OPTS="${NET_OPTS},ip=${CT_IP}"
        [ -n "$CT_GW" ] && NET_OPTS="${NET_OPTS},gw=${CT_GW}"
    else
        NET_OPTS="${NET_OPTS},ip=${NET_MODE}"
    fi

    TEMPLATE_PATH="${TEMPLATE_STORAGE}:vztmpl/${TEMPLATE##*/}"
    pct create "$CTID" "$TEMPLATE_PATH" \
        --hostname "$CT_HOSTNAME" \
        --memory "$MEMORY" \
        --swap "$SWAP" \
        --cores "$CORES" \
        --rootfs "${CONTAINER_STORAGE}:${DISK}" \
        --net0 "$NET_OPTS" \
        --unprivileged "$UNPRIVILEGED" \
        --features nesting=1 \
        --onboot 1 \
        --start 1
    ok "container ${CTID} created and started"
fi

# ── Wait for network ─────────────────────────────────────────────────────────
msg "waiting for container network..."
for _ in $(seq 1 30); do
    if pct exec "$CTID" -- getent hosts deb.debian.org >/dev/null 2>&1; then
        ok "network ready"
        break
    fi
    sleep 2
done

# ── Install dependencies inside the container ────────────────────────────────
msg "installing build dependencies inside the container (this takes a few minutes)..."
pct exec "$CTID" -- bash -c '
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq build-essential pkg-config libsqlite3-dev libdbus-1-dev libdbus-1-3 curl git ca-certificates
echo "deps installed"
'

# ── Install Rust toolchain ───────────────────────────────────────────────────
msg "installing Rust toolchain..."
pct exec "$CTID" -- bash -c '
set -euo pipefail
if ! command -v cargo >/dev/null 2>&1; then
    curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    . "$HOME/.cargo/env"
fi
cargo --version
'

# ── Clone repository ─────────────────────────────────────────────────────────
msg "cloning ${REPO_URL} (branch ${BRANCH})..."
pct exec "$CTID" -- bash -c "
set -euo pipefail
if [ ! -d /root/oura/.git ]; then
    git clone --branch '${BRANCH}' '${REPO_URL}' /root/oura
else
    cd /root/oura
    git fetch --all
    git checkout '${BRANCH}'
    git pull --ff-only
fi
"

# ── Build the application ────────────────────────────────────────────────────
msg "building oura-cli and oura-backup (this takes several minutes)..."
pct exec "$CTID" -- bash -c '
set -euo pipefail
. "$HOME/.cargo/env"
cd /root/oura
cargo build --release -p oura-cli -p oura-backup
'

# ── Run the in-container installer ───────────────────────────────────────────
msg "running deploy/install.sh..."
pct exec "$CTID" -- bash -c '
set -euo pipefail
cd /root/oura
./deploy/install.sh --bin ./target/release/oura --backup-bin ./target/release/oura-backup
'

# ── Configure oura.env ───────────────────────────────────────────────────────
msg "configuring /etc/oura/oura.env..."
pct exec "$CTID" -- bash -c "
set -euo pipefail
CONF=/etc/oura/oura.env
if grep -q '__PLACEHOLDER__' \"\$CONF\"; then
    sed -i 's|__RING_DB__|/srv/oura/data/oura.db|g' \"\$CONF\"
    sed -i 's|__MANUAL_DB__|/srv/oura/data/manual.db|g' \"\$CONF\"
    sed -i 's|__TZ_OFFSET__|0|g' \"\$CONF\"
    sed -i 's|__HUB_STATE_DIR__|/srv/oura/data/hub|g' \"\$CONF\"
    echo 'placeholders replaced — review /etc/oura/oura.env'
else
    echo 'oura.env already configured — left untouched'
fi
"

# ── Enable and start the service ─────────────────────────────────────────────
msg "starting oura-dashboard.service..."
pct exec "$CTID" -- bash -c '
set -euo pipefail
systemctl daemon-reload
systemctl enable --now oura-dashboard.service
'

# ── Health check ─────────────────────────────────────────────────────────────
msg "running deploy/health.sh..."
pct exec "$CTID" -- bash -c '
set -euo pipefail
cd /root/oura
./deploy/health.sh || true
'

msg "done — dashboard listens on http://127.0.0.1:${APP_PORT} inside CT ${CTID}"
msg "to reach it from your LAN, use an SSH tunnel or a reverse proxy; see docs/DEPLOY_LXC.md"
