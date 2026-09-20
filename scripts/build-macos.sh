#!/usr/bin/env bash
# Build aetherium and package it as a macOS .app bundle plus a zip in dist/.
# Usage: ./scripts/build-macos.sh [--debug]   (default: --release)
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE=release
[[ "${1:-}" == "--debug" ]] && PROFILE=debug

ICON_SRC="aetherium_icons_dark_v2/main_executable.svg"
APP="aetherium.app"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

ARGS=(--locked)
[[ "$PROFILE" == release ]] && ARGS+=(--release)
cargo build "${ARGS[@]}"

# Rasterize the SVG icon and assemble an .iconset at all required sizes.
TMP=$(mktemp -d)
ICONSET="$TMP/aetherium.iconset"
mkdir -p "$ICONSET"
qlmanage -t -s 1024 -o "$TMP" "$ICON_SRC" >/dev/null
PNG="$TMP/aetherium-1024.png"
mv "$TMP/$(basename "$ICON_SRC").png" "$PNG"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$PNG" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
    sips -z $((size * 2)) $((size * 2)) "$PNG" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
mkdir -p dist
iconutil -c icns "$ICONSET" -o dist/aetherium.icns
rm -rf "$TMP"

# Assemble the bundle.
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/$PROFILE/aetherium" "$APP/Contents/MacOS/aetherium"
cp dist/aetherium.icns "$APP/Contents/Resources/aetherium.icns"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>aetherium</string>
    <key>CFBundleDisplayName</key>
    <string>aetherium</string>
    <key>CFBundleIdentifier</key>
    <string>dev.aetherium.app</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleExecutable</key>
    <string>aetherium</string>
    <key>CFBundleIconFile</key>
    <string>aetherium</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

# Ad-hoc sign so Gatekeeper allows local launches.
codesign --force --deep --sign - "$APP" 2>/dev/null || true

ARCH=$(uname -m)
ZIP="dist/aetherium-${VERSION}-macos-${ARCH}.zip"
ditto -c -k --sequesterRsrc --keepParent "$APP" "$ZIP"
echo "built $APP and $ZIP ($PROFILE)"
