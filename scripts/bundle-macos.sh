#!/usr/bin/env bash
# Bundle hc-app into HorizonCast.app and a compressed .dmg.
#
# Run on macOS after building the release binary. If both Apple targets are present
# (x86_64-apple-darwin + aarch64-apple-darwin) it produces a universal binary; otherwise
# it falls back to target/release/hc-app.
#
# Usage:  scripts/bundle-macos.sh [version]   (version defaults to 0.0.0-dev)
set -euo pipefail

APP_NAME="HorizonCast"
BUNDLE_ID="com.horizoncast.app"
VERSION="${1:-0.0.0-dev}"
LOGO="crates/hc-app/ui/logo.png"
APP="dist/$APP_NAME.app"

mkdir -p dist

# Universal binary when both arch builds exist; else the plain release build.
if [ -f target/x86_64-apple-darwin/release/hc-app ] \
   && [ -f target/aarch64-apple-darwin/release/hc-app ]; then
  lipo -create -output dist/hc-app \
    target/x86_64-apple-darwin/release/hc-app \
    target/aarch64-apple-darwin/release/hc-app
else
  cp target/release/hc-app dist/hc-app
fi

# Bundle layout.
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp dist/hc-app "$APP/Contents/MacOS/hc-app"
chmod +x "$APP/Contents/MacOS/hc-app"

# Icon: square PNG -> .icns.
ICONSET="dist/AppIcon.iconset"
rm -rf "$ICONSET"; mkdir -p "$ICONSET"
for size in 16 32 64 128 256 512; do
  sips -z "$size" "$size" "$LOGO" \
    --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  sips -z "$((size * 2))" "$((size * 2))" "$LOGO" \
    --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"
rm -rf "$ICONSET"

# Info.plist.
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>            <string>$APP_NAME</string>
  <key>CFBundleDisplayName</key>     <string>$APP_NAME</string>
  <key>CFBundleIdentifier</key>      <string>$BUNDLE_ID</string>
  <key>CFBundleExecutable</key>      <string>hc-app</string>
  <key>CFBundleIconFile</key>        <string>AppIcon</string>
  <key>CFBundleVersion</key>         <string>$VERSION</string>
  <key>CFBundleShortVersionString</key> <string>$VERSION</string>
  <key>CFBundlePackageType</key>     <string>APPL</string>
  <key>LSMinimumSystemVersion</key>  <string>12.3</string>
  <key>NSHighResolutionCapable</key> <true/>
  <key>LSApplicationCategoryType</key> <string>public.app-category.video</string>
</dict>
</plist>
PLIST

# Ad-hoc sign (keeps the Screen Recording grant stable; not notarized).
codesign --force --deep --sign - "$APP"

# Compressed disk image.
DMG="dist/HorizonCast-$VERSION-macos-universal.dmg"
hdiutil create -volname "$APP_NAME" -srcfolder "$APP" -ov -format UDZO "$DMG"
echo "Built $DMG"
