#!/bin/sh
# Cross-build the Rust gateway for OpenWrt aarch64 (MT6000 / MT3000).
#
# WHY THIS SCRIPT EXISTS: the documented blocker was "no musl cross-linker", and the
# obvious fixes do not work. Recorded here so nobody repeats them:
#
#   * `gcc-aarch64-linux-gnu` (Ubuntu apt) is a GLIBC toolchain. Linking rustc's musl
#     `.a` files with it fails on `pwrite64`, `mmap64`, `lstat64` -- glibc renamed the
#     64-bit-offset syscalls, musl did not.
#   * `zig cc` gets further but cannot finish: rustc supplies musl's self-contained
#     crt1.o/crti.o/crtn.o, and zig adds its own CRT -> `duplicate symbol: _start_c`.
#     Suppressing it (`-nostartfiles`) does not help, because rustc passes the CRT
#     objects explicitly. zig is a dead end for this target.
#   * Ubuntu has no packaged aarch64-musl GCC at all.
#
# What works: the musl.cc cross toolchain. One download, no apt, no zig.
#
#   curl -sL https://musl.cc/aarch64-linux-musl-cross.tgz | tar xz -C "$HOME"
#
# Usage: ci/cross-build-aarch64.sh [--install]   (--install fetches the toolchain)
set -eu

TOOLCHAIN="${TOOLCHAIN:-$HOME/aarch64-linux-musl-cross}"
CC="$TOOLCHAIN/bin/aarch64-linux-musl-gcc"
TARGET=aarch64-unknown-linux-musl

if [ ! -x "$CC" ]; then
    if [ "${1:-}" = "--install" ]; then
        echo "fetching musl.cc aarch64 toolchain (104 MB)..."
        curl -sL https://musl.cc/aarch64-linux-musl-cross.tgz | tar xz -C "$HOME"
    else
        echo "ERROR: $CC not found. Run with --install to fetch it." >&2
        exit 2
    fi
fi

export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$CC"
export CC_aarch64_unknown_linux_musl="$CC"
export AR_aarch64_unknown_linux_musl="$TOOLCHAIN/bin/aarch64-linux-musl-ar"

cargo build --release --target "$TARGET"

BIN="target/$TARGET/release/tollgate-module-basic-rust"
echo
echo "built: $BIN"
ls -lh "$BIN"
file "$BIN"
sha256sum "$BIN"
