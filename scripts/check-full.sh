#!/usr/bin/env bash
# Every check in one command:
#
#   - the Rust workspace: fmt, clippy (warnings denied), build, test
#   - the vibe-guest cross-build: a static aarch64-unknown-linux-musl binary
#   - cargo check of the workspace for every host target it doesn't run on
#     (macOS and Linux, arm64 and x86_64)
#   - a vibed hello-world smoke test, through `vibe hello` and `vibe rpc`
#   - both Swift packages (swift/VibeOS, swift/vibe-vm): build and test,
#     including the Swift half of the Rust<->Swift contract test
#   - ad hoc signing, with the virtualization entitlement on vibe-vm
#   - vibe-vm check, signed and unsigned
#   - the Swift client against a live vibed
#   - VibeOS.app launching and saying hello to a live vibed
#
# Made for a Mac on macOS 26 with Xcode 26 (or its command line tools) and
# rustup. On Linux it runs the Rust steps and the vibed smoke test and skips
# the macOS-only ones. Safe to run repeatedly: logs go to build/check/, and
# daemons and temp files are cleaned up on exit. Exits non-zero if any step
# fails (or, on a Mac, is skipped).
#
# On a Mac, run it from Terminal in the logged-in GUI session, not over SSH:
# the app-launch step starts VibeOS.app, which needs the window server.

set -o pipefail
cd "$(dirname "$0")/.." || exit 1
ROOT=$(pwd)
OS=$(uname -s)
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TERM_COLOR=never

TEST_ARGS=()
if [ "$OS" = Linux ]; then
  # Keep small Linux hosts responsive: few build jobs, one test thread.
  export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}
  TEST_ARGS=(-- "--test-threads=${TEST_THREADS:-1}")
fi

LOGS="$ROOT/build/check"
rm -rf "$LOGS"
mkdir -p "$LOGS"
# Under /tmp, not $TMPDIR: a Unix socket path must fit in 104 bytes on macOS.
WORK=$(mktemp -d /tmp/vibe-check.XXXXXX) || exit 1

cleanup() {
  if [ -f "$WORK/pids" ]; then
    while read -r pid; do kill "$pid" 2>/dev/null; done <"$WORK/pids"
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

IDS=()
STATUS=()
NOTES=()

record() {
  IDS+=("$1")
  STATUS+=("$2")
  NOTES+=("$3")
}

status_of() {
  local i
  for i in "${!IDS[@]}"; do
    if [ "${IDS[$i]}" = "$1" ]; then
      echo "${STATUS[$i]}"
      return
    fi
  done
  echo NONE
}

# step <id> <description> <function> [<id it needs>...]
step() {
  local id=$1 desc=$2 fn=$3 dep log start rc
  shift 3
  for dep in "$@"; do
    if [ "$(status_of "$dep")" != PASS ]; then
      record "$id" SKIP "$desc (needs $dep)"
      echo "--  $id: skipped, needs $dep"
      return
    fi
  done
  log="$LOGS/$id.log"
  start=$SECONDS
  echo "==> $id: $desc"
  # A subshell with errexit, so any failing command fails the step.
  (set -e; "$fn") >"$log" 2>&1
  rc=$?
  if [ $rc -eq 0 ]; then
    record "$id" PASS "$desc ($((SECONDS - start))s)"
  else
    record "$id" FAIL "$desc ($((SECONDS - start))s; log: build/check/$id.log)"
    echo "--- $id failed (exit $rc); the end of its log:"
    tail -n 40 "$log" | sed 's/^/    /'
  fi
}

mac_step() {
  if [ "$OS" = Darwin ]; then
    step "$@"
  else
    record "$1" SKIP "$2 (macOS only)"
    echo "--  $1: skipped, macOS only"
  fi
}

# start_vibed <socket> <log>: starts target/debug/vibed and waits for it.
start_vibed() {
  VIBED_SOCKET=$1
  target/debug/vibed --socket "$1" 2>"$2" &
  VIBED_PID=$!
  echo "$VIBED_PID" >>"$WORK/pids"
  local i
  for i in $(seq 1 200); do
    [ -S "$1" ] && return 0
    kill -0 "$VIBED_PID" 2>/dev/null || { echo "vibed exited:"; cat "$2"; return 1; }
    sleep 0.1
  done
  echo "vibed didn't create $1 in 20 s"
  return 1
}

# stop_vibed: SIGTERM, then checks it exited cleanly and removed its socket.
stop_vibed() {
  kill -TERM "$VIBED_PID"
  local rc=0
  wait "$VIBED_PID" || rc=$?
  [ $rc -eq 0 ] || { echo "vibed exited with $rc on SIGTERM"; return 1; }
  [ ! -e "$VIBED_SOCKET" ] || { echo "vibed left $VIBED_SOCKET behind"; return 1; }
}

# wait_for <file> <text>: up to 20 s for <text> to appear in <file>.
wait_for() {
  local i
  for i in $(seq 1 200); do
    grep -q "$2" "$1" 2>/dev/null && return 0
    sleep 0.1
  done
  echo "\"$2\" never appeared in $1:"
  cat "$1"
  return 1
}

s_tools() {
  uname -a
  rustc --version
  cargo --version
  if [ "$OS" = Darwin ]; then
    sw_vers
    xcode-select -p
    swift --version
  fi
}

s_fmt() { cargo fmt --all --check; }

s_clippy() { cargo clippy --workspace --all-targets --locked -- -D warnings; }

s_build() { cargo build --workspace --locked; }

s_test() { cargo test --workspace --locked "${TEST_ARGS[@]}"; }

s_guest() {
  local target=aarch64-unknown-linux-musl info
  rustup target add "$target"
  cargo build -p vibe-guest --locked --release --target "$target"
  info=$(file "target/$target/release/vibe-guest")
  echo "$info"
  case $info in *"ARM aarch64"*) ;; *) echo "not an aarch64 binary"; return 1 ;; esac
  case $info in *"statically linked"*) ;; *) echo "not statically linked"; return 1 ;; esac
}

