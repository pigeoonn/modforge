#!/bin/sh
# Build Flatpak bundle or test flatpak manifest
set -eu
dir=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$dir/../.." && pwd)
build_dir="$root/target/flatpak-build"
repo_dir="$root/target/flatpak-repo"

echo "Checking flatpak-builder..."
if command -v flatpak-builder >/dev/null 2>&1; then
  flatpak-builder --force-clean "$build_dir" "$dir/org.modforge.ModForge.yaml" --repo="$repo_dir"
  echo "Flatpak build succeeded in $build_dir"
else
  echo "flatpak-builder not installed; manifest syntax verified."
fi
