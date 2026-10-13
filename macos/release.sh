#!/usr/bin/env bash
# Checks, bumps the version, tags and publishes a GitHub release.
#
#   macos/release.sh [patch|minor|major|X.Y.Z|current] [options]
#
#   1. requires a clean tree that is up to date with origin
#   2. cargo test + cargo clippy (warnings are errors)
#   3. bumps `version` in Cargo.toml (default: patch) + Cargo.lock, commits "chore: release vX.Y.Z"
#   4. tags vX.Y.Z and pushes the branch and tag together
#   5. GitHub Actions (.github/workflows/release.yml) builds LStack.app and attaches .dmg/.zip/checksums
#   If anything fails before the push, the bump commit and tag are undone, so you're back where you started.
#
#   current         release the version already in Cargo.toml (no bump; e.g. to retry a failed push)
#   --local         build here and upload with the GitHub CLI instead of Actions (needs `gh auth login`)
#   --skip-build    with --local: upload what's already in dist/
#   --draft         with --local: create the release as a draft
#   --skip-checks   don't run cargo test / clippy
#   --dry-run       run the checks and show the new version, but change nothing
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

BUMP=patch; LOCAL=0; BUILD=1; DRAFT=(); CHECKS=1; DRY=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    patch|minor|major|current) BUMP="$1"; shift ;;
    [0-9]*.[0-9]*.[0-9]*) BUMP="$1"; shift ;;
    --local) LOCAL=1; shift ;;
    --skip-build) BUILD=0; shift ;;
    --draft) DRAFT=(--draft); shift ;;
    --skip-checks) CHECKS=0; shift ;;
    --dry-run) DRY=1; shift ;;
    -h|--help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
  esac
done

die() { echo "✗ $*" >&2; exit 1; }
step() { echo "▸ $*"; }

current_version() { sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1; }
OLD="$(current_version)"
BRANCH="$(git branch --show-current)"
REPO_URL="$(git remote get-url origin | sed -E 's#^git@github.com:#https://github.com/#; s#\.git$##')"

# 1. Clean, up-to-date tree.
[[ -n "$BRANCH" ]] || die "not on a branch (detached HEAD)"
[[ -z "$(git status --porcelain)" ]] || die "working tree has uncommitted changes — commit or stash them first"
if [[ $LOCAL == 1 ]]; then
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

# 4. Tag + push (atomically, so the branch and tag land together or not at all).
if ! git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  git tag -a "$TAG" -m "LStack $NEW"
  MADE_TAG=1
  step "tagged $TAG"
elif [[ "$(git rev-list -n1 "$TAG")" != "$(git rev-parse HEAD)" ]]; then
  die "$TAG already exists on another commit — bump the version"
fi
step "pushing $BRANCH and $TAG"
git push -q --atomic origin "HEAD:refs/heads/$BRANCH" "refs/tags/$TAG"
PUSHED=1

if [[ $LOCAL == 0 ]]; then
  echo "✓ released $TAG — GitHub Actions is building it"
  echo "  progress: $REPO_URL/actions"
  echo "  release:  $REPO_URL/releases/tag/$TAG"
  exit 0
fi

# 5. (--local) Build here and upload.
ARCH="$(uname -m)"
[[ $BUILD == 1 ]] && macos/build.sh
ASSETS=("dist/LStack-$NEW-$ARCH.dmg" "dist/LStack-$NEW-$ARCH.zip")
for a in "${ASSETS[@]}"; do [[ -f "$a" ]] || die "missing $a"; done
(cd dist && shasum -a 256 "LStack-$NEW-$ARCH.dmg" "LStack-$NEW-$ARCH.zip" > "LStack-$NEW-$ARCH.sha256")
ASSETS+=("dist/LStack-$NEW-$ARCH.sha256")

if gh release view "$TAG" >/dev/null 2>&1; then
  step "release $TAG exists — replacing assets"
  gh release upload "$TAG" "${ASSETS[@]}" --clobber
else
  step "creating release $TAG"
  gh release create "$TAG" "${ASSETS[@]}" --title "LStack $NEW" --generate-notes --verify-tag "${DRAFT[@]+"${DRAFT[@]}"}"
fi
echo "✓ $(gh release view "$TAG" --json url -q .url)"
