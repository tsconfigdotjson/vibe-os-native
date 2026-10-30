#!/usr/bin/env bash
# Builds the small Linux guest that tests vibe-vm end to end
# (crates/vibe-env/tests/vm_cycle.rs) into build/test-guest/, or OUT:
#
#   Image    an uncompressed arm64 kernel, as Virtualization.framework boots
#   initrd   Alpine's netboot initramfs, plus our /init and vibe-testguest
#   modloop  Alpine's squashfs of kernel modules, attached as a read-only
#            disk: it has the vsock drivers the initramfs lacks
#
# The kernel, initramfs and modloop are Alpine 3.24.3's netboot files for
# aarch64, pinned by checksum and kept in OUT/cache/. Works on macOS and
# Linux; needs curl, cpio, gzip and rustup.
#
#   scripts/test-guest.sh [OUT]

set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

OUT=${1:-build/test-guest}
CACHE="$OUT/cache"
ALPINE=https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/aarch64/netboot-3.24.3
TARGET=aarch64-unknown-linux-musl

mkdir -p "$CACHE"

sha256() {
  if command -v sha256sum >/dev/null; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# fetch <name> <sha256>: downloads an Alpine netboot file into the cache.
fetch() {
  local file="$CACHE/$1"
  if [ -f "$file" ] && [ "$(sha256 "$file")" = "$2" ]; then
    return
  fi
  curl -fsSL --retry 3 -o "$file.part" "$ALPINE/$1"
  local got
  got=$(sha256 "$file.part")
  if [ "$got" != "$2" ]; then
    echo "test-guest.sh: $1 has checksum $got, expected $2" >&2
    rm -f "$file.part"
    exit 1
  fi
  mv "$file.part" "$file"
}

fetch vmlinuz-virt 849394914e6a93cc9bb17e901f445ce21fcd7e4528541d9496a74d6a6ccee319
fetch initramfs-virt 7b75e0475ce73eca283ab60eac28f2fe1a8b62e8ffd870c056baaafc9d6e9b1b
fetch modloop-virt 5252eb841fdf753d75562c4993ef55859aef828922159a64a299f6ee73f53403

# The kernel is an EFI zboot image: a PE stub around a gzipped Image, whose
# offset and size are at bytes 8 and 12. Virtualization.framework boots the
# Image itself.
kernel="$CACHE/vmlinuz-virt"
if [ "$(dd if="$kernel" bs=1 skip=4 count=4 2>/dev/null)" != zimg ]; then
  echo "test-guest.sh: $kernel isn't an EFI zboot image" >&2
  exit 1
fi
offset=$(od -An -t u4 -j 8 -N 4 "$kernel" | tr -d ' ')
size=$(od -An -t u4 -j 12 -N 4 "$kernel" | tr -d ' ')
head -c $((offset + size)) "$kernel" | tail -c "$size" | gunzip >"$OUT/Image.part"
# An arm64 Image says so at byte 56.
if [ "$(dd if="$OUT/Image.part" bs=1 skip=56 count=3 2>/dev/null)" != ARM ]; then
  echo "test-guest.sh: what came out of $kernel isn't an arm64 Image" >&2
  exit 1
fi
mv "$OUT/Image.part" "$OUT/Image"

rustup target add "$TARGET" >/dev/null
cargo build -p vibe-testguest --locked --release --target "$TARGET"

# Our files go in a second archive after Alpine's: the kernel unpacks both,
# and the later /init wins.
root="$OUT/root"
rm -rf "$root"
mkdir -p "$root"
cp "target/$TARGET/release/vibe-testguest" "$root/vibe-testguest"
cat >"$root/init" <<'INIT'
#!/bin/sh
# PID 1 of the vibe-vm test guest until it execs vibe-testguest. It runs on
# Alpine's netboot initramfs and uses its busybox and kernel modules.
/bin/busybox mkdir -p /proc /sys /dev /media/modloop /mnt/share /usr/bin /usr/sbin
/bin/busybox --install -s
export PATH=/usr/bin:/usr/sbin:/bin:/sbin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
exec >/dev/console 2>&1
echo "test-guest: starting"
kv=$(uname -r)
k=/usr/lib/modules/$kv/kernel
for m in drivers/block/virtio_blk net/core/failover drivers/net/net_failover \
  drivers/net/virtio_net drivers/virtio/virtio_balloon drivers/char/hw_random/rng-core \
  drivers/char/hw_random/virtio-rng fs/fuse/fuse fs/fuse/virtiofs fs/squashfs/squashfs; do
  insmod "$k/$m.ko" || echo "test-guest: can't load $m"
done
# The rest of the modules, vsock's among them, are on the disk whose
# serial is `modloop`.
modloop=
for _ in $(seq 50); do
  for b in /sys/block/vd*; do
    [ "$(cat "$b/serial" 2>/dev/null)" = modloop ] && modloop=/dev/${b##*/}
  done
  [ -n "$modloop" ] && [ -b "$modloop" ] && break
  sleep 0.1
done
mount -t squashfs -o ro "$modloop" /media/modloop || echo "test-guest: no modloop disk"
v=/media/modloop/modules/$kv/kernel/net/vmw_vsock
for m in vsock vmw_vsock_virtio_transport_common vmw_vsock_virtio_transport; do
  insmod "$v/$m.ko" || echo "test-guest: can't load $m"
done
mount -t virtiofs share /mnt/share || echo "test-guest: no share"
ip link set eth0 up || echo "test-guest: no eth0"
echo "test-guest: exec vibe-testguest"
exec /vibe-testguest
INIT
chmod 755 "$root/init" "$root/vibe-testguest"
(cd "$root" && find . -mindepth 1 | LC_ALL=C sort | cpio -o -H newc -R 0:0 2>/dev/null) |
  gzip -9 >"$OUT/overlay.cpio.gz"
cat "$CACHE/initramfs-virt" "$OUT/overlay.cpio.gz" >"$OUT/initrd"
rm -rf "$root" "$OUT/overlay.cpio.gz"
cp "$CACHE/modloop-virt" "$OUT/modloop"

echo "test guest in $OUT: Image, initrd, modloop"
