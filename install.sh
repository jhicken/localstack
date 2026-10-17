#!/bin/bash
# Install or update LStack.app from the latest GitHub release.
#   curl -fsSL https://raw.githubusercontent.com/jhicken/localstack/main/install.sh | bash
# Options (environment): LSTACK_VERSION=0.7.1 to pick a release, LSTACK_APP_DIR=~/Applications to install elsewhere.
set -euo pipefail

REPO="jhicken/localstack"
APP_DIR="${LSTACK_APP_DIR:-/Applications}"
APP="$APP_DIR/LStack.app"

fail() { echo "lstack install: $*" >&2; exit 1; }

[ "$(uname -s)" = "Darwin" ] || fail "LStack runs on macOS only"
[ "$(uname -m)" = "arm64" ] || fail "releases are built for Apple silicon (arm64) only; build from source instead"

if [ -n "${LSTACK_VERSION:-}" ]; then
  version="${LSTACK_VERSION#v}"
else
  tag=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)
  [ -n "$tag" ] || fail "couldn't find the latest release of $REPO"
  version="${tag#v}"
fi

current=""
if [ -d "$APP" ]; then
  current=$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" "$APP/Contents/Info.plist" 2>/dev/null || true)
fi
if [ "$current" = "$version" ]; then
  echo "LStack $version is already installed in $APP_DIR."
  exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
base="https://github.com/$REPO/releases/download/v$version"
zip="LStack-$version-arm64.zip"
echo "Downloading LStack ${version}…"
curl -fsSL -o "$tmp/$zip" "$base/$zip" || fail "couldn't download $base/$zip"
curl -fsSL -o "$tmp/sums" "$base/LStack-$version-arm64.sha256" || fail "couldn't download the checksums"
(cd "$tmp" && grep " $zip\$" sums | shasum -a 256 -c - >/dev/null) || fail "checksum mismatch for $zip"
ditto -x -k "$tmp/$zip" "$tmp/x"
[ -d "$tmp/x/LStack.app" ] || fail "$zip doesn't contain LStack.app"

# Quit the menu bar app; the daemon keeps running and the new app offers to restart it.
if pgrep -xq LStack; then
  osascript -e 'tell application id "dev.lstack.menubar" to quit' >/dev/null 2>&1 || true
  for _ in $(seq 1 50); do pgrep -xq LStack || break; sleep 0.1; done
fi

sudo=""
mkdir -p "$APP_DIR" 2>/dev/null || true
if [ ! -w "$APP_DIR" ] || { [ -e "$APP" ] && [ ! -w "$APP" ]; }; then
  echo "$APP_DIR isn't writable; using sudo."
  sudo="sudo"
fi
$sudo rm -rf "$APP"
$sudo ditto "$tmp/x/LStack.app" "$APP"
$sudo xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true

if [ -n "$current" ]; then echo "Updated LStack ${current} → ${version}."; else echo "Installed LStack $version."; fi
open "$APP"
