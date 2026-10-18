#!/usr/bin/env bash
# Builds fraum_<version>_amd64.deb and FRAUM-<version>-x86_64.AppImage from a release binary.
# Usage: build-packages.sh <path to fraum-native> <version> <output directory>
set -euo pipefail

binary=$(realpath "$1")
version=$2
out=$(realpath -m "$3")
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$out"

# .deb
deb="$work/deb"
install -Dm755 "$binary" "$deb/usr/bin/fraum-native"
install -Dm644 "$here/fraum.desktop" "$deb/usr/share/applications/fraum.desktop"
install -Dm644 "$root/assets/icon-256.png" "$deb/usr/share/icons/hicolor/256x256/apps/fraum.png"
mkdir -p "$deb/DEBIAN"
cat > "$deb/DEBIAN/control" <<EOF
Package: fraum
Version: $version
Section: net
Priority: optional
Architecture: amd64
Depends: libc6 (>= 2.35), libssl3 (>= 3.0), libgcc-s1
Recommends: libxkbcommon0, libwayland-client0, libgl1
Maintainer: FRAUM <noreply@users.noreply.github.com>
Homepage: https://github.com/pancakers2001/FRAUM
Description: Client for the MiNERVA Archive
 Search the MiNERVA Archive and download games over BitTorrent, with
 curated presets and RetroBat / Batocera system folders.
EOF
dpkg-deb --root-owner-group --build "$deb" "$out/fraum_${version}_amd64.deb"

# AppImage, with the OpenSSL libraries bundled so it runs on distributions that lack OpenSSL 3
appdir="$work/FRAUM.AppDir"
install -Dm755 "$binary" "$appdir/usr/bin/fraum-native"
install -Dm644 "$here/fraum.desktop" "$appdir/fraum.desktop"
install -Dm644 "$root/assets/icon-256.png" "$appdir/fraum.png"
mkdir -p "$appdir/usr/lib"
for library in libssl.so.3 libcrypto.so.3; do
  cp -L "$(ldd "$binary" | awk -v lib="$library" '$1 == lib { print $3 }')" "$appdir/usr/lib/$library"
done
cat > "$appdir/AppRun" <<'EOF'
#!/bin/sh
here=$(dirname "$(readlink -f "$0")")
export LD_LIBRARY_PATH="$here/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
exec "$here/usr/bin/fraum-native" "$@"
EOF
chmod +x "$appdir/AppRun"

tool="$work/appimagetool"
curl -fsSL -o "$tool" \
  https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
chmod +x "$tool"
ARCH=x86_64 "$tool" --appimage-extract-and-run "$appdir" "$out/FRAUM-${version}-x86_64.AppImage"
