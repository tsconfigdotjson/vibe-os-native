#!/usr/bin/env bash
# Builds base release N of the Linux image every linux agent boots (PRD:
# What's inside linux), into OUT/base-N/ (default build/base-image/base-2/):
#
#   Image        an arm64 kernel (Linux 6.18 LTS) with everything vibe-guest
#                needs built in: scripts/base-image/kernel.config
#   initrd       vibe-guest as /init, a static mke2fs in /sbin, and the
#                /dev/console and /dev/null nodes PID 1 starts with
#   base.img.gz  the root filesystem as a read-only ext4 image: Debian 13
#                slim with git, gh, curl, ripgrep, jq, build-essential,
#                Python 3, Node LTS and mise (scripts/base-image/Dockerfile)
#   base.json    the manifest: each file's size and SHA-256, which vibed
#                checks every download against
#
# OUT is then a release directory vibed can install from:
#
#   VIBE_BASE_RELEASES=file://$PWD/build/base-image vibed
#
# and what gets published, as base-N/, for an app release to download.
#
# Needs docker (or podman, as DOCKER=podman) that runs linux/arm64
# containers natively: Docker Desktop on Apple silicon, or an arm64 Linux
# host. The kernel and mke2fs are built in a container of this machine's
# own architecture, cross-compiled where that isn't arm64, and kept in
# OUT/cache/ by version. Also needs rustup, for vibe-guest. No VM runs.
#
#   scripts/base-image.sh [OUT]

set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

N=${BASE_RELEASE:-2}
OUT=${1:-build/base-image}
DOCKER=${DOCKER:-docker}
TARGET=aarch64-unknown-linux-musl

KERNEL_VERSION=6.18.60
KERNEL_SHA256=3c2f44ef6acc021a7ad444d9a2e6ec1ef488ff09368793d0004a6080f69d621e
E2FS_VERSION=1.47.4
E2FS_SHA256=fd5bf388cbdbe006a3d3b318d983b2948382440acc85a87f1e7d108653e8db0b

say() { echo "base-image.sh: $*" >&2; }

command -v "$DOCKER" >/dev/null || {
  say "needs $DOCKER: install Docker Desktop (or set DOCKER=podman)"
  exit 1
}

mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
CACHE="$OUT/cache"
WORK="$OUT/work"
DEST="$OUT/base-$N"
rm -rf "$WORK" "$DEST.partial"
mkdir -p "$CACHE" "$WORK" "$DEST.partial"
trap 'rm -rf "$WORK"' EXIT

sha256() {
  if command -v sha256sum >/dev/null; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# 1. vibe-guest, PID 1: a static arm64 binary.
rustup target add "$TARGET" >/dev/null
cargo build -p vibe-guest --locked --release --target "$TARGET"
cp "target/$TARGET/release/vibe-guest" "$WORK/vibe-guest"

# 2. The kernel and mke2fs, in a builder of this machine's architecture.
case $(uname -m) in
  arm64 | aarch64) CROSS= ;;
  *) CROSS=aarch64-linux-gnu- ;;
