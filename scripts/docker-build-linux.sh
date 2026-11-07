#!/usr/bin/env bash
# Reproducible Linux build of the oura CLI/dashboard inside Docker.
# The repo root is mounted at $WORK (default /w). Output: $WORK/out/build.log
# Usage (Git Bash, from the repo root):
#   MSYS_NO_PATHCONV=1 docker run --rm -v "C:/path/to/oura:/w" \
#     -v oura-cargo:/usr/local/cargo/registry -v oura-target:/w/target \
#     -w /w rust:1-bookworm bash /w/scripts/docker-build-linux.sh
set -uo pipefail

WORK="${WORK:-/w}"
SRC="$WORK"
OUT="$WORK/out"
mkdir -p "$OUT"
LOG="$OUT/build.log"
exec > >(tee "$LOG") 2>&1

echo "== toolchain =="
rustc --version
cargo --version

echo "== system deps (build-time downloads only; not runtime deps) =="
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
  build-essential pkg-config libsqlite3-dev libdbus-1-dev ca-certificates >/dev/null

cd "$SRC" || exit 1
cp Cargo.lock "$OUT/Cargo.lock.before"

echo "== attempt 1: --locked (reproducibility check) =="
if cargo build --release --locked -p oura-cli; then
  echo "LOCKED_OK"
else
  echo "LOCKED_FAILED -> rebuilding without --locked (lock needs re-resolution)"
  cargo build --release -p oura-cli || { echo "RELEASE_BUILD_FAILED"; exit 1; }
fi

echo "== lock diff =="
diff -u "$OUT/Cargo.lock.before" Cargo.lock | head -120
echo "== artifact =="
ls -la target/release/oura
echo "BUILD_DONE"
