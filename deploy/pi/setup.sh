#!/usr/bin/env bash
# One-shot Pi install: verify artifact checksum, then hand off to install.sh.
# Dry-run is the default. --apply requires root on ARM64 Linux and deployment
# approval. No keys are read, no ring is touched, no service is started.
set -euo pipefail
HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
BINARY="$HERE/../../target/aarch64-unknown-linux-musl/release/oura-collector"
MODE=--dry-run
SHA256=""
usage() {
  cat <<'EOF'
Usage: setup.sh [--dry-run | --apply] [--binary PATH] [--sha256 HASH]
Default: dry-run. --sha256 verifies the artifact before install.
EOF
}
while (($#)); do
  case "$1" in
    --dry-run) MODE=--dry-run; shift ;;
    --apply) MODE=--apply; shift ;;
    --binary) [[ -n "${2:-}" ]] || { echo 'Missing --binary path' >&2; exit 2; }; BINARY=$2; shift 2 ;;
    --sha256) [[ -n "${2:-}" ]] || { echo 'Missing --sha256 value' >&2; exit 2; }; SHA256=$2; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) printf 'Unknown option: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done
[[ -f "$BINARY" && ! -L "$BINARY" ]] || { echo "Artifact missing or symlink: $BINARY" >&2; exit 1; }
actual="$(sha256sum "$BINARY" | cut -d' ' -f1)"
if [[ -n "$SHA256" ]]; then
  [[ "$actual" == "${SHA256,,}" ]] || { echo "Checksum mismatch: $actual != $SHA256" >&2; exit 1; }
  echo "Checksum OK: $actual"
else
  echo "No --sha256 given. Record artifact checksum before --apply: $actual"
fi
bash "$HERE/install.sh" "$MODE" --binary "$BINARY"
if [[ "$MODE" == "--apply" ]]; then
  /usr/local/bin/oura-collector validate /etc/oura-collector/collector.toml \
    || echo 'Config not valid yet: replace CHANGE_ME placeholders and provision 0600 keys, then validate again.' >&2
fi
echo 'Next: follow the manual start gate in the runbook (docs/pi-collector.md / /usr/share/doc/oura-collector/pi-collector.md).'