# Every crate type-checks for the hosts vibed runs on, whatever this machine
# is. Checking needs each target's std but no linker, so it works anywhere.
s_cross() {
  local host target
  host=$(rustc -vV | sed -n 's/^host: //p')
  for target in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
    [ "$target" = "$host" ] && continue
    rustup target add "$target"
    cargo check --workspace --all-targets --locked --target "$target"
  done
}

s_smoke() {
  local log="$WORK/smoke-vibed.log" out
  start_vibed "$WORK/smoke/run/vibed.sock" "$log"
  target/debug/vibe --socket "$VIBED_SOCKET" hello
  out=$(printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"daemon.hello","params":{"client":"check-full","protocol":1}}' \
    | target/debug/vibe --socket "$VIBED_SOCKET" rpc)
  echo "vibe rpc: $out"
  case $out in *'"daemon":"vibed"'*) ;; *) echo "vibe rpc gave no hello result"; return 1 ;; esac
  stop_vibed
  grep -q "hello from vibe-cli" "$log"
  grep -q "hello from check-full" "$log"
  cat "$log"
}

s_swift_app_build() { swift build --package-path swift/VibeOS; }

# The Swift half of the contract test runs here; the live suite stays off.
s_swift_app_test() { env -u VIBE_TEST_SOCKET swift test --package-path swift/VibeOS; }

s_swift_vm_build() { swift build --package-path swift/vibe-vm; }

s_swift_vm_test() { swift test --package-path swift/vibe-vm; }

s_sign() { scripts/sign-macos.sh; }

s_vm_check() {
  local unsigned out rc=0 in_vm
  unsigned="$(swift build --package-path swift/vibe-vm --show-bin-path)/vibe-vm"
  build/vibe-vm --version
  out=$(build/vibe-vm check) || rc=$?
  echo "signed (exit $rc): $out"
  case $out in *'"entitled":true'*) ;; *) echo "signed vibe-vm lacks the entitlement"; return 1 ;; esac
  # A Mac that is itself a VM (a CI runner) can't run nested VMs.
  in_vm=$(sysctl -n kern.hv_vmm_present 2>/dev/null || echo 0)
  if [ "$in_vm" = 1 ]; then
    echo "this Mac is a VM: not requiring Virtualization.framework support"
  else
    [ $rc -eq 0 ] || { echo "signed vibe-vm check should exit 0 on real hardware"; return 1; }
    case $out in *'"supported":true'*) ;; *) echo "Virtualization.framework unsupported"; return 1 ;; esac
  fi
  # Without the entitlement it must say so, or the check above proves nothing.
  rc=0
  out=$("$unsigned" check) || rc=$?
  echo "unsigned (exit $rc): $out"
  [ $rc -eq 1 ] || { echo "unsigned vibe-vm check should exit 1"; return 1; }
  case $out in *'"entitled":false'*) ;; *) echo "unsigned vibe-vm claims the entitlement"; return 1 ;; esac
}

