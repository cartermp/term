#!/bin/bash
# install.sh — build term from source and install it as a native macOS app
set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")" && pwd)"
APP_NAME="Term"
APP_BUNDLE="/Applications/$APP_NAME.app"
BIN_DIR="$REPO_DIR/target/release"

echo "→ Building (release)…"
cd "$REPO_DIR"
cargo build --release

echo "→ Creating app bundle at $APP_BUNDLE"
mkdir -p "$APP_BUNDLE/Contents/MacOS"
mkdir -p "$APP_BUNDLE/Contents/Resources"

# Copy all binaries — tcat, tdiff, and tjson must live next to term
cp "$BIN_DIR/term"  "$APP_BUNDLE/Contents/MacOS/term"
cp "$BIN_DIR/tcat"  "$APP_BUNDLE/Contents/MacOS/tcat"
cp "$BIN_DIR/tdiff" "$APP_BUNDLE/Contents/MacOS/tdiff"
cp "$BIN_DIR/tjson" "$APP_BUNDLE/Contents/MacOS/tjson"

# Build .icns from assets/icon.svg using macOS-native NSImage (no external tools needed)
ICON_SVG="$REPO_DIR/assets/icon.svg"
ICON_ICNS="$APP_BUNDLE/Contents/Resources/AppIcon.icns"
if [ -f "$ICON_SVG" ]; then
  echo "→ Generating AppIcon.icns from assets/icon.svg"
  ICONSET=$(mktemp -d)
  ICONSET_DIR="$ICONSET/AppIcon.iconset"
  mkdir -p "$ICONSET_DIR"

  # Render SVG → TIFF via NSImage (no NSDictionary needed), then TIFF → PNG via sips.
  render_png() {
    local size=$1 out=$2
    local tmp
    tmp=$(mktemp /tmp/term_icon_XXXXXX.tiff)
    osascript - "$ICON_SVG" "$tmp" "$size" <<'APPLESCRIPT' >/dev/null
use framework "AppKit"
use framework "Foundation"
use scripting additions
on run {svgPath, tiffPath, sizeStr}
  set sz to sizeStr as integer
  set img to current application's NSImage's alloc()'s initWithContentsOfFile:svgPath
  img's setSize:{sz, sz}
  img's TIFFRepresentation()'s writeToFile:tiffPath atomically:true
end run
APPLESCRIPT
    sips -s format png "$tmp" --out "$out" &>/dev/null
    rm -f "$tmp"
  }

  render_png  16  "$ICONSET_DIR/icon_16x16.png"
  render_png  32  "$ICONSET_DIR/icon_16x16@2x.png"
  render_png  32  "$ICONSET_DIR/icon_32x32.png"
  render_png  64  "$ICONSET_DIR/icon_32x32@2x.png"
  render_png 128  "$ICONSET_DIR/icon_128x128.png"
  render_png 256  "$ICONSET_DIR/icon_128x128@2x.png"
  render_png 256  "$ICONSET_DIR/icon_256x256.png"
  render_png 512  "$ICONSET_DIR/icon_256x256@2x.png"
  render_png 512  "$ICONSET_DIR/icon_512x512.png"
  render_png 1024 "$ICONSET_DIR/icon_512x512@2x.png"

  if iconutil -c icns "$ICONSET_DIR" -o "$ICON_ICNS" 2>/dev/null; then
    echo "  ✓ AppIcon.icns written"
  else
    echo "  ⚠ iconutil failed — icon skipped"
  fi
  rm -rf "$ICONSET"
fi

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
  <key>CFBundleIconFile</key>         <string>AppIcon</string>
  <key>NSHighResolutionCapable</key>  <true/>
  <key>LSUIElement</key>              <false/>
</dict>
</plist>
PLIST

# Tell Finder/Spotlight about the new bundle
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$APP_BUNDLE" 2>/dev/null || true

echo "→ Symlinking 'term' into /usr/local/bin (for CLI use)"
sudo mkdir -p /usr/local/bin
sudo ln -sf "$APP_BUNDLE/Contents/MacOS/term" /usr/local/bin/term

echo ""
echo "Done. You can now:"
echo "  • Double-click $APP_BUNDLE in Finder"
echo "  • Run 'term' from any terminal"
echo "  • Drag Term.app to your Dock"
