#!/usr/bin/env bash
# Pi-only package installation. Dry-run is the default; never provisions a ring.
set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
BINARY="$HERE/../../target/aarch64-unknown-linux-musl/release/oura-collector"
APPLY=0
usage() {
  cat <<'EOF'
Usage: deploy/pi/install.sh [--dry-run | --apply] [--binary PATH]
Default: print installation commands without changing the host.
--apply installs files/user only on an ARM64 Linux host as root.
It never reads/copies keys, pairs a ring, changes networking, or starts a service.
Review docs/pi-collector.md and obtain deployment approval before --apply.
EOF
}
while (($#)); do
  case "$1" in
    --dry-run) APPLY=0; shift ;;
    --apply) APPLY=1; shift ;;
    --binary)
      [[ $# -ge 2 && -n "$2" ]] || { echo 'Missing --binary path' >&2; exit 2; }
      BINARY="$2"; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) printf 'Unknown option: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done
run() {
  printf '+'
  printf ' %q' "$@"
  printf '\n'
  if ((APPLY)); then "$@"; fi
}
if ((APPLY)); then
  [[ $(id -u) == 0 ]] || { echo '--apply requires root' >&2; exit 1; }
  [[ $(uname -s) == Linux && $(uname -m) == aarch64 ]] || {
    echo '--apply requires ARM64 Linux; no host changes made' >&2; exit 1;
  }
  command -v python3 >/dev/null || { echo 'python3 required for ELF validation' >&2; exit 1; }
  python3 - "$BINARY" <<'PY'
import pathlib, struct, sys
p = pathlib.Path(sys.argv[1])
if p.is_symlink() or not p.is_file():
    raise SystemExit('Binary must be a regular non-symlink file')
with p.open('rb') as f:
    header = f.read(20)
if len(header) != 20 or header[:6] != b'\x7fELF\x02\x01' or struct.unpack('<H', header[18:20])[0] != 183:
    raise SystemExit('Binary must be little-endian ELF64 AArch64')
PY
fi
if (( ! APPLY )); then
  echo 'DRY RUN: no commands below execute; no keys or ring are accessed.'
  [[ -f "$BINARY" ]] || echo 'Artifact absent: --apply will fail until a real ARM64 binary is supplied.'
fi
if ! getent group bluetooth >/dev/null; then
  echo 'BlueZ bluetooth group missing: install/configure BlueZ manually before applying.' >&2
  if ((APPLY)); then exit 1; fi
fi
if ! id oura-collector >/dev/null 2>&1; then
  run useradd --system --user-group --home-dir /var/lib/oura-collector --shell /usr/sbin/nologin oura-collector
fi
run usermod --append --groups bluetooth oura-collector
run install -d -m 0750 -o root -g oura-collector /etc/oura-collector
run install -d -m 0700 -o oura-collector -g oura-collector /var/lib/oura-collector
run install -m 0755 -o root -g root "$BINARY" /usr/local/bin/oura-collector
if [[ -e /etc/oura-collector/collector.toml || -L /etc/oura-collector/collector.toml ]]; then
  echo 'Preserving existing /etc/oura-collector/collector.toml; review it manually.'
else
  run install -m 0640 -o root -g oura-collector "$HERE/collector.toml.example" /etc/oura-collector/collector.toml
fi
run install -m 0644 -o root -g root "$HERE/oura-collector.service" /etc/systemd/system/oura-collector.service
run install -d -m 0755 -o root -g root /usr/share/doc/oura-collector
run install -m 0644 -o root -g root "$HERE/../../docs/pi-collector.md" /usr/share/doc/oura-collector/pi-collector.md
run systemctl daemon-reload
echo 'No service was started, stopped, enabled, or disabled. Existing service state is unchanged. No keys were read or installed.'
echo 'Review config, provision approved service-owned 0600 keys, then follow the manual start gate in the runbook.'
