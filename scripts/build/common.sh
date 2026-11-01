# Shared by the build scripts: checks the tools. (build.rs clones lino at its
# pinned version and builds it for each target.)
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
DIST="$ROOT/dist"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

need() {
	command -v "$1" >/dev/null 2>&1 || die "$1 is needed: $2"
}

prepare() {
	need cargo "install Rust from https://rustup.rs"
	need go "lino is built from source with Go: https://go.dev/dl (or brew install go)"
	need git "lino is cloned with git: install git"
}

# Adds a Rust target if rustup has it missing.
add_target() {
	if command -v rustup >/dev/null 2>&1; then
		rustup target list --installed | grep -qx "$1" || rustup target add "$1"
	fi
}

build_target() {
	add_target "$1"
	say "building xharness (with lino) for $1"
	(cd "$ROOT" && cargo build --release --locked --target "$1")
}
