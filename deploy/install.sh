#!/usr/bin/env bash
# Idempotent installer for the Oura dashboard on a Debian/Ubuntu LXC.
#
#   sudo ./deploy/install.sh --bin /path/to/oura
#   sudo ./deploy/install.sh --dry-run
#
# Safe to re-run: every step checks what already exists and reports it instead of
# failing. Nothing is deleted, and no existing configuration file is overwritten.
set -euo pipefail

PREFIX="${PREFIX:-/srv/oura}"
CONF_DIR="${CONF_DIR:-/etc/oura}"
SERVICE_USER="${SERVICE_USER:-oura}"
BIN_SRC="${BIN_SRC:-./target/release/oura}"
BACKUP_BIN_SRC="${BACKUP_BIN_SRC:-}"
DRY_RUN=0

usage() {
    cat <<'EOF'
Usage: install.sh [--bin PATH] [--backup-bin PATH] [--dry-run]

  --bin PATH         dashboard binary to install (default ./target/release/oura)
  --backup-bin PATH  backup tool to install (default: none)
  --dry-run          print what would happen; change nothing
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --bin) BIN_SRC="${2:?--bin needs a path}"; shift 2 ;;
        --backup-bin) BACKUP_BIN_SRC="${2:?--backup-bin needs a path}"; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
done

run() {
    if [ "$DRY_RUN" -eq 1 ]; then
        printf 'DRY-RUN   %s\n' "$*"
    else
        "$@"
    fi
}

say() { printf '%s\n' "$*"; }
have() { command -v "$1" >/dev/null 2>&1; }

# ─────────────────────────────────────────────────────────────────────────────
# PRIVILEGED STEPS. Everything that needs root is in this section and nowhere
# else: creating a system user, writing under /srv and /etc, installing units,
# and talking to systemd.
# ─────────────────────────────────────────────────────────────────────────────
if [ "$(id -u)" -ne 0 ]; then
    say "error: install.sh must run as root — it installs a system user, files under ${PREFIX} and ${CONF_DIR}, and systemd units." >&2
    exit 1
fi

if ! getent group "$SERVICE_USER" >/dev/null 2>&1; then
    run groupadd --system "$SERVICE_USER"
else
    say "group ${SERVICE_USER} already present"
fi

if ! id -u "$SERVICE_USER" >/dev/null 2>&1; then
    run useradd --system --gid "$SERVICE_USER" --home-dir "$PREFIX" --shell /usr/sbin/nologin "$SERVICE_USER"
else
    say "user ${SERVICE_USER} already present"
fi

for dir in "$PREFIX" "$PREFIX/bin" "$PREFIX/data" "$PREFIX/backups" "$CONF_DIR"; do
    if [ -d "$dir" ]; then
        say "directory ${dir} already present"
    else
        run mkdir -p "$dir"
    fi
done
# $PREFIX is the unit's WorkingDirectory, so the service user has to be able to
# traverse it. Mode 0750 alone left it root:root, and systemd then failed with
# "Changing to the requested working directory failed: Permission denied"
# (status=200/CHDIR). The binaries under $PREFIX/bin stay root-owned 0755.
run chown "$SERVICE_USER:$SERVICE_USER" "$PREFIX" "$PREFIX/data" "$PREFIX/backups"
run chmod 0750 "$PREFIX" "$PREFIX/data" "$PREFIX/backups"
run chmod 0755 "$PREFIX/bin"
# The group has to match oura.env's (root:$SERVICE_USER, 0640) or the file is
# unreadable through its own directory.
run chown "root:$SERVICE_USER" "$CONF_DIR"
run chmod 0750 "$CONF_DIR"

if [ ! -f "$BIN_SRC" ]; then
    say "error: binary not found at ${BIN_SRC} (build it first, or pass --bin)" >&2
    exit 1
fi
if [ "${DRY_RUN}" -eq 0 ] && cmp -s "$BIN_SRC" "$PREFIX/bin/oura"; then
    say "binary unchanged: ${PREFIX}/bin/oura"
else
    run install -m 0755 -o root -g root "$BIN_SRC" "$PREFIX/bin/oura"
fi

if [ -n "$BACKUP_BIN_SRC" ]; then
    if [ ! -f "$BACKUP_BIN_SRC" ]; then
        say "error: backup binary not found at ${BACKUP_BIN_SRC}" >&2
        exit 1
    fi
    if [ "${DRY_RUN}" -eq 0 ] && cmp -s "$BACKUP_BIN_SRC" "$PREFIX/bin/oura-backup"; then
        say "binary unchanged: ${PREFIX}/bin/oura-backup"
    else
        run install -m 0755 -o root -g root "$BACKUP_BIN_SRC" "$PREFIX/bin/oura-backup"
    fi
fi

UNIT_SRC="$(cd "$(dirname "$0")" && pwd)/systemd"
for unit in oura-dashboard.service oura-backup.service oura-backup.timer; do
    if [ "${DRY_RUN}" -eq 0 ] && cmp -s "$UNIT_SRC/$unit" "/etc/systemd/system/$unit"; then
        say "unit unchanged: /etc/systemd/system/${unit}"
    else
        run install -m 0644 -o root -g root "$UNIT_SRC/$unit" "/etc/systemd/system/$unit"
    fi
done

if [ -e "$CONF_DIR/oura.env" ]; then
    say "config ${CONF_DIR}/oura.env already present — left untouched; merge new keys by hand"
else
    run install -m 0640 -o root -g "$SERVICE_USER" \
        "$(cd "$(dirname "$0")" && pwd)/oura.env.example" "$CONF_DIR/oura.env"
    if [ "${DRY_RUN}" -eq 1 ]; then
        say "DRY-RUN   would write ${CONF_DIR}/oura.env from the example"
    else
        say "wrote ${CONF_DIR}/oura.env from the example — replace every __PLACEHOLDER__ before starting"
    fi
fi

if have systemctl && [ "${DRY_RUN}" -eq 0 ]; then
    run systemctl daemon-reload
    run systemctl enable --now oura-dashboard.service
    run systemctl enable --now oura-backup.timer
elif [ "${DRY_RUN}" -eq 1 ]; then
    say "DRY-RUN   systemctl daemon-reload && systemctl enable --now oura-dashboard.service oura-backup.timer"
else
    say "warning: systemctl not available — units installed but not enabled"
fi

say "done"
