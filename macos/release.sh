#!/usr/bin/env bash
# Checks, bumps the version, builds LStack.app on this Mac and publishes a GitHub release.
#
#   macos/release.sh [patch|minor|major|X.Y.Z|current] [options]
#
#   1. requires a clean tree that is up to date with origin, and the GitHub CLI (`gh auth login`)
#   2. cargo test + cargo clippy (warnings are errors)
#   3. bumps `version` in Cargo.toml (default: patch) + Cargo.lock, commits "chore: release vX.Y.Z", tags it
#   4. builds dist/LStack-X.Y.Z-<arch>.{dmg,zip,sha256} with macos/build.sh, and the Tailscale helper with
#      tailnet/build.sh (its manifest must be signed: TAILNET_SIGNING_KEY or the keychain item, see that script)
#   5. pushes the branch and tag together, then creates the GitHub release with those files (gh)
#   If anything fails before the push (checks, build), the bump commit and tag are undone.
#   If only the upload fails, rerun with `current --skip-build` to retry it.
#
#   current         release the version already in Cargo.toml (no bump; e.g. to retry a failed upload)
#   --skip-build    upload what's already in dist/
#   --draft         create the release as a draft
#   --ci            don't build here: push, then start the GitHub Actions workflow to build and upload
#   --skip-checks   don't run cargo test / clippy
#   --dry-run       run the checks and show the new version, but change nothing
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

BUMP=patch; LOCAL=1; BUILD=1; DRAFT=(); CHECKS=1; DRY=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    patch|minor|major|current) BUMP="$1"; shift ;;
    [0-9]*.[0-9]*.[0-9]*) BUMP="$1"; shift ;;
    --local) LOCAL=1; shift ;;  # the default; kept for old muscle memory
    --ci) LOCAL=0; shift ;;
    --skip-build) BUILD=0; shift ;;
    --draft) DRAFT=(--draft); shift ;;
    --skip-checks) CHECKS=0; shift ;;
    --dry-run) DRY=1; shift ;;
    -h|--help) sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
  esac
done

die() { echo "✗ $*" >&2; exit 1; }
step() { echo "▸ $*"; }

current_version() { sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1; }
OLD="$(current_version)"
BRANCH="$(git branch --show-current)"
REPO_URL="$(git remote get-url origin | sed -E 's#^(ssh://)?git@github\.com[:/]#https://github.com/#; s#\.git$##')"

# 1. Clean, up-to-date tree.
[[ -n "$BRANCH" ]] || die "not on a branch (detached HEAD)"
[[ -z "$(git status --porcelain)" ]] || die "working tree has uncommitted changes — commit or stash them first"
if [[ $DRY == 0 ]]; then
  command -v gh >/dev/null || die "GitHub CLI not found — brew install gh"
  gh auth status >/dev/null 2>&1 || die "not logged in to GitHub — run: gh auth login"
fi
step "fetching origin"
git fetch -q --tags origin
if git rev-parse -q --verify "origin/$BRANCH" >/dev/null; then
  [[ "$(git rev-list --count "HEAD..origin/$BRANCH")" == 0 ]] || die "$BRANCH is behind origin/$BRANCH — pull first"
fi

# New version.
case "$BUMP" in
  current) NEW="$OLD" ;;
  patch|minor|major)
    IFS=. read -r MA MI PA <<<"${OLD%%-*}"
    case "$BUMP" in
      major) NEW="$((MA + 1)).0.0" ;;
      minor) NEW="$MA.$((MI + 1)).0" ;;
      patch) NEW="$MA.$MI.$((PA + 1))" ;;
    esac ;;
  *) NEW="$BUMP" ;;
esac
TAG="v$NEW"
if [[ "$BUMP" != current ]] && git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  die "$TAG already exists — pick another version"
fi

# 2. Checks.
if [[ $CHECKS == 1 ]]; then
  step "cargo test"
  cargo test --quiet
  step "cargo clippy"
  cargo clippy --quiet --all-targets -- -D warnings
  step "go test (tailnet helper)"
  (cd tailnet && "${GO:-$(command -v go || echo /opt/homebrew/bin/go)}" test ./...)
