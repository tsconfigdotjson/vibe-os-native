#!/usr/bin/env bash
# Builds a release of VibeOS for Apple Silicon (PRD: Auth, security and
# trust, Distribution): VibeOS.app with everything it runs inside it, in a
# DMG, and the Sparkle appcast for it.
#
#   VibeOS.app/Contents/
#     MacOS/VibeOS, vibed, vibe, vibe-shim, vibe-vm (with the
#           com.apple.security.virtualization entitlement)
#     Library/LaunchAgents/io.github.tsconfigdotjson.VibeOS.vibed.plist,
#           which the app registers with SMAppService on first launch
#     Resources/AppIcon.icns, and linux/: the Linux builds of vibed, vibe
#           and vibe-shim that Add Linux Host installs
#     Frameworks/Sparkle.framework, when there's a key to sign updates with
#
#   scripts/release-macos.sh [--out DIR] [--tag vX.Y.Z]
#
# What it does depends on what it's given, so a build with none of the
# secrets still works, ad hoc signed, for trying on this Mac:
#
#   MACOS_SIGN_IDENTITY  a Developer ID Application identity in a keychain
#                        on the search list, by name or SHA-1: signs with
#                        it, under the hardened runtime (else ad hoc)
#   APPLE_TEAM_ID        the identity's team, which it's checked against
#   NOTARY_KEY_ID, NOTARY_ISSUER_ID, NOTARY_KEY_P8
#                        an App Store Connect API key (the .p8's text):
#                        notarizes the app and the DMG, and staples both;
#                        or APPLE_ID and APPLE_APP_PASSWORD, with
#                        APPLE_TEAM_ID, instead
#   SPARKLE_PRIVATE_KEY  Sparkle's EdDSA private key (base64): builds Sparkle
#                        in, with its public key as SUPublicEDKey, signs the
#                        DMG and writes appcast.xml
#   RELEASE_REQUIRE=1    what release CI sets: fails unless it's all there,
#                        the Linux builds included
#   GITHUB_REPOSITORY    owner/name the release is published from (default
#                        tsconfigdotjson/vibe-os-native), for the appcast's
#                        and the app's URLs
#
# The version is the Cargo workspace's, which Info.plist must agree with;
# --tag, as CI passes it, must be v<that version>. Out comes OUT (default
# build/release/out): VibeOS-<version>.dmg, appcast.xml when Sparkle signed
# it, and SHA256SUMS. Secrets are read from the environment only, never
# printed, and written to disk only as files of this run's own, removed on
# exit.

set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)
export PATH="$HOME/.cargo/bin:$PATH"

OUT=build/release/out
TAG=
while [ $# -gt 0 ]; do
  case $1 in
    --out) OUT=$2; shift 2 ;;
    --tag) TAG=$2; shift 2 ;;
    -h | --help) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "release-macos.sh: unexpected $1" >&2; exit 2 ;;
  esac
done

say() { echo "release-macos.sh: $*" >&2; }
die() { say "$*"; exit 1; }

[ "$(uname -s)" = Darwin ] || die "needs macOS"
REQUIRE=${RELEASE_REQUIRE:-0}
REPO=${GITHUB_REPOSITORY:-tsconfigdotjson/vibe-os-native}
LABEL=io.github.tsconfigdotjson.VibeOS.vibed
TARGET=aarch64-apple-darwin

