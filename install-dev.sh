#!/bin/bash
# Build LStack.app from this checkout and install it into /Applications (then launch it).
# Uses a faster, less optimised build than a release (see macos/build.sh). To install the latest
# published release instead, use install.sh.
# Needs Rust (cargo) and the Xcode command line tools. Extra arguments go to macos/build.sh.
set -euo pipefail
cd "$(dirname "$0")"
exec macos/build.sh --install "$@"
