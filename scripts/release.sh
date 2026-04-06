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

# ── Pre-flight checks ─────────────────────────────────────────────────────────

cd "$(git rev-parse --show-toplevel)"

BRANCH=$(git symbolic-ref --short HEAD 2>/dev/null || echo "")
if [ "$BRANCH" != "main" ]; then
  echo "error: must be on main (currently on '${BRANCH:-detached HEAD}')" >&2
  exit 1
fi

if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "error: working tree has uncommitted changes" >&2
  exit 1
fi

git fetch --quiet origin
if [ "$(git rev-parse HEAD)" != "$(git rev-parse origin/main)" ]; then
  echo "error: main is not in sync with origin/main — push or pull first" >&2
  exit 1
fi

# ── Compute next version ──────────────────────────────────────────────────────

LATEST=$(git tag --sort=-version:refname | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | head -1)

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

# ── Tag & push ────────────────────────────────────────────────────────────────

echo "→ Tagging $VERSION"
git tag "$VERSION"

echo "→ Pushing tag to origin"
git push origin "$VERSION"

echo ""
echo "✓ Release $VERSION is building:"
echo "  https://github.com/cartermp/term/actions"
echo ""
echo "Once done, install with:"
echo "  curl -fsSL https://github.com/cartermp/term/releases/latest/download/install.sh | bash"
