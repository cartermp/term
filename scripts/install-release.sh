#!/bin/bash
# Download and install a pre-built Term.app release.
#
# This script is uploaded as 'install.sh' to every GitHub Release, so the
# curl one-liner always installs the matching version:
#
#   curl -fsSL https://github.com/cartermp/term/releases/latest/download/install.sh | bash
#
# Pin a specific version:
#   TERM_VERSION=v1.0.0 curl -fsSL .../releases/download/v1.0.0/install.sh | bash
set -euo pipefail

REPO="cartermp/term"
APP_DST="/Applications/Term.app"
BIN_DIR="/usr/local/bin"

# ── Determine version ────────────────────────────────────────────────────────

if [ -n "${TERM_VERSION:-}" ]; then
  VERSION="$TERM_VERSION"
else
  echo "→ Fetching latest release…"
  VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
    | grep '"tag_name"' | head -1 \
    | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')
fi

if [ -z "$VERSION" ]; then
  echo "error: could not determine release version." >&2
  echo "       Set TERM_VERSION=vX.Y.Z to pin a version." >&2
  exit 1
fi

echo "→ Installing Term $VERSION"

# ── Download ──────────────────────────────────────────────────────────────────

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

ZIP_URL="https://github.com/$REPO/releases/download/$VERSION/Term.app.zip"
echo "→ Downloading Term.app.zip…"
curl -fL --progress-bar "$ZIP_URL" -o "$TMP/Term.app.zip"

echo "→ Unpacking…"
unzip -q "$TMP/Term.app.zip" -d "$TMP"

# ── Remove quarantine (Gatekeeper blocks ad-hoc signed binaries otherwise) ───

xattr -cr "$TMP/Term.app"

# ── Install ───────────────────────────────────────────────────────────────────

echo "→ Installing to $APP_DST…"
rm -rf "$APP_DST"
cp -R "$TMP/Term.app" "$APP_DST"

echo "→ Symlinking binaries into $BIN_DIR…"
sudo mkdir -p "$BIN_DIR"
for bin in term tcat tdiff tjson; do
  sudo ln -sf "$APP_DST/Contents/MacOS/$bin" "$BIN_DIR/$bin"
done

/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
  -f "$APP_DST" 2>/dev/null || true

echo ""
echo "✓ Term $VERSION installed to $APP_DST"
echo "  • Double-click it in Finder, or run 'term' from any terminal."
echo "  • To update: re-run this script."
