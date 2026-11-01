#!/bin/sh
# Builds the server on its own (no app, no GPUI): what runs on a remote
# machine, with or without a display. Linux builds are static (musl), so one
# file runs on any distribution, and they cross-build from a Mac: Rust's own
# linker links them, nothing else to install.
#
#   scripts/build/server.sh               # this machine
#   scripts/build/server.sh linux-x64     # Linux, Intel/AMD
#   scripts/build/server.sh linux-arm64   # Linux, ARM (Graviton, Raspberry Pi 4/5)
#   scripts/build/server.sh macos-arm64
#   scripts/build/server.sh macos-x64
#   scripts/build/server.sh all           # the Linux ones and this Mac's
#
# Output: dist/server/xharness-server-<version>-<os>-<arch>.tar.gz, holding
# one file, xharness-server (lino is inside it). On the server:
#
#   tar xzf xharness-server-*.tar.gz
#   ./xharness-server --listen 0.0.0.0:38888
#
# It prints a key; paste it into XHarness (New project → Remote server).
# Needs Rust and Go (lino is built from source and bundled).
. "$(dirname "$0")/common.sh"

OUT="$DIST/server"
# Its own target folder, so it doesn't replace the app's release build.
export CARGO_TARGET_DIR="$ROOT/target/server"
export CARGO_PROFILE_RELEASE_STRIP=true

host() {
	case "$(uname -s)-$(uname -m)" in
		Darwin-arm64) echo macos-arm64 ;;
		Darwin-x86_64) echo macos-x64 ;;
		Linux-x86_64) echo linux-x64 ;;
		Linux-aarch64 | Linux-arm64) echo linux-arm64 ;;
		*) die "unknown machine $(uname -s) $(uname -m); pass a target" ;;
	esac
}

build() {
	case "$1" in
		linux-x64) TARGET=x86_64-unknown-linux-musl ;;
		linux-arm64) TARGET=aarch64-unknown-linux-musl ;;
		macos-arm64) TARGET=aarch64-apple-darwin ;;
		macos-x64) TARGET=x86_64-apple-darwin ;;
		*) die "unknown target $1 (linux-x64, linux-arm64, macos-arm64, macos-x64, all)" ;;
	esac
	add_target "$TARGET"
	say "building the server for $1 ($TARGET)"
	case "$TARGET" in
		*-linux-musl)
			# Static, linked by rust-lld with Rust's bundled musl.
			VAR=$(echo "$TARGET" | tr 'a-z-' 'A-Z_')
			(cd "$ROOT" && env "CARGO_TARGET_${VAR}_LINKER=rust-lld" \
				"CARGO_TARGET_${VAR}_RUSTFLAGS=-C link-self-contained=yes" \
				cargo build --release --locked --no-default-features --target "$TARGET")
			;;
		*)
			(cd "$ROOT" && cargo build --release --locked --no-default-features --target "$TARGET")
			;;
	esac
	STAGE=$(mktemp -d)
	cp "$CARGO_TARGET_DIR/$TARGET/release/xharness" "$STAGE/xharness-server"
	NAME="xharness-server-$VERSION-$1"
	mkdir -p "$OUT"
	tar -C "$STAGE" -czf "$OUT/$NAME.tar.gz" xharness-server
	rm -rf "$STAGE"
	say "done: $OUT/$NAME.tar.gz"
}

prepare
case "${1:-}" in
	"") build "$(host)" ;;
	all)
		build linux-x64
		build linux-arm64
		case "$(host)" in macos-*) build "$(host)" ;; esac
		;;
	*) build "$1" ;;
esac
