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
# On a terminal it opens the same whiptail dialog the community LXC scripts use:
# a Default/Advanced menu, an advanced wizard (container ID, hostname, template,
# storages, resources, network) with Back navigation, and a summary you confirm
# before anything on the host is touched. Create the container with, say, ID 222
# through Advanced Install; the default is only a starting value.
#
# The dialog is skipped (defaults and env vars apply) when stdin is not a TTY,
# when whiptail is missing, or when NONINTERACTIVE=1. Pass MODE=default or
# MODE=advanced to skip the menu and go straight into that flow.
#
# Override defaults with environment variables:
#
#   CTID=200 CT_HOSTNAME=oura CONTAINER_STORAGE=local-lvm \
#   TEMPLATE_STORAGE=local TEMPLATE=debian-12-standard \
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
TEMPLATE="${TEMPLATE:-debian-12-standard}"
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

# ── Interactive UI (whiptail) ────────────────────────────────────────────────
# Same flow as the community-scripts LXC dialog: a Default/Advanced menu, an
# advanced wizard with Back navigation, and a summary you confirm before
# anything on the host is touched. Skipped entirely when stdin is not a TTY,
# when whiptail is missing, or when NONINTERACTIVE=1. MODE=default|advanced
# jumps straight into that flow (and is how you script it unattended).
WHIPTAIL_BACKTITLE="Oura Dashboard LXC"

ui_tty() {
    [ "${NONINTERACTIVE:-0}" != "1" ] &&
        [ -t 0 ] && [ -t 1 ] &&
        [ "${TERM:-}" != "dumb" ]
}

# whiptail ships with Proxmox VE; install it if a fresh host lacks it, since the
# dialog is the whole point of running this on a terminal.
ensure_whiptail() {
    command -v whiptail >/dev/null 2>&1 && return 0
    msg "installing whiptail (needed for the setup dialog)..."
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq whiptail >/dev/null 2>&1 || true
    command -v whiptail >/dev/null 2>&1
}

ui_available() {
    ui_tty || return 1
    ensure_whiptail
}

ui_input() { # ui_input <title> <prompt> <default>
    whiptail --backtitle "$WHIPTAIL_BACKTITLE" --title "$1" \
        --ok-button "Next" --cancel-button "Back" \
        --inputbox "$2" 13 70 "$3" 3>&1 1>&2 2>&3
}

ui_menu() { # ui_menu <title> <prompt> <height> <width> <list-height> <default-item> <tag item>...
    local title="$1" prompt="$2" h="$3" w="$4" lh="$5" def="$6"
    shift 6
    whiptail --backtitle "$WHIPTAIL_BACKTITLE" --title "$title" \
        --ok-button "Select" --cancel-button "Back" --notags \
        --menu "$prompt" "$h" "$w" "$lh" "$@" --default-item "$def" 3>&1 1>&2 2>&3
}

ui_confirm() { # ui_confirm <title> <text> [no]   (default button: yes, or no)
    if [ "${3:-}" = "no" ]; then
        whiptail --backtitle "$WHIPTAIL_BACKTITLE" --title "$1" --defaultno --yesno "$2" 22 78
    else
        whiptail --backtitle "$WHIPTAIL_BACKTITLE" --title "$1" --yesno "$2" 22 78
    fi
}

ui_note() { whiptail --backtitle "$WHIPTAIL_BACKTITLE" --title "$1" --msgbox "$2" 13 70; }

ui_ask() { # ui_ask <title> <prompt> <default> <validator-cmd...> — re-prompts until valid
    local title="$1" prompt="$2" default="$3" out
    shift 3
    while :; do
        out="$(ui_input "$title" "$prompt" "$default")" || return 1
        out="${out:-$default}"
        if "$@" "$out"; then printf '%s' "$out"; return 0; fi
        ui_note "$title" "Invalid value: '${out}'. Try again."
    done
}

# Storages usable for a given content type, active only.
list_storage() {
    pvesm status -content "$1" 2>/dev/null | awk 'NR>1 && $3=="active" {print $1}'
}