# The version: Cargo's, Info.plist's, and the tag's must agree.
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
[[ $VERSION =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "the workspace version isn't X.Y.Z: $VERSION"
plist_version=$(plutil -extract CFBundleShortVersionString raw swift/VibeOS/Support/Info.plist)
[ "$plist_version" = "$VERSION" ] || die "Info.plist says $plist_version, Cargo.toml $VERSION"
if [ -n "$TAG" ] && [ "$TAG" != "v$VERSION" ]; then
  die "the tag is $TAG, but this is version $VERSION: tag v$VERSION, or bump the version"
fi

IDENTITY=${MACOS_SIGN_IDENTITY:-}
NOTARIZE=false
NOTARY=
if [ -n "${NOTARY_KEY_ID:-}" ] && [ -n "${NOTARY_ISSUER_ID:-}" ] && [ -n "${NOTARY_KEY_P8:-}" ]; then
  NOTARIZE=true
  NOTARY=api-key
elif [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_APP_PASSWORD:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ]; then
  NOTARIZE=true
  NOTARY=apple-id
fi
SPARKLE=false
[ -n "${SPARKLE_PRIVATE_KEY:-}" ] && SPARKLE=true
if [ "$REQUIRE" = 1 ]; then
  [ -n "$IDENTITY" ] || die "no Developer ID identity (MACOS_SIGN_IDENTITY)"
  [ -n "${APPLE_TEAM_ID:-}" ] || die "no APPLE_TEAM_ID"
  $NOTARIZE || die "no notarization credentials: NOTARY_KEY_ID, NOTARY_ISSUER_ID and NOTARY_KEY_P8, or APPLE_ID and APPLE_APP_PASSWORD"
  $SPARKLE || die "no SPARKLE_PRIVATE_KEY"
  command -v cargo-zigbuild >/dev/null || die "the Linux builds need cargo-zigbuild and zig"
fi
if [ -n "$IDENTITY" ] && [ -n "${APPLE_TEAM_ID:-}" ]; then
  # The identity, by name or SHA-1, must be the team's Developer ID.
  security find-identity -v -p codesigning | grep -F -- "$IDENTITY" | grep -q "Developer ID Application: .*($APPLE_TEAM_ID)\"" ||
    die "$IDENTITY isn't a Developer ID Application identity of team APPLE_TEAM_ID in the keychains"
fi
$NOTARIZE && [ -z "$IDENTITY" ] && die "notarizing needs a Developer ID identity (MACOS_SIGN_IDENTITY)"

WORK=build/release
rm -rf "$WORK/stage" "$OUT"
mkdir -p "$WORK" "$OUT"
OUT=$(cd "$OUT" && pwd)
PRIVATE=$(mktemp -d "${TMPDIR:-/tmp}/vibeos-release.XXXXXX")
chmod 700 "$PRIVATE"
trap 'rm -rf "$PRIVATE"' EXIT

say "VibeOS $VERSION: $( [ -n "$IDENTITY" ] && echo "Developer ID" || echo "ad hoc" ) signing, notarize=$NOTARIZE, sparkle=$SPARKLE"

# The Rust binaries, for Apple Silicon.
rustup target add "$TARGET" >/dev/null
cargo build --locked --release --target "$TARGET" -p vibed -p vibe-cli -p vibe-shim
RUST=target/$TARGET/release

# The Linux builds Add Linux Host installs.
HOST_BUNDLE=
if command -v cargo-zigbuild >/dev/null; then
  scripts/host-bundle.sh --out "$WORK/host-bundle"
  HOST_BUNDLE=$WORK/host-bundle
else
  say "no cargo-zigbuild: this build has no Linux vibed for Add Linux Host"
fi

# The Swift products, each in a build directory of its own: Sparkle comes
# in only with VIBEOS_SPARKLE=1, and switching that in one directory would
# rebuild everything.
SWIFT_APP=$WORK/swift-app
if $SPARKLE; then SWIFT_APP=$WORK/swift-app-sparkle; fi
VIBEOS_SPARKLE=$($SPARKLE && echo 1 || echo 0) \
  swift build --package-path swift/VibeOS -c release --product VibeOS \
  --scratch-path "$SWIFT_APP" --manifest-cache none
app_bin_dir=$(VIBEOS_SPARKLE=$($SPARKLE && echo 1 || echo 0) \
  swift build --package-path swift/VibeOS -c release --scratch-path "$SWIFT_APP" --manifest-cache none \
  --show-bin-path)
swift build --package-path swift/vibe-vm -c release --product vibe-vm --scratch-path "$WORK/swift-vm"
vm_bin=$(swift build --package-path swift/vibe-vm -c release --scratch-path "$WORK/swift-vm" --show-bin-path)/vibe-vm

# The bundle.
APP=$WORK/stage/VibeOS.app
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$APP/Contents/Library/LaunchAgents"
cp swift/VibeOS/Support/Info.plist "$APP/Contents/Info.plist"
plist() { plutil "$@" "$APP/Contents/Info.plist"; }
plist -replace CFBundleShortVersionString -string "$VERSION"
# Sparkle compares builds by CFBundleVersion: the version itself, which
# only goes up.
plist -replace CFBundleVersion -string "$VERSION"
cp "$app_bin_dir/VibeOS" "$APP/Contents/MacOS/VibeOS"
for b in vibed vibe vibe-shim; do cp "$RUST/$b" "$APP/Contents/MacOS/$b"; done
cp "$vm_bin" "$APP/Contents/MacOS/vibe-vm"
cp "swift/VibeOS/Support/$LABEL.plist" "$APP/Contents/Library/LaunchAgents/$LABEL.plist"
icns=$WORK/AppIcon.icns
rm -rf "$WORK/AppIcon.iconset"
python3 scripts/app-icon.py "$WORK/AppIcon.iconset"
iconutil -c icns "$WORK/AppIcon.iconset" -o "$icns"
cp "$icns" "$APP/Contents/Resources/AppIcon.icns"
if [ -n "$HOST_BUNDLE" ]; then cp -R "$HOST_BUNDLE" "$APP/Contents/Resources/linux"; fi
if $SPARKLE; then
  [ -d "$app_bin_dir/Sparkle.framework" ] || die "swift build left no Sparkle.framework in $app_bin_dir"
  mkdir -p "$APP/Contents/Frameworks"
  ditto "$app_bin_dir/Sparkle.framework" "$APP/Contents/Frameworks/Sparkle.framework"
  public=$(python3 -I scripts/sparkle.py public-key)
  plist -insert SUFeedURL -string "https://github.com/$REPO/releases/latest/download/appcast.xml"
  plist -insert SUPublicEDKey -string "$public"
  plist -insert SUEnableAutomaticChecks -bool YES
fi
plist -lint

# Signing, from the inside out: each nested program, then the bundle.
if [ -n "$IDENTITY" ]; then
  SIGN=(codesign --force --sign "$IDENTITY" --timestamp --options runtime)
else
  # Ad hoc, without the hardened runtime: it would refuse an ad hoc
  # Sparkle.framework, which has no team to match.
  SIGN=(codesign --force --sign - --timestamp=none)
fi
if $SPARKLE; then
  # As Sparkle's documentation has it: its helpers first, the Downloader
  # keeping its entitlements, then the framework.
  fw=$APP/Contents/Frameworks/Sparkle.framework/Versions/B
  for part in XPCServices/Installer.xpc XPCServices/Downloader.xpc Autoupdate Updater.app; do
    [ -e "$fw/$part" ] || continue
    if [ "$part" = XPCServices/Downloader.xpc ]; then
      "${SIGN[@]}" --preserve-metadata=entitlements "$fw/$part"
    else
      "${SIGN[@]}" "$fw/$part"
    fi
  done
  "${SIGN[@]}" "$APP/Contents/Frameworks/Sparkle.framework"
fi
# The Linux builds in Resources/linux are data to macOS, sealed with the
# bundle's resources.
for b in vibed vibe vibe-shim; do "${SIGN[@]}" "$APP/Contents/MacOS/$b"; done
"${SIGN[@]}" --entitlements swift/vibe-vm/vibe-vm.entitlements "$APP/Contents/MacOS/vibe-vm"
"${SIGN[@]}" --entitlements swift/VibeOS/Support/VibeOS.entitlements "$APP"
codesign --verify --strict --deep --verbose=2 "$APP"
for b in VibeOS vibed vibe vibe-shim vibe-vm; do
  case " $(lipo -archs "$APP/Contents/MacOS/$b") " in
    *" arm64 "*) ;;
    *) die "$b isn't built for Apple Silicon" ;;
  esac
