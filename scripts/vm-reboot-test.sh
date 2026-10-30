#!/usr/bin/env bash
# Checks a sleeping linux VM survives a reboot of the Mac (#7), in two
# halves on either side of one:
#
#   scripts/vm-reboot-test.sh save      boot the test guest, tick its in-memory
#                                       counter to 7, write to its disk, save it
#   (reboot the Mac)
#   scripts/vm-reboot-test.sh restore   restore it: the counter must go on to 8
#                                       in the same boot, the disk intact
#
# Needs build/vibe-vm (scripts/sign-macos.sh) and build/test-guest
# (scripts/test-guest.sh); builds them if missing. Its state is kept in
# build/vm-reboot/ between the halves. Run from Terminal in the logged-in
# session, like check-full.sh.

set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

phase=${1:-}
case $phase in
  save | restore) ;;
  *)
    echo "usage: scripts/vm-reboot-test.sh save|restore" >&2
    exit 2
    ;;
esac
if [ "$(uname -s)" != Darwin ]; then
  echo "vm-reboot-test.sh: needs macOS" >&2
  exit 1
fi

STATE=build/vm-reboot
MARKER=build/vm-reboot-marker
[ -x build/vibe-vm ] || scripts/sign-macos.sh
[ -f build/test-guest/Image ] || scripts/test-guest.sh build/test-guest

boot_time() { sysctl -n kern.boottime; }

if [ "$phase" = restore ]; then
  [ -f "$STATE/boottime" ] || { echo "run the save half first" >&2; exit 1; }
  if [ "$(boot_time)" = "$(cat "$STATE/boottime")" ]; then
    echo "vm-reboot-test.sh: the Mac hasn't rebooted since the save; restoring anyway" >&2
  fi
fi
rm -rf "$MARKER"

VIBE_VM_PHASE=$phase VIBE_VM_STATE="$STATE" VIBE_VM_MARKER="$MARKER" \
  VIBE_VM_BIN=build/vibe-vm VIBE_VM_GUEST=build/test-guest \
  cargo test -p vibe-env --locked --test vm_cycle -- reboot --exact --nocapture

if [ "$phase" = save ]; then
  boot_time >"$STATE/boottime"
  echo "saved. Reboot the Mac, then run: scripts/vm-reboot-test.sh restore"
else
  [ -f "$MARKER/reboot" ] || { echo "the restore half didn't run" >&2; exit 1; }
  echo "PASS: the VM saved before $(cat "$STATE/boottime") restored after $(boot_time)"
fi
