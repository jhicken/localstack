#!/usr/bin/env bash
# Builds LStack and publishes it as a GitHub release (tag v<version from Cargo.toml>).
#
#   macos/release.sh                 build, tag, push, create the release and upload the .zip/.dmg
#   macos/release.sh --notes "…"     use these release notes (default: auto-generated from commits)
#   macos/release.sh --draft         create the release as a draft
#   macos/release.sh --skip-build    upload whatever is already in dist/
#
# If the release already exists, the assets are replaced (handy for re-uploading a fixed build).
# Requires: a clean git tree, the GitHub CLI (`gh auth login`), and push access to `origin`.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

NOTES=""; DRAFT=(); BUILD=1
while [[ $# -gt 0 ]]; do
  case "$1" in
    --notes) NOTES="$2"; shift 2 ;;
    --draft) DRAFT=(--draft); shift ;;
    --skip-build) BUILD=0; shift ;;
    -h|--help) sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
TAG="v$VERSION"
ARCH="$(uname -m)"

command -v gh >/dev/null || { echo "✗ GitHub CLI not found — brew install gh" >&2; exit 1; }
gh auth status >/dev/null 2>&1 || { echo "✗ not logged in to GitHub — run: gh auth login" >&2; exit 1; }
if [[ -n "$(git status --porcelain)" ]]; then
  echo "✗ working tree has uncommitted changes — commit or stash them first" >&2; exit 1
fi

[[ $BUILD == 1 ]] && macos/build.sh
ASSETS=("dist/LStack-$VERSION-$ARCH.zip" "dist/LStack-$VERSION-$ARCH.dmg")
for a in "${ASSETS[@]}"; do [[ -f "$a" ]] || { echo "✗ missing $a" >&2; exit 1; }; done
(cd dist && shasum -a 256 "LStack-$VERSION-$ARCH.zip" "LStack-$VERSION-$ARCH.dmg" > "LStack-$VERSION-$ARCH.sha256")
ASSETS+=("dist/LStack-$VERSION-$ARCH.sha256")

if ! git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  git tag -a "$TAG" -m "LStack $VERSION"
  echo "▸ tagged $TAG"
fi
git push -q origin HEAD
git push -q origin "$TAG"

if gh release view "$TAG" >/dev/null 2>&1; then
  echo "▸ release $TAG exists — replacing assets"
  gh release upload "$TAG" "${ASSETS[@]}" --clobber
else
  echo "▸ creating release $TAG"
  if [[ -n "$NOTES" ]]; then
    gh release create "$TAG" "${ASSETS[@]}" --title "LStack $VERSION" --notes "$NOTES" --verify-tag "${DRAFT[@]+"${DRAFT[@]}"}"
  else
    gh release create "$TAG" "${ASSETS[@]}" --title "LStack $VERSION" --generate-notes --verify-tag "${DRAFT[@]+"${DRAFT[@]}"}"
  fi
fi
echo "✓ $(gh release view "$TAG" --json url -q .url)"