valid_ctid()     { [[ "$1" =~ ^[1-9][0-9]{2,8}$ ]]; }
valid_hostname() { [[ "$1" =~ ^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)*$ ]]; }
valid_positive() { [[ "$1" =~ ^[1-9][0-9]*$ ]]; }
valid_cidr()     { [[ "$1" =~ ^([0-9]{1,3}\.){3}[0-9]{1,3}/[0-9]{1,2}$ ]]; }
valid_ipv4()     { [[ "$1" =~ ^([0-9]{1,3}\.){3}[0-9]{1,3}$ ]]; }
valid_ifname()   { [[ "$1" =~ ^[a-zA-Z0-9._-]+$ ]]; }

ctid_in_use() {
    pct status "$1" >/dev/null 2>&1 && return 0
    command -v qm >/dev/null 2>&1 && qm status "$1" >/dev/null 2>&1 && return 0
    return 1
}

# advanced_settings — step wizard with Back navigation, like community-scripts.
advanced_settings() {
    local step=1
    local id host disk cores ram swap bridge net x
    while [ "$step" -le 10 ]; do
        case "$step" in
        1) # Container ID
            if ! id="$(ui_input "CONTAINER ID" \
                "\nSet CONTAINER ID\n\nThis is the Proxmox container to create, or to reuse if it already exists." \
                "$CTID")"; then
                return 1
            fi
            id="${id:-$CTID}"
            if ! valid_ctid "$id"; then
                ui_note "Invalid ID" "Container ID must be a number from 100 up, without leading zeros."
                continue
            fi
            if ctid_in_use "$id" &&
                ! whiptail --backtitle "$WHIPTAIL_BACKTITLE" --title "ID Already In Use" --defaultno \
                    --yesno "Container/VM ID ${id} already exists.\n\nUse it and (re)install the Oura dashboard inside it?\n\nChoose No to enter a different ID." 13 70; then
                continue
            fi
            CTID="$id"; step=$((step + 1))
            ;;
        2) # Hostname
            if ! host="$(ui_ask "HOSTNAME" \
                "\nSet the container hostname\n(lowercase letters, digits, dots and hyphens)" \
                "$CT_HOSTNAME" valid_hostname)"; then
                step=$((step - 1))
                continue
            fi
            CT_HOSTNAME="$host"; step=$((step + 1))
            ;;
        3) # Template family
            local -a tt=()
            while IFS= read -r x; do
                [ -n "$x" ] || continue
                tt+=("$x" "$x")
            done < <(pveam available --section system 2>/dev/null | awk '{print $2}' |
                grep -E '^(debian|ubuntu)-[0-9.]+-standard_amd64\.tar\.zst$' |
                sed -E 's/_[0-9][0-9.]*-[0-9]+_amd64\.tar\.zst$//' | sort -uVr)
            if [ "${#tt[@]}" -eq 0 ]; then
                if x="$(ui_input "TEMPLATE" \
                    "\nNo template catalog found; type a base name (e.g. debian-12-standard)." "$TEMPLATE")"; then
                    TEMPLATE="${x:-$TEMPLATE}"; step=$((step + 1))
                else
                    step=$((step - 1))
                fi
                continue
            fi
            if x="$(ui_menu "TEMPLATE" \
                "\nChoose the container OS template.\nPoint releases are resolved to the newest available." \
                20 70 "$(( ${#tt[@]} / 2 > 12 ? 12 : ${#tt[@]} / 2 ))" "$TEMPLATE" "${tt[@]}")"; then
                TEMPLATE="$x"; step=$((step + 1))
            else
                step=$((step - 1))
            fi
            ;;
        4) # Container storage (rootfs)
            local -a st=()
            while IFS= read -r x; do [ -n "$x" ] && st+=("$x" "$x"); done < <(list_storage rootdir)
            if [ "${#st[@]}" -eq 0 ]; then
                if x="$(ui_input "CONTAINER STORAGE" \
                    "\nNo active storage with 'rootdir' content found.\n\nSet the container storage name." \
                    "$CONTAINER_STORAGE")"; then
                    CONTAINER_STORAGE="${x:-$CONTAINER_STORAGE}"; step=$((step + 1))
                else
                    step=$((step - 1))
                fi
                continue
            fi
            if x="$(ui_menu "CONTAINER STORAGE" \
                "\nChoose the storage for the container root filesystem." \
                20 70 "$(( ${#st[@]} / 2 > 10 ? 10 : ${#st[@]} / 2 ))" "$CONTAINER_STORAGE" "${st[@]}")"; then
                CONTAINER_STORAGE="$x"; step=$((step + 1))
            else
                step=$((step - 1))
            fi
            ;;
        5) # Template storage
            local -a ts=()
            while IFS= read -r x; do [ -n "$x" ] && ts+=("$x" "$x"); done < <(list_storage vztmpl)
            if [ "${#ts[@]}" -eq 0 ]; then
                if x="$(ui_input "TEMPLATE STORAGE" \
                    "\nNo active storage with 'vztmpl' content found.\n\nSet the template storage name." \
                    "$TEMPLATE_STORAGE")"; then
                    TEMPLATE_STORAGE="${x:-$TEMPLATE_STORAGE}"; step=$((step + 1))
                else
                    step=$((step - 1))
                fi
                continue
            fi
            if x="$(ui_menu "TEMPLATE STORAGE" \
                "\nChoose the storage for the downloaded template." \
                20 70 "$(( ${#ts[@]} / 2 > 10 ? 10 : ${#ts[@]} / 2 ))" "$TEMPLATE_STORAGE" "${ts[@]}")"; then
                TEMPLATE_STORAGE="$x"; step=$((step + 1))
            else
                step=$((step - 1))
            fi
            ;;
        6) # Disk
            if ! disk="$(ui_ask "DISK SIZE" "\nSet the container disk size in GiB." "$DISK" valid_positive)"; then
                step=$((step - 1)); continue
            fi
            DISK="$disk"; step=$((step + 1))
            ;;
        7) # Cores
            if ! cores="$(ui_ask "CPU CORES" "\nAllocate CPU cores." "$CORES" valid_positive)"; then
                step=$((step - 1)); continue
            fi
            CORES="$cores"; step=$((step + 1))
            ;;
        8) # RAM
            if ! ram="$(ui_ask "MEMORY" "\nAllocate RAM in MiB." "$MEMORY" valid_positive)"; then
                step=$((step - 1)); continue
            fi
            MEMORY="$ram"
            if ! swap="$(ui_ask "SWAP" "\nAllocate swap in MiB." "$SWAP" valid_positive)"; then
                step=$((step - 1)); continue
            fi
            SWAP="$swap"; step=$((step + 1))
            ;;
        9) # Network
            if ! net="$(ui_menu "NETWORK" \
                "\nChoose the network configuration for eth0." 13 70 2 \
                "$([ -n "$CT_IP" ] && echo static || echo dhcp)" \
                "dhcp" "Address assigned by your LAN (recommended)" \
                "static" "You supply a static IPv4 CIDR address and gateway")"; then
                step=$((step - 1)); continue
            fi
            if [ "$net" = "static" ]; then
                NET_MODE="static"
                if ! CT_IP="$(ui_ask "STATIC IPV4" \
                    "\nEnter the static IPv4 CIDR address\n(e.g. [IP_ADDRESS]/24)." "${CT_IP}" valid_cidr)"; then
                    step=$((step - 1)); continue
                fi
                if ! CT_GW="$(ui_ask "GATEWAY" \
                    "\nEnter the gateway IP address." "${CT_GW}" valid_ipv4)"; then
                    step=$((step - 1)); continue
                fi
            else
                NET_MODE="dhcp"; CT_IP=""; CT_GW=""
            fi
            step=$((step + 1))
            ;;
        10) # Bridge
            if ! bridge="$(ui_ask "BRIDGE" "\nSet the network bridge (e.g. vmbr0)." "$BRIDGE" valid_ifname)"; then
                step=$((step - 1)); continue
            fi
            BRIDGE="$bridge"; step=$((step + 1))
            ;;
        esac
    done
    return 0
}

show_summary() {
    local net action
    if [ -n "$CT_IP" ]; then
        net="${BRIDGE}, static ${CT_IP}${CT_GW:+ via ${CT_GW}}"
    else
        net="${BRIDGE}, dhcp"
    fi
    if container_exists; then
        action="Container ${CTID} already exists — the dashboard will be (re)installed inside it."
    else
        action="Container ${CTID} will be created and started."
    fi
    printf '%s' "About to set up the Oura dashboard LXC.

  Container ID : ${CTID}
  Hostname     : ${CT_HOSTNAME}
  Template     : ${TEMPLATE} (newest point release)
  Resources    : ${CORES} vCPU, ${MEMORY} MiB RAM, ${SWAP} MiB swap, ${DISK} GiB disk
  Storage      : ${CONTAINER_STORAGE} (rootfs), ${TEMPLATE_STORAGE} (templates)
  Network      : ${net}

${action}

Inside the container this installs build dependencies, the Rust toolchain,
clones ${REPO_URL} (branch ${BRANCH}), builds it, and enables the service.

Nothing has been changed yet. Proceed?"
}

# ── Preflight ────────────────────────────────────────────────────────────────
if [ "$(id -u)" -ne 0 ]; then
    err "this script must run as root on the Proxmox VE host"
    exit 1
fi

if ui_available; then
    case "${MODE:-}" in
    "") # Show the menu, like the community scripts.
        MODE="$(ui_menu "OURA DASHBOARD LXC" \
            "\nChoose an option:\n Use TAB or Arrow keys to navigate, ENTER to select.\n" \
            20 74 3 "1" \
            "1" "Default Install — CTID ${CTID}, ${CORES} vCPU, ${MEMORY} MiB RAM, ${DISK} GiB, DHCP" \
            "2" "Advanced Install — choose container ID, hostname, resources, storage, network" \
            "3" "Exit")" || { msg "cancelled"; exit 0; }
        ;;
    esac
    case "$MODE" in
    2 | advanced | ADVANCED)
        MODE=advanced
        advanced_settings || { err "cancelled during advanced settings"; exit 1; }
        ;;
    3 | exit | EXIT)
        msg "cancelled"
        exit 0
        ;;
    *)
        MODE=default
        ;;
    esac
    # Reusing an existing container defaults the confirm button to No.
    DEFNO=yes
    if container_exists; then DEFNO=no; fi
    ui_confirm "CONFIRM" "$(show_summary)" "$DEFNO" || {
        err "aborted by user — nothing was changed"
        exit 0
    }
