#!/bin/bash
# install.sh — build term and install it as a native macOS app
set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")" && pwd)"
APP_NAME="Term"
APP_BUNDLE="$HOME/Applications/$APP_NAME.app"
BIN_DIR="$REPO_DIR/target/release"

echo "→ Building (release)…"
cd "$REPO_DIR"
cargo build --release

echo "→ Creating app bundle at $APP_BUNDLE"
mkdir -p "$APP_BUNDLE/Contents/MacOS"
mkdir -p "$APP_BUNDLE/Contents/Resources"

# Copy both binaries — tcat must live next to term
cp "$BIN_DIR/term" "$APP_BUNDLE/Contents/MacOS/term"
cp "$BIN_DIR/tcat" "$APP_BUNDLE/Contents/MacOS/tcat"

# Info.plist — minimum viable for macOS to treat this as an app
cat > "$APP_BUNDLE/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>             <string>Term</string>
  <key>CFBundleDisplayName</key>      <string>Term</string>
  <key>CFBundleIdentifier</key>       <string>com.local.term</string>
  <key>CFBundleVersion</key>          <string>1.0</string>
  <key>CFBundleExecutable</key>       <string>term</string>
  <key>CFBundlePackageType</key>      <string>APPL</string>
  <key>NSHighResolutionCapable</key>  <true/>
  <key>LSUIElement</key>              <false/>
</dict>
</plist>
PLIST

echo "→ Symlinking 'term' into /usr/local/bin (for CLI use)"
sudo ln -sf "$APP_BUNDLE/Contents/MacOS/term" /usr/local/bin/term

echo ""
echo "Done. You can now:"
echo "  • Double-click $APP_BUNDLE in Finder"
echo "  • Run 'term' from any terminal"
echo "  • Drag Term.app to your Dock"
