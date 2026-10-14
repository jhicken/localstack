#!/usr/bin/env bash
# Builds the Tailscale helper (lstack-tailnet) as a universal binary and its signed release manifest.
#
#   tailnet/build.sh            → dist/tailnet/lstack-tailnet-<ver>-darwin-universal.tar.gz
#                                 dist/tailnet/tailnet-manifest.json (+ .sig when a signing key is available)
#   tailnet/build.sh --dev      → also installs it as ~/.localstack/bin/lstack-tailnet-dev for local testing
#                                 (run the daemon with LSTACK_TAILNET_BIN pointing at it)
#
# SIGN_IDENTITY="Developer ID Application: …" codesigns with a real identity (default: ad-hoc "-").
# TAILNET_SIGNING_KEY (base64 ed25519 seed) signs the manifest; if unset it's read from the login keychain item
# "dev.lstack.tailnet-signing" (`security add-generic-password -a lstack -s dev.lstack.tailnet-signing -w <key>`).
# lstack only installs helpers whose manifest is signed by the key it embeds (src/daemon/tailnet.rs).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT/tailnet"
GO="${GO:-$(command -v go || echo /opt/homebrew/bin/go)}"
VERSION="$(tr -d '[:space:]' < VERSION)"
MIN_LSTACK="$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
SIGN="${SIGN_IDENTITY:--}"
OUT="$ROOT/dist/tailnet"
mkdir -p "$OUT"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

echo "▸ building lstack-tailnet $VERSION (go, universal)"
for arch in arm64 amd64; do
  CGO_ENABLED=0 GOOS=darwin GOARCH=$arch "$GO" build -trimpath -ldflags "-s -w -X main.version=$VERSION" \
    -o "$STAGE/lstack-tailnet-$arch" .
done
lipo -create -output "$STAGE/lstack-tailnet" "$STAGE/lstack-tailnet-arm64" "$STAGE/lstack-tailnet-amd64"
RUNTIME=()
[[ "$SIGN" != "-" ]] && RUNTIME=(--options runtime --timestamp)
codesign --force --sign "$SIGN" "${RUNTIME[@]+"${RUNTIME[@]}"}" --identifier dev.lstack.tailnet "$STAGE/lstack-tailnet"
codesign --verify --strict "$STAGE/lstack-tailnet"
[[ "$("$STAGE/lstack-tailnet" version)" == "$VERSION" ]] || { echo "✗ built helper reports the wrong version" >&2; exit 1; }

ASSET="lstack-tailnet-$VERSION-darwin-universal.tar.gz"
tar -C "$STAGE" -czf "$OUT/$ASSET" lstack-tailnet
SHA="$(shasum -a 256 "$OUT/$ASSET" | cut -d' ' -f1)"
printf '{"version":"%s","asset":"%s","sha256":"%s","minLStack":"%s"}\n' "$VERSION" "$ASSET" "$SHA" "$MIN_LSTACK" \
  > "$OUT/tailnet-manifest.json"

KEY="${TAILNET_SIGNING_KEY:-$(security find-generic-password -s dev.lstack.tailnet-signing -w 2>/dev/null || true)}"
rm -f "$OUT/tailnet-manifest.sig"
if [[ -n "$KEY" ]]; then
  TAILNET_SIGNING_KEY="$KEY" "$GO" run ./cmd/lstack-sign sign "$OUT/tailnet-manifest.json" > "$OUT/tailnet-manifest.sig"
  echo "✓ manifest signed (public key $(TAILNET_SIGNING_KEY="$KEY" "$GO" run ./cmd/lstack-sign pub))"
else
  echo "! no signing key — manifest left unsigned (lstack won't install it from a release)" >&2
fi

echo "✓ $OUT/$ASSET  ($(du -h "$OUT/$ASSET" | cut -f1))"
echo "✓ $OUT/tailnet-manifest.json"

if [[ "${1:-}" == "--dev" ]]; then
  mkdir -p "$HOME/.localstack/bin"
  cp "$STAGE/lstack-tailnet" "$HOME/.localstack/bin/lstack-tailnet-dev"
  echo "✓ ~/.localstack/bin/lstack-tailnet-dev  (LSTACK_TAILNET_BIN=~/.localstack/bin/lstack-tailnet-dev)"
fi