fi

msg "Oura LXC setup — CTID=${CTID}, hostname=${CT_HOSTNAME}, template=${TEMPLATE}"

# ── Template ─────────────────────────────────────────────────────────────────
# Resolve the template name to the newest matching image present locally or in
# the PVE index. Accepts a full filename or a base name such as
# "debian-12-standard" (the default). Point releases move with every PVE
# refresh, so a hardcoded name goes stale and breaks pveam download.
resolve_template() {
    local base="$1" storage="$2" found=""
    case "$base" in
        *_amd64.tar.zst|*_amd64.tar.gz) basename "$base"; return 0 ;;
    esac
    found="$(pveam list "$storage" 2>/dev/null | grep -oE "${base}[0-9._-]+_amd64\\.tar\\.(zst|gz)" | sort -V | tail -n1)"
    if [ -n "$found" ]; then
        echo "$found"
        return 0
    fi
    found="$(pveam available --section system 2>/dev/null | grep -oE "${base}[0-9._-]+_amd64\\.tar\\.(zst|gz)" | sort -V | tail -n1)"
    [ -n "$found" ] && { echo "$found"; return 0; }
    return 1
}

TEMPLATE_NAME="$(resolve_template "$TEMPLATE" "$TEMPLATE_STORAGE" || true)"

if [ -z "$TEMPLATE_NAME" ] || ! pveam list "$TEMPLATE_STORAGE" 2>/dev/null | grep -q "$TEMPLATE_NAME"; then
    msg "downloading template ${TEMPLATE} to ${TEMPLATE_STORAGE}..."
    pveam update
    TEMPLATE_NAME="$(resolve_template "$TEMPLATE" "$TEMPLATE_STORAGE")" || {
        err "no ${TEMPLATE} template found in storage '${TEMPLATE_STORAGE}' or the PVE index"
        err "list available templates with: pveam available --section system | grep debian"
        exit 1
    }
    if ! pveam download "$TEMPLATE_STORAGE" "$TEMPLATE_NAME"; then
        err "failed to download template ${TEMPLATE_NAME}"
        err "list available templates with: pveam available --section system | grep debian"
        exit 1
    fi
fi
TEMPLATE="$TEMPLATE_NAME"
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
