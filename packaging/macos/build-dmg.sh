#!/usr/bin/env bash
# Builds FRAUM-<version>-<arch>.dmg containing FRAUM.app from a release binary (run on macOS).
# Usage: build-dmg.sh <path to fraum-native> <version> <arch label> <output directory>
set -euo pipefail

binary="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
version=$2
arch=$3
mkdir -p "$4"
out=$(cd "$4" && pwd)
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
app="$work/stage/FRAUM.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
install -m755 "$binary" "$app/Contents/MacOS/fraum-native"

iconset="$work/fraum.iconset"
mkdir "$iconset"
for size in 16 32 64 128 256; do
  sips -z "$size" "$size" "$root/assets/icon-256.png" --out "$iconset/icon_${size}.png" >/dev/null
done
cp "$iconset/icon_16.png" "$iconset/icon_16x16.png"
cp "$iconset/icon_32.png" "$iconset/icon_16x16@2x.png"
cp "$iconset/icon_32.png" "$iconset/icon_32x32.png"
cp "$iconset/icon_64.png" "$iconset/icon_32x32@2x.png"
cp "$iconset/icon_128.png" "$iconset/icon_128x128.png"
cp "$iconset/icon_256.png" "$iconset/icon_128x128@2x.png"
cp "$iconset/icon_256.png" "$iconset/icon_256x256.png"
rm "$iconset"/icon_16.png "$iconset"/icon_32.png "$iconset"/icon_64.png "$iconset"/icon_128.png "$iconset"/icon_256.png
iconutil -c icns "$iconset" -o "$app/Contents/Resources/fraum.icns"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>FRAUM</string>
  <key>CFBundleDisplayName</key><string>FRAUM</string>
  <key>CFBundleIdentifier</key><string>org.FRAUM.FRAUM</string>
  <key>CFBundleExecutable</key><string>fraum-native</string>
  <key>CFBundleIconFile</key><string>fraum</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
</dict>
</plist>
EOF

# An ad-hoc signature is enough for the app to launch once the user allows it; it is not notarized.
codesign --force --deep --sign - "$app"
codesign --verify --deep --strict "$app"

ln -s /Applications "$work/stage/Applications"
hdiutil create -volname "FRAUM" -srcfolder "$work/stage" -ov -format UDZO "$out/FRAUM-${version}-${arch}.dmg"
