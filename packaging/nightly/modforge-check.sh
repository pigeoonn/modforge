#!/bin/sh
# A short health report of all your games, once a night. It starts nothing visible and changes no game.
# Skips the night when a game or Wine is running. The newest report is ~/.cache/modforge/nightly.txt;
# the file is empty of problems when every line starts with "ok".
set -u
out="${XDG_CACHE_HOME:-$HOME/.cache}/modforge"
mkdir -p "$out"
if pgrep -x wineserver >/dev/null 2>&1 || pgrep -f 'reaper SteamLaunch|umu-run' >/dev/null 2>&1; then
  echo "skipped: a game is running" > "$out/nightly.txt"
  exit 0
fi
{ date '+%Y-%m-%d %H:%M'; nice -n 19 timeout 300 modforge --doctor; } > "$out/nightly.tmp" 2>&1 && mv "$out/nightly.tmp" "$out/nightly.txt" || mv "$out/nightly.tmp" "$out/nightly.txt"
