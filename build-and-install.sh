#!/bin/bash
# Build LStack.app from this checkout and install it into /Applications (then launch it).
# Needs Rust (cargo) and the Xcode command line tools. Extra arguments go to macos/build.sh.
set -euo pipefail
cd "$(dirname "$0")"
exec macos/build.sh --install "$@"
