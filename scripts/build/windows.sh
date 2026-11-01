#!/bin/sh
# Builds xharness.exe for 64-bit Windows (10 or later), cross-compiled with
# MinGW-w64 from macOS or Linux.
#
#   scripts/build/windows.sh
#
# Output: dist/windows/xharness-<version>-x64.zip (xharness.exe).
# Needs Rust and MinGW-w64: `brew install mingw-w64` (macOS) or
# `sudo apt install mingw-w64` (Debian/Ubuntu). lino isn't bundled: it
# doesn't support Windows yet.
. "$(dirname "$0")/common.sh"

need cargo "install Rust from https://rustup.rs"
need x86_64-w64-mingw32-gcc "install MinGW-w64: brew install mingw-w64 (or apt install mingw-w64)"
TARGET=x86_64-pc-windows-gnu
build_target "$TARGET"

OUT="$DIST/windows"
STAGE=$(mktemp -d)/xharness-$VERSION-x64
mkdir -p "$OUT" "$STAGE"
cp "$ROOT/target/$TARGET/release/xharness.exe" "$STAGE/"
cat > "$STAGE/README.txt" <<TXT
XHarness $VERSION for Windows 10 and 11 (64-bit)

Run xharness.exe. Windows SmartScreen may warn about an unknown publisher
the first time: click "More info", then "Run anyway".

Terminals run PowerShell (PowerShell 7 if it's installed). The Git window
needs Git for Windows (https://git-scm.com). lino isn't included on Windows.
TXT
ZIP="$OUT/xharness-$VERSION-x64.zip"
rm -f "$ZIP"
(cd "$(dirname "$STAGE")" && zip -qr "$ZIP" "$(basename "$STAGE")")
say "done: $ZIP"