s_live_contract() {
  local log="$WORK/live-vibed.log" marker="$WORK/live-marker"
  start_vibed "$WORK/live/run/vibed.sock" "$log"
  rm -f "$marker"
  VIBE_TEST_SOCKET="$VIBED_SOCKET" VIBE_TEST_MARKER="$marker" \
    swift test --package-path swift/VibeOS --filter LiveDaemonTests
  [ -f "$marker" ] || { echo "the live test was skipped, not run"; return 1; }
  grep -q "hello from VibeRPCTests" "$log"
  stop_vibed
}

s_app_launch() {
  local log="$WORK/app-vibed.log" app
  start_vibed "$WORK/app/run/vibed.sock" "$log"
  VIBE_SOCKET="$VIBED_SOCKET" build/VibeOS.app/Contents/MacOS/VibeOS >"$WORK/app.log" 2>&1 &
  app=$!
  echo "$app" >>"$WORK/pids"
  # The app says hello at launch, so vibed's log shows it came up.
  wait_for "$log" "hello from VibeOS.app"
  sleep 2
  kill -0 "$app" 2>/dev/null || { echo "VibeOS.app quit on its own:"; cat "$WORK/app.log"; return 1; }
  kill "$app"
  wait "$app" 2>/dev/null || true
  stop_vibed
  echo "VibeOS.app launched and said hello to vibed"
}

step tools "toolchain versions" s_tools
step fmt "cargo fmt --check" s_fmt
step clippy "cargo clippy, warnings denied" s_clippy
step build "cargo build" s_build
step test "cargo test (incl. the Rust contract tests)" s_test
step guest "vibe-guest: static aarch64-unknown-linux-musl" s_guest
step cross "cargo check for macOS and Linux, arm64 and x86_64" s_cross
step smoke "vibed hello world via vibe hello and vibe rpc" s_smoke build
mac_step swift-app-build "swift build swift/VibeOS" s_swift_app_build
mac_step swift-app-test "swift test swift/VibeOS (Swift contract tests)" s_swift_app_test swift-app-build
mac_step swift-vm-build "swift build swift/vibe-vm" s_swift_vm_build
mac_step swift-vm-test "swift test swift/vibe-vm" s_swift_vm_test swift-vm-build
mac_step sign "ad hoc signing; vibe-vm gets the virtualization entitlement" s_sign swift-app-build swift-vm-build
mac_step vm-check "vibe-vm check, signed and unsigned" s_vm_check sign
mac_step live-contract "Swift client against a live vibed" s_live_contract build swift-app-test
mac_step app-launch "VibeOS.app launches and says hello to vibed" s_app_launch build sign

echo
echo "================================ summary ================================"
failed=0
skipped=0
for i in "${!IDS[@]}"; do
  printf '%-5s %-16s %s\n' "${STATUS[$i]}" "${IDS[$i]}" "${NOTES[$i]}"
  case ${STATUS[$i]} in
    FAIL) failed=$((failed + 1)) ;;
    SKIP) skipped=$((skipped + 1)) ;;
  esac
done
echo "========================================================================="
if [ $failed -gt 0 ]; then
  echo "FAIL: $failed failed, $skipped skipped. Logs: build/check/"
  exit 1
elif [ "$OS" = Darwin ] && [ $skipped -gt 0 ]; then
  echo "FAIL: $skipped skipped; every step must run on a Mac. Logs: build/check/"
  exit 1
elif [ $skipped -gt 0 ]; then
  echo "PASS on $OS: $skipped macOS-only steps skipped; run this on a Mac for those."
else
  echo "PASS: every step ran and passed."
fi