esac
say "making the builder (${CROSS:-native} arm64 toolchain)"
"$DOCKER" build -q -t vibeos-base-builder - >/dev/null <<DOCKERFILE
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential bc bison flex libssl-dev libelf-dev cpio xz-utils gzip \
      curl ca-certificates e2fsprogs file python3 \
      ${CROSS:+gcc-aarch64-linux-gnu libc6-dev-arm64-cross} \
 && rm -rf /var/lib/apt/lists/*
DOCKERFILE

fragment_sum=$(sha256 scripts/base-image/kernel.config | cut -c1-12)
KERNEL_CACHE="$CACHE/kernel-$KERNEL_VERSION-$fragment_sum"
if [ ! -f "$KERNEL_CACHE/Image" ] || [ ! -f "$KERNEL_CACHE/gen_init_cpio" ]; then
  say "building Linux $KERNEL_VERSION"
  mkdir -p "$KERNEL_CACHE.partial"
  cp scripts/base-image/kernel.config "$KERNEL_CACHE.partial/fragment"
  "$DOCKER" run --rm -v "$KERNEL_CACHE.partial:/out" -e CROSS="$CROSS" \
    -e V="$KERNEL_VERSION" -e SUM="$KERNEL_SHA256" vibeos-base-builder bash -euo pipefail -c '
      cd /tmp
      curl -fsSLo linux.tar.xz "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$V.tar.xz"
      echo "$SUM  linux.tar.xz" | sha256sum -c -
      tar -xJf linux.tar.xz && rm linux.tar.xz && cd "linux-$V"
      make() { command make ARCH=arm64 CROSS_COMPILE="$CROSS" "$@"; }
      make allnoconfig >/dev/null
      scripts/kconfig/merge_config.sh -m .config /out/fragment >/dev/null
      make olddefconfig >/dev/null
      # Every option asked for holds, or the kernel would boot without it.
      missing=0
      while IFS= read -r line; do
        case $line in CONFIG_*=*) ;; *) continue ;; esac
        grep -qx "$line" .config || { echo "kernel.config: $line did not hold"; missing=1; }
      done </out/fragment
      [ "$missing" = 0 ]
      grep -q "^CONFIG_MODULES=y" .config && { echo "modules are on"; exit 1; }
      make -j"$(nproc)" Image
      cp arch/arm64/boot/Image .config /out/
      gcc -O2 -o /out/gen_init_cpio usr/gen_init_cpio.c
    '
  rm -rf "$KERNEL_CACHE"
  mv "$KERNEL_CACHE.partial" "$KERNEL_CACHE"
fi
if [ "$(dd if="$KERNEL_CACHE/Image" bs=1 skip=56 count=3 2>/dev/null)" != ARM ]; then
  say "$KERNEL_CACHE/Image isn't an arm64 Image"
  exit 1
fi

E2FS_CACHE="$CACHE/mke2fs-$E2FS_VERSION"
if [ ! -f "$E2FS_CACHE/mke2fs" ]; then
  say "building a static mke2fs $E2FS_VERSION"
  mkdir -p "$E2FS_CACHE.partial"
  "$DOCKER" run --rm -v "$E2FS_CACHE.partial:/out" -e CROSS="$CROSS" \
    -e V="$E2FS_VERSION" -e SUM="$E2FS_SHA256" vibeos-base-builder bash -euo pipefail -c '
      cd /tmp
      curl -fsSLo e2fs.tar.xz "https://mirrors.edge.kernel.org/pub/linux/kernel/people/tytso/e2fsprogs/v$V/e2fsprogs-$V.tar.xz"
      echo "$SUM  e2fs.tar.xz" | sha256sum -c -
      tar -xJf e2fs.tar.xz && cd "e2fsprogs-$V"
      host=; [ -n "$CROSS" ] && host=--host=aarch64-linux-gnu
      CC="${CROSS}gcc" LDFLAGS=-static ./configure $host --disable-nls --disable-fuse2fs \
        --disable-defrag --disable-imager --disable-resizer --disable-debugfs \
        --disable-e2initrd-helper --disable-tdb --disable-uuidd >/dev/null
      make -j"$(nproc)" libs >/dev/null
      make -j"$(nproc)" -C misc mke2fs >/dev/null
      cp misc/mke2fs /out/
      file /out/mke2fs | tee /dev/stderr | grep -q "ARM aarch64.*statically linked"
    '
  rm -rf "$E2FS_CACHE"
  mv "$E2FS_CACHE.partial" "$E2FS_CACHE"
fi

# 3. The initrd: vibe-guest as /init, and what it needs before the switch.
cp "$E2FS_CACHE/mke2fs" "$WORK/mke2fs"
cp "$KERNEL_CACHE/gen_init_cpio" "$WORK/gen_init_cpio"
cat >"$WORK/initrd.list" <<'LIST'
dir /dev 0755 0 0
nod /dev/console 0600 0 0 c 5 1
nod /dev/null 0666 0 0 c 1 3
dir /proc 0755 0 0
dir /sys 0755 0 0
dir /sbin 0755 0 0
file /sbin/mke2fs /work/mke2fs 0755 0 0
file /init /work/vibe-guest 0755 0 0
LIST

# 4. The root filesystem, for linux/arm64, exported and made an ext4 image.
say "building the root filesystem"
"$DOCKER" build --platform linux/arm64 -t "vibeos-base-rootfs:$N" scripts/base-image
container=$("$DOCKER" create --platform linux/arm64 "vibeos-base-rootfs:$N" /bin/true)
"$DOCKER" export "$container" -o "$WORK/rootfs.tar"
"$DOCKER" rm "$container" >/dev/null
versions=$("$DOCKER" run --rm --platform linux/arm64 "vibeos-base-rootfs:$N" sh -c \
  'echo "debian $(cat /etc/debian_version); $(git --version); gh $(gh --version | head -n1 | cut -d" " -f3); node $(node --version); mise $(mise --version | cut -d" " -f1); $(python3 --version)"')

# The toolbox's sudo works in this root: vibe set up as vibe-guest sets it
# up (its passwd, group and shadow entries, and the toolbox's sudo rule,
# read from crates/vibe-guest/src/init/root.rs), sudo runs as root for it
# with no password.
rule=$(sed -n 's/^pub(crate) const SUDO_RULE: &str = "\(.*\)";$/\1/p' crates/vibe-guest/src/init/root.rs)
shadow=$(sed -n 's/^pub(crate) const SHADOW_ENTRY: &str = "\(.*\)";$/\1/p' crates/vibe-guest/src/init/root.rs)
[ -n "$rule" ] && [ -n "$shadow" ] || { say "can't find SUDO_RULE and SHADOW_ENTRY in vibe-guest"; exit 1; }
say "checking sudo for vibe in the toolbox"
"$DOCKER" run --rm --platform linux/arm64 -e RULE="$rule" -e SHADOW="$shadow" \
  "vibeos-base-rootfs:$N" bash -euo pipefail -c '
  echo "vibe:x:501:20:vibe:/home/vibe:/bin/bash" >>/etc/passwd
  grep -q "^vibe:" /etc/group || echo "vibe:x:20:" >>/etc/group
  echo "$SHADOW" >>/etc/shadow
  echo "$RULE" >/etc/sudoers.d/vibe-toolbox
  chmod 440 /etc/sudoers.d/vibe-toolbox
  [ "$(setpriv --reuid=501 --regid=20 --clear-groups sudo -n id -u)" = 0 ]
' || { say "sudo doesn't work for vibe as vibe-guest sets it up"; exit 1; }

say "making the images"
"$DOCKER" run --rm -v "$WORK:/work" vibeos-base-builder bash -euo pipefail -c '
  cd /work
  ./gen_init_cpio initrd.list | gzip -9 >initrd
  # Unpacked inside the container, never in /work: on a Mac, /work is the
  # host directory through Docker Desktop'"'"'s file sharing, which keeps
  # neither root ownership nor setuid bits, and mke2fs copies what it sees
  # (sudo then refuses to run).
  rootfs=/rootfs
  mkdir "$rootfs"
  tar -xpf rootfs.tar -C "$rootfs" --numeric-owner --same-owner
  rm rootfs.tar
  # What docker leaves to the runtime, and vibe-guest fills in at boot.
  echo vibe >"$rootfs/etc/hostname"
  printf "127.0.0.1\tlocalhost\n::1\tlocalhost\n127.0.1.1\tvibe\n" >"$rootfs/etc/hosts"
  : >"$rootfs/etc/resolv.conf"
  rm -f "$rootfs/.dockerenv"
  used=$(du -sk --apparent-size "$rootfs" | cut -f1)
  inodes=$(find "$rootfs" | wc -l)
  # A quarter more than it holds, for ext4'"'"'s own, and nothing reserved:
  # nobody writes to it.
  size=$(( (used + used / 4 + 65536) / 1024 ))M
  mke2fs -q -t ext4 -b 4096 -m 0 -O ^has_journal -N $(( inodes + inodes / 4 + 1024 )) \
    -L base -d "$rootfs" base.img "$size"
  e2fsck -fn base.img
  rm -rf "$rootfs"
  # The image keeps owners and modes: sudo is root'"'"'s and setuid.
  check() {
    debugfs -R "stat $1" base.img 2>/dev/null >stat.txt
    grep -Eq "Mode: +0*$2 " stat.txt && grep -Eq "^User: +0 +Group: +0 " stat.txt || {
      echo "base.img: $1 isn'"'"'t mode $2, owned by root:" >&2
      head -3 stat.txt >&2
      exit 1
    }
  }
  check /usr/bin/sudo 4755
  check /etc 755
  rm -f stat.txt
  gzip -6 base.img
'

mv "$WORK/initrd" "$WORK/base.img.gz" "$DEST.partial/"
cp "$KERNEL_CACHE/Image" "$DEST.partial/Image"
cp "$KERNEL_CACHE/.config" "$DEST.partial/kernel.config"

file_entry() {
  printf '"%s": {"sha256": "%s", "size": %s}' "$1" "$(sha256 "$DEST.partial/$1")" \
    "$(wc -c <"$DEST.partial/$1" | tr -d ' ')"
}
cat >"$DEST.partial/base.json" <<JSON
{
  "version": $N,
  "built": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "kernel": "$KERNEL_VERSION",
  "contents": "$versions",
  "files": {
    $(file_entry Image),
    $(file_entry initrd),
    $(file_entry base.img.gz)
  }
}
JSON
rm -rf "$DEST"
mv "$DEST.partial" "$DEST"
say "base release $N in $DEST: $versions"
