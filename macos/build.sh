#!/usr/bin/env bash
# Builds dist/LStack.app — the SwiftUI menu bar app with the Rust `lstack` binary (daemon + CLI)
# bundled at Contents/Helpers/lstack — plus a .zip and .dmg for distribution.
#
#   macos/build.sh             build dist/LStack.app, dist/LStack-<ver>.zip, dist/LStack-<ver>.dmg
#   macos/build.sh --install   …then replace /Applications/LStack.app and launch it
#
# SIGN_IDENTITY="Developer ID Application: …" signs with a real identity (default: ad-hoc "-").
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
SIGN="${SIGN_IDENTITY:--}"
ARCH="$(uname -m)"

echo "▸ building lstack $VERSION (rust)"
cargo build --release --quiet
echo "▸ building menu bar app (swift)"
(cd macos && swift build -c release 2>&1 | grep -E "error|warning: " || true)
SWIFT_BIN="$(cd macos && swift build -c release --show-bin-path)/LStack"

APP="dist/LStack.app"
rm -rf "$APP"
# Helpers/ rather than MacOS/: `LStack` and `lstack` would collide on a case-insensitive volume.
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Helpers" "$APP/Contents/Resources"
cp "$SWIFT_BIN" "$APP/Contents/MacOS/LStack"
cp target/release/lstack "$APP/Contents/Helpers/lstack"
cp macos/Resources/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>LStack</string>
  <key>CFBundleDisplayName</key><string>LStack</string>
  <key>CFBundleIdentifier</key><string>dev.lstack.menubar</string>
  <key>CFBundleExecutable</key><string>LStack</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundleIconName</key><string>AppIcon</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSAppleEventsUsageDescription</key><string>LStack opens Terminal to attach to your services.</string>
</dict>
</plist>
PLIST

# Sign inside-out: the helper first, then the bundle.
RUNTIME=()
[[ "$SIGN" != "-" ]] && RUNTIME=(--options runtime --timestamp)
codesign --force --sign "$SIGN" "${RUNTIME[@]+"${RUNTIME[@]}"}" --identifier dev.lstack.cli "$APP/Contents/Helpers/lstack"
codesign --force --sign "$SIGN" "${RUNTIME[@]+"${RUNTIME[@]}"}" "$APP"
codesign --verify --strict "$APP"

ZIP="dist/LStack-$VERSION-$ARCH.zip"
DMG="dist/LStack-$VERSION-$ARCH.dmg"
rm -f "$ZIP" "$DMG"
ditto -c -k --keepParent "$APP" "$ZIP"
STAGE="$(mktemp -d)"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -quiet -volname "LStack $VERSION" -srcfolder "$STAGE" -ov -format UDZO "$DMG"
rm -rf "$STAGE"

echo "✓ $APP  ($(du -sh "$APP" | cut -f1))"
echo "✓ $ZIP"
echo "✓ $DMG"

if [[ "${1:-}" == "--install" ]]; then
  osascript -e 'tell application id "dev.lstack.menubar" to quit' >/dev/null 2>&1 || true
  sleep 1
  rm -rf /Applications/LStack.app
  ditto "$APP" /Applications/LStack.app
  echo "✓ installed /Applications/LStack.app"
  open /Applications/LStack.app
fi
