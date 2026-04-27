#!/bin/bash
# scripts/release.sh — bump version and cut a release tag
#
# Usage:
#   ./scripts/release.sh          # bump patch  (v1.2.3 → v1.2.4)
#   ./scripts/release.sh minor    # bump minor  (v1.2.3 → v1.3.0)
#   ./scripts/release.sh major    # bump major  (v1.2.3 → v2.0.0)
set -euo pipefail

BUMP="${1:-patch}"
if [[ "$BUMP" != "patch" && "$BUMP" != "minor" && "$BUMP" != "major" ]]; then
  echo "usage: $0 [patch|minor|major]" >&2
  exit 1
fi

REPO_DIR="$(git rev-parse --show-toplevel)"
cd "$REPO_DIR"
TMP_WORKTREE=""

cleanup() {
  if [[ -n "$TMP_WORKTREE" ]]; then
    git worktree remove --force "$TMP_WORKTREE" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

update_well_known_release_number() {
  local repo_dir=$1
  local version=$2
  python3 - "$repo_dir" "$version" <<'PY'
from pathlib import Path
import re
import sys

repo_dir = Path(sys.argv[1])
version = sys.argv[2]

cargo_toml = repo_dir / "Cargo.toml"
text = cargo_toml.read_text()
text, count = re.subn(r'(?m)^version = "[^"]+"$', f'version = "{version}"', text, count=1)
if count != 1:
    raise SystemExit("error: failed to update Cargo.toml version")
cargo_toml.write_text(text)

cargo_lock = repo_dir / "Cargo.lock"
text = cargo_lock.read_text()
text, count = re.subn(
    r'(\[\[package\]\]\nname = "term"\nversion = ")[^"]+(")',
    rf'\g<1>{version}\2',
    text,
    count=1,
)
if count != 1:
    raise SystemExit('error: failed to update Cargo.lock term package version')
cargo_lock.write_text(text)
PY
}

# ── Pre-flight checks ─────────────────────────────────────────────────────────

# Check for uncommitted changes in the working copy
if [ -n "$(jj diff -r @)" ]; then
  echo "error: working copy has uncommitted changes" >&2
  exit 1
fi

# Fetch and check main is in sync with origin
jj git fetch --quiet
if [ "$(git rev-parse main)" != "$(git rev-parse origin/main)" ]; then
  echo "error: main is not in sync with origin/main — push or pull first" >&2
  exit 1
fi

# ── Compute next version ──────────────────────────────────────────────────────

LATEST=$(git tag --sort=-version:refname | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | head -1) || true

if [ -z "$LATEST" ]; then
  MAJOR=0; MINOR=0; PATCH=0
else
  IFS='.' read -r MAJOR MINOR PATCH <<< "${LATEST#v}"
fi

case "$BUMP" in
  major) MAJOR=$((MAJOR + 1)); MINOR=0; PATCH=0 ;;
  minor) MINOR=$((MINOR + 1)); PATCH=0 ;;
  patch) PATCH=$((PATCH + 1)) ;;
esac

VERSION="v${MAJOR}.${MINOR}.${PATCH}"
VERSION_NO_V="${VERSION#v}"

# ── Update version on main ────────────────────────────────────────────────────

TMP_WORKTREE="$(mktemp -d "${TMPDIR:-/tmp}/term-release.XXXXXX")"
git worktree add --detach "$TMP_WORKTREE" main >/dev/null

echo "→ Updating well-known release number to $VERSION"
update_well_known_release_number "$TMP_WORKTREE" "$VERSION_NO_V"

git -C "$TMP_WORKTREE" add Cargo.toml Cargo.lock
git -C "$TMP_WORKTREE" commit -m "release $VERSION" >/dev/null

echo "→ Pushing version bump to origin/main"
if ! git -C "$TMP_WORKTREE" push origin HEAD:main; then
  echo "error: failed to push version bump to origin/main" >&2
  exit 1
fi

# ── Tag & push ────────────────────────────────────────────────────────────────

# Tag the version-bump commit on main explicitly — in jj, HEAD points to @
# (the working copy), not the commit we just created for the release.
echo "→ Tagging $VERSION"
git tag "$VERSION" "$(git -C "$TMP_WORKTREE" rev-parse HEAD)"

echo "→ Pushing tag to origin"
if ! git push origin "$VERSION"; then
  git tag -d "$VERSION"
  echo "error: push failed — local tag $VERSION deleted" >&2
  exit 1
fi

echo ""
echo "✓ Release $VERSION is building:"
echo "  https://github.com/cartermp/term/actions"
echo ""
echo "Once done, install with:"
echo "  curl -fsSL https://github.com/cartermp/term/releases/latest/download/install.sh | bash"
