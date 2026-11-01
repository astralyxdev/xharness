#!/bin/sh
# Builds xharness for Linux. Run it on Linux (cross-compiling GPUI needs the
# target's system libraries).
#
#   scripts/build/linux.sh              # this machine's architecture
#   scripts/build/linux.sh x86_64
#   scripts/build/linux.sh aarch64
#
# Output: dist/linux/xharness-<version>-<arch>.tar.gz.
# Needs Rust, Go, and GPUI's libraries; on Debian/Ubuntu:
#   sudo apt install build-essential pkg-config libxkbcommon-dev libxkbcommon-x11-dev \
#     libwayland-dev libx11-xcb-dev libvulkan-dev libfontconfig-dev libssl-dev
. "$(dirname "$0")/common.sh"

[ "$(uname -s)" = Linux ] || die "build the Linux version on Linux"
ARCH=${1:-$(uname -m)}
case "$ARCH" in
	x86_64 | amd64) ARCH=x86_64 ;;
	aarch64 | arm64) ARCH=aarch64 ;;
	*) die "unknown architecture $ARCH (x86_64 or aarch64)" ;;
esac
TARGET="$ARCH-unknown-linux-gnu"

prepare
build_target "$TARGET"

OUT="$DIST/linux"
STAGE=$(mktemp -d)/xharness-$VERSION-$ARCH
mkdir -p "$OUT" "$STAGE"
cp "$ROOT/target/$TARGET/release/xharness" "$STAGE/"
cp "$ROOT/assets/logo.svg" "$STAGE/xharness.svg"
tar -C "$(dirname "$STAGE")" -czf "$OUT/xharness-$VERSION-$ARCH.tar.gz" "$(basename "$STAGE")"
say "done: $OUT/xharness-$VERSION-$ARCH.tar.gz"
