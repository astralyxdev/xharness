#!/bin/sh
# Builds XHarness.app for macOS.
#
#   scripts/build/macos.sh              # this Mac's architecture
#   scripts/build/macos.sh arm64        # Apple silicon
#   scripts/build/macos.sh x86_64       # Intel
#   scripts/build/macos.sh universal    # both, in one app
#
# Output: dist/macos/XHarness.app and dist/macos/XHarness-<version>-<arch>.zip.
# lino is cloned at its pinned version and built for each architecture (by
# build.rs), and bundled into the binary. Needs Rust, Go, and Xcode's command line tools.
. "$(dirname "$0")/common.sh"

[ "$(uname -s)" = Darwin ] || die "build macOS apps on a Mac"
need lipo "install Xcode's command line tools: xcode-select --install"
need iconutil "install Xcode's command line tools: xcode-select --install"

ARCH=${1:-$(uname -m)}
case "$ARCH" in
	arm64 | aarch64) TARGETS="aarch64-apple-darwin"; ARCH=arm64 ;;
	x86_64) TARGETS="x86_64-apple-darwin" ;;
	universal) TARGETS="aarch64-apple-darwin x86_64-apple-darwin" ;;
	*) die "unknown architecture $ARCH (arm64, x86_64 or universal)" ;;
esac

prepare
BINARIES=""
for target in $TARGETS; do
	build_target "$target"
	BINARIES="$BINARIES $ROOT/target/$target/release/xharness"
done

OUT="$DIST/macos"
APP="$OUT/XHarness.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
say "assembling $APP"
# shellcheck disable=SC2086
lipo -create $BINARIES -output "$APP/Contents/MacOS/xharness"

# The icon: the app draws its own, then iconutil makes the .icns.
ICONSET=$(mktemp -d)/AppIcon.iconset
mkdir -p "$ICONSET"
"$APP/Contents/MacOS/xharness" --write-app-icon "$ICONSET/icon_512x512@2x.png"
for size in 16 32 128 256 512; do
	sips -z $size $size "$ICONSET/icon_512x512@2x.png" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
	double=$((size * 2))
	[ $double -le 1024 ] && sips -z $double $double "$ICONSET/icon_512x512@2x.png" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key><string>XHarness</string>
	<key>CFBundleDisplayName</key><string>XHarness</string>
	<key>CFBundleIdentifier</key><string>dev.astralyx.xharness</string>
	<key>CFBundleExecutable</key><string>xharness</string>
	<key>CFBundleIconFile</key><string>AppIcon</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>$VERSION</string>
	<key>CFBundleVersion</key><string>$VERSION</string>
	<key>LSMinimumSystemVersion</key><string>11.0</string>
	<key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# Ad-hoc signed, so Apple silicon runs it; sign with your identity to
# distribute it.
codesign --force --deep --sign - "$APP" >/dev/null 2>&1 || true

ZIP="$OUT/XHarness-$VERSION-$ARCH.zip"
rm -f "$ZIP"
ditto -c -k --keepParent "$APP" "$ZIP"
say "done: $APP"
say "      $ZIP"