done
case $(codesign -d --entitlements - --xml "$APP/Contents/MacOS/vibe-vm" 2>/dev/null) in
  *com.apple.security.virtualization*) ;;
  *) die "vibe-vm lost its virtualization entitlement" ;;
esac

# notary <notarytool command> <args>…: notarytool, with the credentials.
notary() {
  local cmd=$1
  shift
  if [ "$NOTARY" = api-key ]; then
    [ -f "$PRIVATE/notary.p8" ] || (umask 077; printf '%s\n' "$NOTARY_KEY_P8" >"$PRIVATE/notary.p8")
    xcrun notarytool "$cmd" "$@" --key "$PRIVATE/notary.p8" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER_ID"
  else
    xcrun notarytool "$cmd" "$@" --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" --password "$APPLE_APP_PASSWORD"
  fi
}

# json_field <field>: that field of the JSON on stdin, or nothing.
json_field() {
  python3 -c 'import json,sys
try: print(json.load(sys.stdin).get(sys.argv[1], ""))
except Exception: pass' "$1"
}

# notarize <file>: submits it and waits; fails unless Apple accepts it,
# with Apple's log of why.
notarize() {
  local result status id
  result=$(notary submit "$1" --wait --output-format json) || true
  status=$(printf '%s' "$result" | json_field status)
  id=$(printf '%s' "$result" | json_field id)
  say "notarization of $(basename "$1"): ${status:-no answer}${id:+ (submission $id)}"
  if [ "$status" != Accepted ]; then
    [ -z "$id" ] || notary log "$id" >&2 || true
    die "Apple didn't accept $(basename "$1")"
  fi
}

