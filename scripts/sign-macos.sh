#!/usr/bin/env bash
# Builds VibeOS.app and vibe-vm into build/ and signs both ad hoc for local
# development. vibe-vm gets the com.apple.security.virtualization
# entitlement it needs to run VMs. Release signing comes with #14.
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
# The app icon, "Night Shift": scripts/app-icon.py draws it with nothing but
# python3's standard library, and iconutil makes the .icns. It's drawn again
# only when the script changes.
icns="$OUT/AppIcon.icns"
if [ ! -s "$icns" ] || [ scripts/app-icon.py -nt "$icns" ]; then
  rm -rf "$OUT/AppIcon.iconset"
  python3 scripts/app-icon.py "$OUT/AppIcon.iconset"
  iconutil -c icns "$OUT/AppIcon.iconset" -o "$icns"
fi
mkdir -p "$APP/Contents/Resources"
cp "$icns" "$APP/Contents/Resources/AppIcon.icns"
# The Linux builds Add Linux Host installs, when scripts/host-bundle.sh has
# made them.
if [ -d "$OUT/host-bundle" ]; then
  cp -R "$OUT/host-bundle" "$APP/Contents/Resources/linux"
fi
codesign --force --sign - --timestamp=none "$APP"
codesign --verify --strict --verbose=2 "$APP"

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

echo "signed ad hoc: $APP and $OUT/vibe-vm (with $ENTITLEMENT)"
