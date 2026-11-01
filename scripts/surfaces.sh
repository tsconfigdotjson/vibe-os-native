#!/usr/bin/env bash
# Screenshots every surface of VibeOS.app in light and dark, for design
# review, true to the screen, materials and all:
# build/surfaces/<surface>-<light|dark>.png. It runs `VibeOS --gallery`,
# which opens each surface on canned data (FixtureDaemon), and captures each
# window with `screencapture -l`.
#
#   scripts/surfaces.sh                        every surface, both appearances
#   scripts/surfaces.sh --surface menu         just that one (repeatable)
#   scripts/surfaces.sh --appearance dark      just dark
#
# Needs the logged-in GUI session, and Screen Recording permission for the
# terminal it runs in (macOS asks once). Builds the app with
# scripts/sign-macos.sh if build/ has none. check-full.sh's `surfaces` step
# draws the same surfaces offscreen, without materials, and needs no
# permission.

set -euo pipefail
cd "$(dirname "$0")/.."

if [ "$(uname -s)" != Darwin ]; then
  echo "surfaces.sh: needs macOS" >&2
  exit 1
fi

app=build/VibeOS.app/Contents/MacOS/VibeOS
[ -x "$app" ] || scripts/sign-macos.sh
out=build/surfaces
rm -rf "$out"
mkdir -p "$out"
log=$(mktemp -t vibeos-gallery)
"$app" --gallery "$@" >"$log" 2>&1 &
pid=$!
trap 'kill "$pid" 2>/dev/null || true; rm -f "$log"' EXIT

for _ in $(seq 1 300); do
  grep -qx ready "$log" && break
  kill -0 "$pid" 2>/dev/null || { echo "the gallery quit:"; cat "$log"; exit 1; }
  sleep 0.1
done
grep -qx ready "$log" || { echo "the gallery never got ready:"; cat "$log"; exit 1; }

count=0
while read -r word name id; do
  [ "$word" = window ] || continue
  screencapture -x -o -l "$id" "$out/$name.png"
  count=$((count + 1))
done <"$log"
echo "$count screenshots in $out/"
