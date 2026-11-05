#!/usr/bin/env bash
# Builds the small Linux guests that test vibe-vm and vibe-guest end to end
# (crates/vibe-env/tests/vm_cycle.rs) into build/test-guest/, or OUT:
#
#   Image         an uncompressed arm64 kernel, as Virtualization.framework
#                 boots
#   initrd        Alpine's netboot initramfs, plus an /init that loads the
#                 drivers and runs vibe-testguest as PID 1
#   initrd-guest  the same, but its /init runs vibe-guest as PID 1, with
#                 vibe-testguest aboard as the command it starts, and the
#                 libraries Claude's musl build needs (libgcc, libstdc++), for
#                 the test that runs the real Claude from a share
#   modloop       Alpine's squashfs of kernel modules, attached as a
#                 read-only disk: it has the vsock drivers the initramfs
#                 lacks
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
  curl -fsSL --retry 3 -o "$file.part" "$ALPINE/$1" || return 1
  local got
  got=$(sha256 "$file.part")
  if [ "$got" != "$2" ]; then
    echo "test-guest.sh: $1 has checksum $got, expected $2" >&2
    rm -f "$file.part"
    return 1
  fi
  mv "$file.part" "$file"
}

# Claude's libraries, from Alpine's package repository, which keeps only
# the latest build of each: if these are gone, the guest goes without, and
# only the opt-in test with the real Claude fails, saying why.
MAIN=https://dl-cdn.alpinelinux.org/alpine/v3.24/main/aarch64
fetch_lib() {
  ALPINE=$MAIN fetch "$@" ||
    echo "test-guest.sh: no $1; the real-Claude test won't run on this guest" >&2
}

fetch vmlinuz-virt 849394914e6a93cc9bb17e901f445ce21fcd7e4528541d9496a74d6a6ccee319 || exit 1
fetch initramfs-virt 7b75e0475ce73eca283ab60eac28f2fe1a8b62e8ffd870c056baaafc9d6e9b1b || exit 1
fetch modloop-virt 5252eb841fdf753d75562c4993ef55859aef828922159a64a299f6ee73f53403 || exit 1
fetch_lib libgcc-15.2.0-r5.apk 369aaa6e9d099a737bad6dd3e6c2fe7bb1547ca26d22b94ee0411228f709b403
fetch_lib libstdc++-15.2.0-r5.apk 2302e766d4e4926038ec166ecb85837ee884576115236ddb565e3a5fca4a11d7

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
cargo build -p vibe-testguest -p vibe-guest --locked --release --target "$TARGET"
RUST=${CARGO_TARGET_DIR:-target}/$TARGET/release

# Loads the drivers both guests need, on Alpine's netboot initramfs with its
# busybox: from the initramfs, then from the disk whose serial is `modloop`.
read -r -d '' DRIVERS <<'DRIVERS' || true
/bin/busybox mkdir -p /proc /sys /dev /media/modloop /usr/bin /usr/sbin
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
DRIVERS

# initrd <name> <init>: Alpine's initramfs with a second archive after it,
# holding our /init (the drivers, then the file <init>) and both binaries.
# The kernel unpacks both, and the later /init wins.
initrd() {
  local root="$OUT/root"
  rm -rf "$root"
  mkdir -p "$root"
  cp "$RUST/vibe-testguest" "$RUST/vibe-guest" "$root/"
  if [ "$1" = initrd-guest ]; then
    mkdir -p "$root/usr/lib"
    for apk in "$CACHE"/libgcc-*.apk "$CACHE"/libstdc++-*.apk; do
      [ -f "$apk" ] || continue
      tar -xzf "$apk" -C "$root" usr/lib 2>/dev/null || echo "test-guest.sh: can't unpack $apk" >&2
    done
  fi
  { printf '#!/bin/sh\n%s\n' "$DRIVERS"; cat "$2"; } >"$root/init"
  chmod 755 "$root/init" "$root/vibe-testguest" "$root/vibe-guest"
  (cd "$root" && find . -mindepth 1 | LC_ALL=C sort | cpio -o -H newc -R 0:0 2>/dev/null) |
    gzip -9 >"$OUT/overlay.cpio.gz"
  cat "$CACHE/initramfs-virt" "$OUT/overlay.cpio.gz" >"$OUT/$1"
  rm -rf "$root" "$OUT/overlay.cpio.gz" "$2"
}

# The vibe-vm test guest: vibe-testguest as PID 1, with the share at
# /mnt/share and DHCP from busybox, in the background so the vsock server
# needn't wait for it.
cat >"$OUT/init-testguest" <<'INIT'
mkdir -p /mnt/share
mount -t virtiofs share /mnt/share || echo "test-guest: no share"
ip link set eth0 up || echo "test-guest: no eth0"
cat >/udhcpc.script <<'DHCP'
#!/bin/sh
case $1 in
  bound | renew)
    ip addr flush dev "$interface"
    ip addr add "$ip/$mask" dev "$interface"
    [ -n "$router" ] && ip route add default via "${router%% *}" dev "$interface"
    ;;
esac
exit 0
DHCP
chmod 755 /udhcpc.script
udhcpc -i eth0 -s /udhcpc.script -f -q -n -t 10 -T 1 >/dev/null 2>&1 &
echo "test-guest: exec vibe-testguest"
exec /vibe-testguest
INIT
initrd initrd "$OUT/init-testguest"

# The vibe-guest test guest: vibe-guest as PID 1, from the initrd (no
# vibe.layers), doing its own mounts and DHCP.
cat >"$OUT/init-guest" <<'INIT'
# Packet sockets, for vibe-guest's DHCP client.
insmod "$k/net/packet/af_packet.ko" || echo "test-guest: can't load af_packet"
echo "test-guest: exec vibe-guest"
exec /vibe-guest
INIT
initrd initrd-guest "$OUT/init-guest"
cp "$CACHE/modloop-virt" "$OUT/modloop"

echo "test guest in $OUT: Image, initrd, initrd-guest, modloop"
