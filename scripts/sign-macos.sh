#!/usr/bin/env bash
# Builds VibeOS.app and vibe-vm into build/ and signs both ad hoc for local
# development. vibe-vm gets the com.apple.security.virtualization
# entitlement it needs to run VMs. The app is bundled as a release is
# (scripts/release-macos.sh, which signs with a Developer ID): vibe-vm in
# Contents/MacOS, with vibed, vibe and vibe-shim from target/ (or
# CARGO_TARGET_DIR) when they're built, and vibed's LaunchAgent. Only a copy
# in Applications registers it; build/VibeOS.app installs nothing.
#
#   scripts/sign-macos.sh             debug build
#   CONFIG=release scripts/sign-macos.sh

set -euo pipefail
cd "$(dirname "$0")/.."

if [ "$(uname -s)" != Darwin ]; then
  echo "sign-macos.sh: needs macOS" >&2
  exit 1
fi

CONFIG=${CONFIG:-debug}
OUT=build
RUST=${CARGO_TARGET_DIR:-target}/$CONFIG
ENTITLEMENT=com.apple.security.virtualization
mkdir -p "$OUT"

# VibeOS.app: SwiftPM builds the executable; the bundle is assembled here.
swift build --package-path swift/VibeOS -c "$CONFIG" --product VibeOS
app_bin="$(swift build --package-path swift/VibeOS -c "$CONFIG" --show-bin-path)/VibeOS"
APP="$OUT/VibeOS.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp swift/VibeOS/Support/Info.plist "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist"
cp "$app_bin" "$APP/Contents/MacOS/VibeOS"
mkdir -p "$APP/Contents/Library/LaunchAgents"
cp swift/VibeOS/Support/io.github.tsconfigdotjson.VibeOS.vibed.plist "$APP/Contents/Library/LaunchAgents/"
plutil -lint "$APP/Contents/Library/LaunchAgents/io.github.tsconfigdotjson.VibeOS.vibed.plist"
# The app icon: VibeOS's own (swift/VibeOS/Support/AppIcon.svg), every size
# of it checked in as AppIcon.iconset (scripts/app-icon.swift renders them).
# iconutil makes the .icns, again only when the iconset changes.
iconset=swift/VibeOS/Support/AppIcon.iconset
icns="$OUT/AppIcon.icns"
if [ ! -s "$icns" ] || [ -n "$(find "$iconset" -newer "$icns")" ]; then
  iconutil -c icns "$iconset" -o "$icns"
fi
mkdir -p "$APP/Contents/Resources"
cp "$icns" "$APP/Contents/Resources/AppIcon.icns"
# The Linux builds Add Linux Host installs, when scripts/host-bundle.sh has
# made them.
if [ -d "$OUT/host-bundle" ]; then
  cp -R "$OUT/host-bundle" "$APP/Contents/Resources/linux"
fi
# vibe-vm, with the virtualization entitlement.
swift build --package-path swift/vibe-vm -c "$CONFIG" --product vibe-vm
vm_bin="$(swift build --package-path swift/vibe-vm -c "$CONFIG" --show-bin-path)/vibe-vm"
rm -f "$OUT/vibe-vm"
cp "$vm_bin" "$OUT/vibe-vm"
codesign --force --sign - --timestamp=none \
  --entitlements swift/vibe-vm/vibe-vm.entitlements "$OUT/vibe-vm"
codesign --verify --strict --verbose=2 "$OUT/vibe-vm"
entitlements=$(codesign -d --entitlements - --xml "$OUT/vibe-vm" 2>/dev/null)
case $entitlements in
  *"$ENTITLEMENT"*) ;;
  *)
    echo "sign-macos.sh: $OUT/vibe-vm is missing $ENTITLEMENT" >&2
    exit 1
    ;;
esac

# Inside the app, as a release has them: vibe-vm, and the Rust programs
# when this build has them. Each is signed before the bundle that seals it.
cp "$OUT/vibe-vm" "$APP/Contents/MacOS/vibe-vm"
for b in vibed vibe vibe-shim; do
  if [ -x "$RUST/$b" ]; then
    cp "$RUST/$b" "$APP/Contents/MacOS/$b"
    codesign --force --sign - --timestamp=none "$APP/Contents/MacOS/$b"
  else
    echo "sign-macos.sh: no $RUST/$b (cargo build first); the app has none" >&2
  fi
done
codesign --force --sign - --timestamp=none \
  --entitlements swift/VibeOS/Support/VibeOS.entitlements "$APP"
codesign --verify --strict --deep --verbose=2 "$APP"

echo "signed ad hoc: $APP and $OUT/vibe-vm (with $ENTITLEMENT)"