if $NOTARIZE; then
  ditto -c -k --keepParent "$APP" "$WORK/VibeOS.zip"
  notarize "$WORK/VibeOS.zip"
  xcrun stapler staple "$APP"
  rm -f "$WORK/VibeOS.zip"
fi

# The DMG: the app, and Applications to drag it to.
DMG=$OUT/VibeOS-$VERSION.dmg
mkdir -p "$WORK/stage/dmg"
mv "$APP" "$WORK/stage/dmg/VibeOS.app"
ln -s /Applications "$WORK/stage/dmg/Applications"
hdiutil create -volname "VibeOS" -srcfolder "$WORK/stage/dmg" -ov -format UDZO "$DMG" >/dev/null
if [ -n "$IDENTITY" ]; then
  codesign --force --sign "$IDENTITY" --timestamp "$DMG"
fi
if $NOTARIZE; then
  notarize "$DMG"
  xcrun stapler staple "$DMG"
  spctl --assess --type open --context context:primary-signature -v "$DMG"
fi

# Sparkle's appcast: this release, signed.
if $SPARKLE; then
  signature=$(python3 -I scripts/sparkle.py sign "$DMG")
  python3 -I scripts/sparkle.py verify "$DMG" "$signature" "$public" || die "the DMG's signature doesn't verify"
  python3 -I scripts/sparkle.py appcast --version "$VERSION" \
    --url "https://github.com/$REPO/releases/download/v$VERSION/VibeOS-$VERSION.dmg" \
    --file "$DMG" --signature "$signature" --minimum-system 26.0 \
    --release-page "https://github.com/$REPO/releases/tag/v$VERSION" >"$OUT/appcast.xml"
fi

(cd "$OUT" && shasum -a 256 ./*.dmg $([ -f appcast.xml ] && echo appcast.xml) >SHA256SUMS)
say "done: $(ls "$OUT" | tr '\n' ' ')"
