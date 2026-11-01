#!/bin/sh
# Builds everything this machine can: a universal app (and Windows, with
# MinGW) on macOS, the machine's architecture (and Windows) on Linux, and the
# server on its own for Linux (x64, arm64) and this machine.
. "$(dirname "$0")/common.sh"
case "$(uname -s)" in
	Darwin) "$(dirname "$0")/macos.sh" universal ;;
	Linux) "$(dirname "$0")/linux.sh" ;;
	*) die "builds run on macOS or Linux (Windows isn't supported yet)" ;;
esac
if command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
	"$(dirname "$0")/windows.sh"
fi
"$(dirname "$0")/server.sh" all
