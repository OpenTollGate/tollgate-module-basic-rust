#!/usr/bin/env bash
# Build a tollgate-wrt .ipk from the staged cross-compiled binary (opkg,
# OpenWrt ≤ 24.10). OpenWrt ≥ 25.12 is apk-native (ADBd binary packages)
# — that format is produced by the OpenWrt SDK feed (openwrt/Makefile),
# not by a CI-side tarball; this script exists for direct opkg installs.
#
# Format: ar-container ipk with gzip members, GNU ar (busybox-ar ipks are
# rejected by opkg 24.10+).
# Usage: packaging/build-ipk.sh [arch]   (x86_64 default)
# Env:   PKG_VERSION (default: Cargo.toml version)
set -euo pipefail

ARCH="${1:-x86_64}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"

case "$ARCH" in
    x86_64)  MUSL_TARGET="x86_64-unknown-linux-musl" ;;
    aarch64) MUSL_TARGET="aarch64-unknown-linux-musl" ;;
    armv7)   MUSL_TARGET="armv7-unknown-linux-musleabihf" ;;
    *) echo "Unknown architecture: $ARCH" >&2; exit 1 ;;
esac

VERSION="${PKG_VERSION:-$(grep '^version' "$REPO_DIR/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')}"
echo "=== Building .ipk for $ARCH ($MUSL_TARGET) v$VERSION ==="

TARGET_DIR="$(cd "$REPO_DIR" && cargo metadata --format-version 1 --no-deps 2>/dev/null \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
[ -n "$TARGET_DIR" ] || TARGET_DIR="$REPO_DIR/target"
BIN="$TARGET_DIR/$MUSL_TARGET/release/tollgate-module-basic-rust"
[ -x "$BIN" ] || { echo "binary not built: $BIN (run: cargo build --release --target $MUSL_TARGET)" >&2; exit 1; }

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
PKGROOT="$STAGE/pkg"
CTL="$STAGE/ctl"
mkdir -p "$PKGROOT/usr/bin" "$PKGROOT/etc/init.d" "$PKGROOT/lib/upgrade/keep.d" "$CTL"

cp "$BIN" "$PKGROOT/usr/bin/tollgate-wrt"
chmod 755 "$PKGROOT/usr/bin/tollgate-wrt"
cp "$REPO_DIR/openwrt/files/etc/init.d/tollgate-wrt" "$PKGROOT/etc/init.d/tollgate-wrt"
chmod 755 "$PKGROOT/etc/init.d/tollgate-wrt"
cp "$REPO_DIR/openwrt/files/lib/upgrade/keep.d/tollgate" "$PKGROOT/lib/upgrade/keep.d/tollgate"

cat > "$CTL/control" << EOF
Package: tollgate-wrt
Version: $VERSION
Depends: libc, nodogsplash, jq
Provides: nodogsplash-files
Source: package/tollgate-wrt
SourceName: tollgate-wrt
Section: net
SourceDateEpoch: $(date +%s)
Maintainer: TollGate <tollgate@tollgate.me>
Architecture: $ARCH
Description: TollGate captive-portal gateway (Rust edition, CDK wallet)
EOF

cat > "$CTL/conffiles" << 'EOF'
/etc/tollgate/config.json
/etc/tollgate/identities.json
EOF

cat > "$CTL/postinst" << 'EOF'
#!/bin/sh
[ -d /etc/tollgate ] || mkdir -p /etc/tollgate
exit 0
EOF
chmod 755 "$CTL/postinst"

(cd "$CTL" && tar czf "$STAGE/control.tar.gz" ./control ./conffiles ./postinst)
(cd "$PKGROOT" && tar czf "$STAGE/data.tar.gz" ./usr ./etc ./lib)
echo "2.0" > "$STAGE/debian-binary"

mkdir -p "$REPO_DIR/dist"
IPK="$REPO_DIR/dist/tollgate-wrt_${VERSION}_${ARCH}.ipk"
rm -f "$IPK"
(cd "$STAGE" && ar rD "$IPK" debian-binary control.tar.gz data.tar.gz 2>/dev/null \
    || ar r "$IPK" debian-binary control.tar.gz data.tar.gz)
echo "=== $IPK ==="
ls -lh "$IPK"
