#!/usr/bin/env bash
# The Linux builds a Mac installs on a Linux host (PRD: Adding a Linux host):
# vibed, vibe and vibe-shim, in <out>/<arch>/, which is what vibed reads from
# VIBE_HOST_BUNDLE (or from linux/ beside it, or the app's Resources/linux).
#
#   scripts/host-bundle.sh [--out DIR] [--target x86_64|aarch64]... [--native]
#
# By default it builds both CPUs, statically linked against musl, so they
# run on any Linux server. SQLite is compiled in, which needs a C compiler
# for the target: this uses cargo-zigbuild, with zig as that compiler, on a
# Mac or on Linux, fetched on first use if you haven't got them
# (scripts/zig-tools.sh).
#
# --native builds for this Linux machine's own CPU with its own toolchain:
# dynamically linked, for a server like this one, and quick, for tests.
set -euo pipefail
cd "$(dirname "$0")/.."
OUT=build/host-bundle
TARGETS=()
NATIVE=false
while [ $# -gt 0 ]; do
  case $1 in
    --out) OUT=$2; shift 2 ;;
    --target) TARGETS+=("$2"); shift 2 ;;
    --native) NATIVE=true; shift ;;
    -h | --help) sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "host-bundle: unexpected $1" >&2; exit 2 ;;
  esac
done
BINS=(vibed vibe vibe-shim)
TARGET_DIR=${CARGO_TARGET_DIR:-target}
PACKAGES=(-p vibed -p vibe-cli -p vibe-shim)

place() { # place <arch> <dir the binaries are in>
  mkdir -p "$OUT/$1"
  for b in "${BINS[@]}"; do
    cp "$2/$b" "$OUT/$1/$b.new"
    mv -f "$OUT/$1/$b.new" "$OUT/$1/$b"
  done
  echo "$OUT/$1: ${BINS[*]}"
}

if $NATIVE; then
  [ "$(uname -s)" = Linux ] || { echo "host-bundle: --native builds on Linux only" >&2; exit 2; }
  arch=$(uname -m)
  case $arch in amd64) arch=x86_64 ;; arm64) arch=aarch64 ;; esac
  cargo build --locked --release "${PACKAGES[@]}"
  place "$arch" "$TARGET_DIR/release"
  exit 0
fi

[ ${#TARGETS[@]} -gt 0 ] || TARGETS=(x86_64 aarch64)
. scripts/zig-tools.sh
ensure_zigbuild || {
  echo "host-bundle: can't get zig and cargo-zigbuild, which it needs for this; on Linux, --native needs neither" >&2
  exit 1
}
for arch in "${TARGETS[@]}"; do
  case $arch in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) echo "host-bundle: vibed is built for x86_64 and aarch64, not $arch" >&2; exit 2 ;;
  esac
  target=$arch-unknown-linux-musl
  rustup target add "$target"
  cargo zigbuild --locked --release --target "$target" "${PACKAGES[@]}"
  for b in "${BINS[@]}"; do
    info=$(file "$TARGET_DIR/$target/release/$b")
    case $info in *"statically linked"* | *"static-pie linked"*) ;; *) echo "$b isn't static: $info" >&2; exit 1 ;; esac
  done
  place "$arch" "$TARGET_DIR/$target/release"
done
