#!/usr/bin/env bash
# Publishes a GitHub release for the version in Cargo.toml (tag v<version>).
#
#   macos/release.sh             tag v<version> and push it: GitHub Actions (.github/workflows/release.yml)
#                                builds LStack.app and attaches the .dmg, .zip and checksums to the release
#   macos/release.sh --local     build here and upload with the GitHub CLI instead (needs `gh auth login`)
#   macos/release.sh --local --skip-build   upload what's already in dist/
#   macos/release.sh --local --draft        create the release as a draft
#
# Bump `version` in Cargo.toml and commit first; the working tree must be clean.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

LOCAL=0; BUILD=1; DRAFT=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --local) LOCAL=1; shift ;;
    --skip-build) BUILD=0; shift ;;
    --draft) DRAFT=(--draft); shift ;;
    -h|--help) sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
TAG="v$VERSION"
ARCH="$(uname -m)"
REPO_URL="$(git remote get-url origin | sed -E 's#^git@github.com:#https://github.com/#; s#\.git$##')"

if [[ -n "$(git status --porcelain)" ]]; then
  echo "✗ working tree has uncommitted changes — commit or stash them first" >&2; exit 1
fi
if [[ $LOCAL == 1 ]]; then
  command -v gh >/dev/null || { echo "✗ GitHub CLI not found — brew install gh" >&2; exit 1; }
  gh auth status >/dev/null 2>&1 || { echo "✗ not logged in to GitHub — run: gh auth login" >&2; exit 1; }
fi

if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  [[ "$(git rev-list -n1 "$TAG")" == "$(git rev-parse HEAD)" ]] || echo "! $TAG already exists on an older commit — bump the version in Cargo.toml for a new release"
else
  git tag -a "$TAG" -m "LStack $VERSION"
  echo "▸ tagged $TAG"
fi
git push -q origin HEAD
git push -q origin "$TAG"

if [[ $LOCAL == 0 ]]; then
  echo "✓ pushed $TAG — GitHub Actions is building the release"
  echo "  progress: $REPO_URL/actions"
  echo "  release:  $REPO_URL/releases/tag/$TAG"
  exit 0
fi

[[ $BUILD == 1 ]] && macos/build.sh
ASSETS=("dist/LStack-$VERSION-$ARCH.dmg" "dist/LStack-$VERSION-$ARCH.zip")
for a in "${ASSETS[@]}"; do [[ -f "$a" ]] || { echo "✗ missing $a" >&2; exit 1; }; done
(cd dist && shasum -a 256 "LStack-$VERSION-$ARCH.dmg" "LStack-$VERSION-$ARCH.zip" > "LStack-$VERSION-$ARCH.sha256")
ASSETS+=("dist/LStack-$VERSION-$ARCH.sha256")

if gh release view "$TAG" >/dev/null 2>&1; then
  echo "▸ release $TAG exists — replacing assets"
  gh release upload "$TAG" "${ASSETS[@]}" --clobber
else
  echo "▸ creating release $TAG"
  gh release create "$TAG" "${ASSETS[@]}" --title "LStack $VERSION" --generate-notes --verify-tag "${DRAFT[@]+"${DRAFT[@]}"}"
fi
echo "✓ $(gh release view "$TAG" --json url -q .url)"
