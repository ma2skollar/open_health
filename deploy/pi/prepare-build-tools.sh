#!/usr/bin/env bash
# Download public tools into ignored target/ only. This script never builds code.
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
TOOLS="$ROOT/target/pi-tools"
RUST_VERSION=1.100.0
RUST_DATE=2026-11-12
ZIG_VERSION=0.14.1
mkdir -p "$TOOLS/downloads" "$TOOLS/bin"
for component in "rustc-$RUST_VERSION-x86_64-unknown-linux-gnu" \
                 "cargo-$RUST_VERSION-x86_64-unknown-linux-gnu" \
                 "rust-std-$RUST_VERSION-x86_64-unknown-linux-gnu" \
                 "rust-std-$RUST_VERSION-aarch64-unknown-linux-musl"; do
  archive="$component.tar.xz"
  base="https://static.rust-lang.org/dist/$RUST_DATE/$archive"
  curl --fail --location --retry 2 "$base" --output "$TOOLS/downloads/$archive"
  curl --fail --location --retry 2 "$base.sha256" --output "$TOOLS/downloads/$archive.sha256"
  (cd "$TOOLS/downloads" && sha256sum --check "$archive.sha256")
  tar -xJf "$TOOLS/downloads/$archive" -C "$TOOLS"
  "$TOOLS/$component/install.sh" --prefix="$TOOLS/rust" --disable-ldconfig
 done
archive="zig-x86_64-linux-$ZIG_VERSION.tar.xz"
curl --fail --location --retry 2 "https://ziglang.org/download/$ZIG_VERSION/$archive" --output "$TOOLS/downloads/$archive"
printf '%s  %s\n' '24aeeec8af16c381934a6cd7d95c807a8cb2cf7df9fa40d359aa884195c4716c' "$TOOLS/downloads/$archive" | sha256sum --check -
tar -xJf "$TOOLS/downloads/$archive" -C "$TOOLS"
python3 - "$TOOLS" <<'PY'
import pathlib, sys
root = pathlib.Path(sys.argv[1])
zig = root / 'zig-x86_64-linux-0.14.1/zig'
for name, mode, target in [
    ('cc', 'cc', 'x86_64-linux-gnu'),
    ('c++', 'c++', 'x86_64-linux-gnu'),
    ('ar', 'ar', None),
    ('aarch64-linux-musl-cc', 'cc', 'aarch64-linux-musl'),
    ('aarch64-linux-musl-ar', 'ar', None),
]:
    wrapper = root / 'bin' / name
    if target is None:
        script = f'#!/bin/sh\nexec "{zig}" ar "$@"\n'
    else:
        # cc-rs recognizes Clang and supplies a Rust-style --target. Zig uses
        # a different triple grammar; keep this wrapper's fixed target instead.
        script = f'''#!/usr/bin/env python3
import os, sys
raw = sys.argv[1:]
args = []
index = 0
while index < len(raw):
    argument = raw[index]
    if argument in ('-target', '--target'):
        index += 2
        continue
    # zig cc rejects this flag; LLVM applies the Cortex-A53 843419 fix by default.
    if argument == '-Wl,--fix-cortex-a53-843419':
        index += 1
        continue
    if not argument.startswith('--target='):
        args.append(argument)
    index += 1
os.execv({str(zig)!r}, [{str(zig)!r}, {mode!r}, '-target', {target!r}] + args)
'''
    wrapper.write_text(script)
    wrapper.chmod(0o755)
PY
cat > "$TOOLS/env.sh" <<'EOF'
# Source this file from the repository root.
PI_TOOLS="$(pwd)/target/pi-tools"
export PI_TOOLS
export PATH="$PI_TOOLS/bin:$PI_TOOLS/rust/bin:$PATH"
export CARGO_HOME="$PI_TOOLS/cargo-home"
export RUSTUP_HOME="$PI_TOOLS/rustup-home"
export ZIG_GLOBAL_CACHE_DIR="$PI_TOOLS/zig-global-cache"
export ZIG_LOCAL_CACHE_DIR="$PI_TOOLS/zig-local-cache"
export CC="$PI_TOOLS/bin/cc"
export AR="$PI_TOOLS/bin/ar"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$PI_TOOLS/bin/cc"
export CC_aarch64_unknown_linux_musl="$PI_TOOLS/bin/aarch64-linux-musl-cc"
export AR_aarch64_unknown_linux_musl="$PI_TOOLS/bin/aarch64-linux-musl-ar"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$PI_TOOLS/bin/aarch64-linux-musl-cc"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C link-self-contained=no"
EOF
printf 'Prepared tools only. From repository root: source target/pi-tools/env.sh\n'
