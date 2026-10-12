#!/usr/bin/env bash
# Regenerates macos/Resources/AppIcon.icns and src/daemon/web/icon.png from make-icon.swift.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TMP="$(mktemp -d)"
swift "$ROOT/macos/icon/make-icon.swift" "$TMP" >/dev/null
SET="$TMP/AppIcon.iconset"
mkdir -p "$SET"
for s in 16 32 128 256 512; do
  sips -z $s $s "$TMP/icon-1024.png" --out "$SET/icon_${s}x${s}.png" >/dev/null
  sips -z $((s * 2)) $((s * 2)) "$TMP/icon-1024.png" --out "$SET/icon_${s}x${s}@2x.png" >/dev/null
done
mkdir -p "$ROOT/macos/Resources"
iconutil -c icns "$SET" -o "$ROOT/macos/Resources/AppIcon.icns"
cp "$TMP/icon-1024.png" "$ROOT/macos/icon/icon-1024.png"
sips -c 840 840 "$TMP/icon-1024.png" --out "$TMP/web.png" >/dev/null
sips -z 256 256 "$TMP/web.png" --out "$ROOT/src/daemon/web/icon.png" >/dev/null
rm -rf "$TMP"
echo "✓ macos/Resources/AppIcon.icns, macos/icon/icon-1024.png, src/daemon/web/icon.png"
