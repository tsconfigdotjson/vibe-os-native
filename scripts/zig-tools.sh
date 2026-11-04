# Sourced, for what builds for Linux from anywhere. ensure_zigbuild puts zig and
# cargo-zigbuild on PATH: yours if you have both, or otherwise these exact versions,
# which it fetches once into a cache shared by every checkout (VIBE_TOOLS_DIR, or
# vibeos/tools under ~/.cache). The first time takes the network and a few minutes.
# Nothing to install by hand, and no package manager: zig's official tarball, checked
# against the checksum here, and cargo-zigbuild built by cargo from crates.io, at this
# exact version with its lockfile, which rests on crates.io's own checksums.
#
# They're the versions the last release was built with. To move them, the checksums
# are in https://ziglang.org/download/index.json.

ZIG_VERSION=0.17.0
CARGO_ZIGBUILD_VERSION=0.24.1

# zig_checksum <arch>-<os>: the SHA-256 of that tarball.
zig_checksum() {
  case $1 in
    aarch64-macos) echo b607e9b9234790a008116ae5bdb71c6243b84b9fb42a53a9e70fde41c06c536a ;;
    x86_64-macos) echo 4f9a1c5269aa17ebda5e6d3c2b89d6cbf36f7d2b22a0306e9ab98f25f95529c6 ;;
    x86_64-linux) echo 1cbe9df9f27e6b78d14ccbca43b6703a404ef79ef1c463de901d7f088d4e2026 ;;
    aarch64-linux) echo 9e8d11661d4ae3bd57702a3832781e23ad151dde5798e16a5ccd503f65234ff8 ;;
    *) return 1 ;;
  esac
}

# zig_platform: this machine, as zig names it.
zig_platform() {
  local arch os
  case $(uname -m) in
    arm64 | aarch64) arch=aarch64 ;;
    x86_64 | amd64) arch=x86_64 ;;
    *) arch=$(uname -m) ;;
  esac
  case $(uname -s) in
    Darwin) os=macos ;;
    Linux) os=linux ;;
    *) os=$(uname -s) ;;
  esac
  echo "$arch-$os"
}

# sha256_of <file>: its SHA-256, by whichever tool this machine has.
sha256_of() {
  if command -v sha256sum >/dev/null; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# fetch_zig <dir>: puts zig in <dir>, whole or not at all. A tarball that isn't
# the one named here is never unpacked.
fetch_zig() {
  local dir=$1 platform want tarball tmp got
  platform=$(zig_platform)
  want=$(zig_checksum "$platform") || {
    echo "zig-tools: no zig named here for $platform" >&2
    return 1
  }
  tarball="zig-$platform-$ZIG_VERSION.tar.xz"
  mkdir -p "$(dirname "$dir")"
  tmp=$(mktemp -d "$dir.XXXXXX") || return 1
  echo "zig-tools: fetching zig $ZIG_VERSION, once" >&2
  if ! curl -fsSL --retry 3 --connect-timeout 30 --max-time 900 -o "$tmp/$tarball" \
    "https://ziglang.org/download/$ZIG_VERSION/$tarball"; then
    echo "zig-tools: can't download zig from ziglang.org" >&2
    rm -rf "$tmp"
    return 1
  fi
  got=$(sha256_of "$tmp/$tarball")
  if [ "$got" != "$want" ]; then
    echo "zig-tools: $tarball isn't the expected one: its SHA-256 is $got, not $want" >&2
    rm -rf "$tmp"
    return 1
  fi
  mkdir "$tmp/unpacked"
  if ! tar -xJf "$tmp/$tarball" -C "$tmp/unpacked" --strip-components=1 ||
    ! [ -x "$tmp/unpacked/zig" ]; then
    echo "zig-tools: can't unpack $tarball" >&2
    rm -rf "$tmp"
    return 1
  fi
  # Another run may have got there first, and its copy will do. mv into a
  # directory that exists would put this one inside it, so it goes if it did.
  [ -e "$dir" ] || mv "$tmp/unpacked" "$dir" 2>/dev/null
  rm -rf "$tmp" "$dir/unpacked"
  [ -x "$dir/zig" ] || {
    echo "zig-tools: $dir is there, and has no zig in it: remove it to fetch it again" >&2
    return 1
  }
}

ensure_zigbuild() {
  if command -v cargo-zigbuild >/dev/null && command -v zig >/dev/null; then
    return 0
  fi
  local tools=${VIBE_TOOLS_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/vibeos/tools}
  local zig_dir="$tools/zig-$ZIG_VERSION"
  local zigbuild_root="$tools/cargo-zigbuild-$CARGO_ZIGBUILD_VERSION"
  [ -x "$zig_dir/zig" ] || fetch_zig "$zig_dir" || return 1
  if ! [ -x "$zigbuild_root/bin/cargo-zigbuild" ]; then
    echo "zig-tools: building cargo-zigbuild $CARGO_ZIGBUILD_VERSION, once" >&2
    cargo install cargo-zigbuild --locked --version "$CARGO_ZIGBUILD_VERSION" \
      --root "$zigbuild_root" --quiet || {
      echo "zig-tools: can't build cargo-zigbuild" >&2
      return 1
    }
  fi
  export PATH="$zig_dir:$zigbuild_root/bin:$PATH"
}