fi

if [[ $DRY == 1 ]]; then
  echo "✓ checks passed — would release $OLD → $NEW ($TAG) from $BRANCH (dry run, nothing changed)"
  exit 0
fi

# Undo the bump commit/tag if we fail before the push.
START="$(git rev-parse HEAD)"; PUSHED=0; MADE_TAG=0
rollback() {
  local code=$?
  if [[ $code != 0 && $PUSHED == 0 ]]; then
    echo "✗ failed — restoring the tree to $(git rev-parse --short "$START")" >&2
    [[ $MADE_TAG == 1 ]] && git tag -d "$TAG" >/dev/null 2>&1 || true
    git reset -q --hard "$START"
  fi
}
trap rollback EXIT

# 3. Bump + commit.
if [[ "$NEW" != "$OLD" ]]; then
  step "version $OLD → $NEW"
  sed -i '' -E "1,/^version *= *\".*\"/s/^version *= *\".*\"/version = \"$NEW\"/" Cargo.toml
  [[ "$(current_version)" == "$NEW" ]] || die "couldn't update Cargo.toml"
  cargo update --quiet --workspace --offline 2>/dev/null || cargo check --quiet
  git add Cargo.toml Cargo.lock
  git commit -q -m "chore: release $TAG"
fi

# Tag locally (pushed after the build succeeds).
if ! git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  git tag -a "$TAG" -m "LStack $NEW"
  MADE_TAG=1
  step "tagged $TAG"
elif [[ "$(git rev-list -n1 "$TAG")" != "$(git rev-parse HEAD)" ]]; then
  die "$TAG already exists on another commit — bump the version"
fi

# 4. Build here, before anything leaves this Mac, so a broken build rolls back cleanly.
ARCH="$(uname -m)"
ASSETS=("dist/LStack-$NEW-$ARCH.dmg" "dist/LStack-$NEW-$ARCH.zip" "dist/LStack-$NEW-$ARCH.sha256")
# Every release carries the helper so `releases/latest/download/tailnet-manifest.json` always resolves.
TAILNET_VERSION="$(tr -d '[:space:]' < tailnet/VERSION)"
TAILNET_ASSETS=("dist/tailnet/lstack-tailnet-$TAILNET_VERSION-darwin-universal.tar.gz" "dist/tailnet/tailnet-manifest.json" "dist/tailnet/tailnet-manifest.sig")
if [[ $LOCAL == 1 ]]; then
  if [[ $BUILD == 1 ]]; then
    step "building LStack $NEW"
    macos/build.sh
    step "building the Tailscale helper $TAILNET_VERSION"
    tailnet/build.sh
  fi
  for a in "${ASSETS[@]:0:2}" "${TAILNET_ASSETS[@]}"; do [[ -f "$a" ]] || die "missing $a"; done
  ASSETS+=("${TAILNET_ASSETS[@]}")
  (cd dist && shasum -a 256 "LStack-$NEW-$ARCH.dmg" "LStack-$NEW-$ARCH.zip" > "LStack-$NEW-$ARCH.sha256")
fi

# 5. Push (atomically, so the branch and tag land together or not at all), then publish.
step "pushing $BRANCH and $TAG"
git push -q --atomic origin "HEAD:refs/heads/$BRANCH" "refs/tags/$TAG"
PUSHED=1

if [[ $LOCAL == 0 ]]; then
  gh workflow run release.yml -f tag="$TAG"
  echo "✓ pushed $TAG — GitHub Actions is building it"
  echo "  progress: $REPO_URL/actions"
  echo "  release:  $REPO_URL/releases/tag/$TAG"
  exit 0
fi

if gh release view "$TAG" >/dev/null 2>&1; then
  step "release $TAG exists — replacing assets"
  gh release upload "$TAG" "${ASSETS[@]}" --clobber
else
  step "creating release $TAG"
  gh release create "$TAG" "${ASSETS[@]}" --title "LStack $NEW" --generate-notes --verify-tag "${DRAFT[@]+"${DRAFT[@]}"}"
fi
echo "✓ $(gh release view "$TAG" --json url -q .url)"
