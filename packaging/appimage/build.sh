#!/bin/sh
# Build ModForge-<version>-x86_64.AppImage from a built binary.
#   cargo build --release && packaging/appimage/build.sh [path/to/modforge] [output folder]
# The AppImage carries only ModForge itself: it uses the system's curl, bsdtar/7z and a Chromium based browser.
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
bin=${1:-$root/target/release/modforge}
out=${2:-$PWD}
ver=$("$bin" --version | awk '{print $2}')
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
app="$work/AppDir"
mkdir -p "$app/usr/bin" "$app/usr/share/applications" "$app/usr/share/icons/hicolor/256x256/apps"
cp "$bin" "$app/usr/bin/modforge"
cp "$root/src/icon.png" "$app/modforge.png"
cp "$root/src/icon.png" "$app/usr/share/icons/hicolor/256x256/apps/modforge.png"
sed 's|^Exec=.*|Exec=modforge|' "$root/packaging/modforge.desktop" > "$app/modforge.desktop"
cp "$app/modforge.desktop" "$app/usr/share/applications/modforge.desktop"
printf '#!/bin/sh\nHERE=$(dirname "$(readlink -f "$0")")\nexec "$HERE/usr/bin/modforge" "$@"\n' > "$app/AppRun"
chmod +x "$app/AppRun"
tool=$(command -v appimagetool || true)
if [ -z "$tool" ]; then
  tool="$work/appimagetool"
  curl -fsSL -o "$tool" https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage || true
  if [ -f "$tool" ]; then
    chmod +x "$tool"
  fi
fi
mkdir -p "$out"
if [ -x "$tool" ]; then
  ARCH=x86_64 "$tool" --appimage-extract-and-run "$app" "$out/ModForge-$ver-x86_64.AppImage"
  echo "built $out/ModForge-$ver-x86_64.AppImage"
else
  echo "AppDir prepared at $app (appimagetool not available on system, AppDir ready for packaging)"
fi
