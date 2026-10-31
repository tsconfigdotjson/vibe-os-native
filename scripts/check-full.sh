#!/usr/bin/env bash
# Every check in one command:
#
#   - the Rust workspace: fmt, clippy (warnings denied), build, test
#   - the vibe-guest cross-build: a static aarch64-unknown-linux-musl binary,
#     and clippy for that target over it and the test guest, as musl's libc
#     differs
#   - cargo check of the workspace for every host target it doesn't run on
#     (macOS and Linux, arm64 and x86_64)
#   - a vibed hello-world smoke test, through `vibe hello` and `vibe rpc`
#   - the registry through `vibe rpc`: add a project, edit a profile, change
#     a host limit, restart vibed, and find all of it
#   - sessions: a shell held by vibe-shim keeps running when vibed is killed,
#     a new vibed adopts it again, and `vibe ls` shows it, then its exit
#   - agents through `vibe rpc`, on vibe-env's test backend with its stand-in
#     for Claude Code: a start makes the worktree, the branch and the
#     transcript where Claude writes it; the status engine reads the
#     question in that transcript as `question`; the agent outlives a killed
#     vibed, and the new one rebuilds that state from the transcript; a
#     delete is refused while a commit is unpushed, and once it's pushed,
#     leaves nothing behind but the branch
#   - both Swift packages (swift/VibeOS, swift/vibe-vm): build and test,
#     including the Swift half of the Rust<->Swift contract test
#   - ad hoc signing, with the virtualization entitlement on vibe-vm
#   - vibe-vm check, signed and unsigned
#   - the test guests (scripts/test-guest.sh): Alpine's netboot kernel, and
#     initrds with vibe-testguest or vibe-guest as PID 1
#   - VMs through vibe-vm, as vibed drives them: boot, save and restore the
#     test guest, on its own and three at once, within the spike's numbers;
#     and vibe-guest's: a stand-in for Claude with a question pending,
#     through a real vibe-shim, still on screen after freeze, save, restore
#     and thaw (crates/vibe-env/tests/vm_cycle.rs). With VIBE_CLAUDE_BIN
#     (Claude Code's linux-arm64-musl build) and CLAUDE_CODE_OAUTH_TOKEN set,
#     the same with the real Claude and its AskUserQuestion, which is then
#     required too, and which the status engine must read from the real
#     transcript as a firm question, before the sleep and after. The check
#     across a reboot is scripts/vm-reboot-test.sh, which can't run here. On a Mac that is
#     itself a VM, such as a CI runner, this step is N/A: it can't nest VMs.
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
  # Anything else started on WORK's files, such as an agent's shim that a
  # failed step left running; killing a shim hangs up its command.
  pkill -KILL -f "$WORK/" 2>/dev/null
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

# A step function that exits with this has nothing it can check here, and
# says why on its last line: the step is N/A, not a pass.
NOT_APPLICABLE=77

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
  elif [ $rc -eq $NOT_APPLICABLE ]; then
    record "$id" N/A "$desc: $(tail -n 1 "$log")"
    echo "--  $id: not applicable: $(tail -n 1 "$log")"
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

# start_vibed <socket> <log>: starts target/debug/vibed and waits for it. Its
# $HOME is the socket's grandparent's home/, so it never touches the real
# state; a restart on the same socket finds the same state.
start_vibed() {
  VIBED_SOCKET=$1
  HOME="$(dirname "$(dirname "$1")")/home" target/debug/vibed --socket "$1" 2>"$2" &
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

# Every test binary runs, so one failure doesn't hide the others.
s_test() { cargo test --workspace --locked --no-fail-fast "${TEST_ARGS[@]}"; }

s_guest() {
  local target=aarch64-unknown-linux-musl info
  rustup target add "$target"
  cargo build -p vibe-guest --locked --release --target "$target"
  info=$(file "target/$target/release/vibe-guest")
  echo "$info"
  case $info in *"ARM aarch64"*) ;; *) echo "not an aarch64 binary"; return 1 ;; esac
  case $info in *"statically linked"*) ;; *) echo "not statically linked"; return 1 ;; esac
  cargo clippy -p vibe-guest -p vibe-testguest --all-targets --locked --target "$target" -- -D warnings
}

s_test_guest() { scripts/test-guest.sh build/test-guest; }

# Every crate type-checks for the hosts vibed runs on, whatever this machine
# is. Checking needs each target's std but no linker, so it works anywhere.
s_cross() {
  local host target
  host=$(rustc -vV | sed -n 's/^host: //p')
  for target in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
    [ "$target" = "$host" ] && continue
    rustup target add "$target"
    # The bundled SQLite is C, which needs the target's C toolchain to
    # build. A check links nothing, so skip it and use the bindings alone.
    LIBSQLITE3_SYS_USE_PKG_CONFIG=1 cargo check --workspace --all-targets --locked --target "$target"
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

# rpc <message>...: one connection through `vibe rpc`, printing the replies.
rpc() { printf '%s\n' "$@" | target/debug/vibe --socket "$VIBED_SOCKET" rpc; }

# expect <text> <what>: fails unless $out contains <text>.
expect() {
  case $out in *"$1"*) ;; *) echo "missing $2: $1"; return 1 ;; esac
}

s_registry() {
  local socket="$WORK/registry/run/vibed.sock" repo="$WORK/registry/src/app" starters
  mkdir -p "$repo"
  git -C "$repo" init -q
  start_vibed "$socket" "$WORK/registry-vibed-1.log"
  out=$(rpc \
    "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"projects.add\",\"params\":{\"path\":\"$repo\"}}" \
    '{"jsonrpc":"2.0","id":2,"method":"profiles.list","params":{"project":"app"}}' \
    '{"jsonrpc":"2.0","id":3,"method":"profiles.update","params":{"project":"app","profile":"software-eng-linux","memory":8589934592,"mode":"bypass"}}' \
    '{"jsonrpc":"2.0","id":4,"method":"limits.set","params":{"max_awake_agents":7}}')
  echo "first run: $out"
  case $out in *'"error"'*) echo "a call failed"; return 1 ;; esac
  # A new project starts with a profile per environment the host has.
  starters=$(printf '%s\n' "$out" | grep '"id":2' | grep -o '"name":"software-eng-[a-z]*"' | tr '\n' ' ')
  echo "starter profiles: $starters"
  if [ "$OS" = Darwin ]; then
    [ "$starters" = '"name":"software-eng-linux" "name":"software-eng-mac" ' ] || { echo "a Mac starts with two"; return 1; }
  else
    [ "$starters" = '"name":"software-eng-linux" ' ] || { echo "a Linux host starts with one"; return 1; }
  fi
  stop_vibed

  start_vibed "$socket" "$WORK/registry-vibed-2.log"
  out=$(rpc \
    '{"jsonrpc":"2.0","id":5,"method":"projects.list","params":{}}' \
    '{"jsonrpc":"2.0","id":6,"method":"limits.get","params":{}}')
  echo "after a restart: $out"
  expect '"name":"app"' "the project"
  expect '"memory":8589934592' "the edited profile"
  expect '"mode":"bypass"' "the edited mode"
  expect '"max_awake_agents":7' "the host limit"
  stop_vibed
  grep -q "max_awake_agents = 7" "$WORK/registry/home/.config/vibeos/host.toml"
}

# vibe ls, against the daemon start_vibed started, with its home.
ls_sessions() { HOME="$1" target/debug/vibe --socket "$VIBED_SOCKET" ls; }

s_sessions() {
  local socket="$WORK/sessions/run/vibed.sock" home="$WORK/sessions/home" run shim i
  if [ "$OS" = Darwin ]; then
    run="$home/Library/Application Support/VibeOS/run"
  else
    run="$home/.local/share/vibeos/run"
  fi
  start_vibed "$socket" "$WORK/sessions-vibed-1.log"
  shim=$(PS1='$ ' target/debug/vibe-shim --ref smoke-shell --socket "$run/smoke-shell.sock" \
    --state "$home/VibeOS/agents/smoke-shell" --cwd "$WORK" -- sh)
  echo "$shim" >>"$WORK/pids"
  echo "vibe-shim started as pid $shim"
  out=$(ls_sessions "$home")
  echo "$out"
  expect "smoke-shell  running  not attached" "the running session"

  # Killed mid-session, vibed takes nothing with it.
  kill -KILL "$VIBED_PID"
  wait "$VIBED_PID" 2>/dev/null || true
  kill -0 "$shim" || { echo "the session died with vibed"; return 1; }
  start_vibed "$socket" "$WORK/sessions-vibed-2.log"
  wait_for "$WORK/sessions-vibed-2.log" "re-adopted session smoke-shell"
  out=$(ls_sessions "$home")
  echo "after a restart: $out"
  expect "smoke-shell  running" "the adopted session"

  # Stopping the shim hangs up the shell and leaves a record of the end.
  kill -TERM "$shim"
  for i in $(seq 1 100); do
    kill -0 "$shim" 2>/dev/null || break
    sleep 0.1
  done
  out=$(ls_sessions "$home")
  echo "after the shim stopped: $out"
  expect "smoke-shell  exited   SIGHUP at" "the session's exit"
  stop_vibed
}

# json_field <text> <id> <field>: the first string <field> in the reply to
# request <id>.
json_field() {
  printf '%s\n' "$1" | grep "\"id\":$2[,}]" | grep -o "\"$3\":\"[^\"]*\"" | head -n 1 | cut -d '"' -f 4
}

# wait_file <path>: up to 20 s for <path> to exist.
wait_file() {
  local i
  for i in $(seq 1 200); do
    [ -e "$1" ] && return 0
    sleep 0.1
  done
  echo "$1 never appeared"
  return 1
}

s_agents() {
  # A short directory: a session's socket is under its home's Library on a
  # Mac, and the whole path must fit in 104 bytes.
  local socket="$WORK/ag/run/vibed.sock" home="$WORK/ag/home" origin="$WORK/ag/origin"
  local repo="$WORK/ag/src/app" ref worktree session transcript i left run
  if [ "$OS" = Darwin ]; then
    run="$home/Library/Application Support/VibeOS/run"
  else
    run="$home/.local/share/vibeos/run"
  fi
  git init -q "$origin"
  git -C "$origin" -c user.name=t -c user.email=t@example.com commit -q --allow-empty -m first
  git clone -q "$origin" "$repo"
  # The stand-in for Claude Code: on the prompt "commit question" it commits
  # on its branch, then asks a question, in its transcript as Claude does,
  # and waits for the answer until it's hung up.
  export VIBE_TEST_BACKEND="$ROOT/crates/vibe-env/src/stand-in.sh"
  start_vibed "$socket" "$WORK/ag-vibed-1.log"
  # 1G agents and nothing kept back, so one fits on any machine.
  out=$(rpc \
    "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"projects.add\",\"params\":{\"path\":\"$repo\"}}" \
    '{"jsonrpc":"2.0","id":2,"method":"profiles.update","params":{"project":"app","profile":"software-eng-linux","memory":1073741824}}' \
    '{"jsonrpc":"2.0","id":3,"method":"limits.set","params":{"reserve_memory":0}}' \
    '{"jsonrpc":"2.0","id":4,"method":"agents.start","params":{"project":"app","profile":"software-eng-linux","prompt":"commit question"}}')
  echo "start: $out"
  case $out in *'"error"'*) echo "a call failed"; return 1 ;; esac
  ref=$(json_field "$out" 4 ref)
  worktree=$(json_field "$out" 4 worktree)
  session=$(json_field "$out" 4 session_id)
  echo "agent $ref in $worktree, session $session"
  [ -n "$ref" ] && [ -d "$worktree" ] || { echo "no worktree"; return 1; }
  [ "$(git -C "$worktree" rev-parse --abbrev-ref HEAD)" = "$ref" ] || { echo "not on its branch"; return 1; }
  # Its transcript, where Claude Code writes it: its Claude home's
  # projects/, in a directory named after the worktree, by session id.
  transcript="$home/VibeOS/agents/$ref/claude/projects/$(printf '%s' "$worktree" | tr -c 'A-Za-z0-9' '-')/$session.jsonl"
  wait_file "$transcript"
  grep -q "\"sessionId\":\"$session\"" "$transcript"
  for i in $(seq 1 200); do
    [ "$(git -C "$repo" log -1 --format=%s "$ref")" = work ] && break
    sleep 0.1
  done
  [ "$(git -C "$repo" log -1 --format=%s "$ref")" = work ] || { echo "the agent never committed"; return 1; }
  wait_for "$transcript" AskUserQuestion
  out=$(rpc '{"jsonrpc":"2.0","id":8,"method":"agents.list","params":{}}')
  echo "asking: $out"
  expect '"state":"question"' "the question, from the transcript"
  expect '"activity":"awaiting_answer"' "its activity"

  # Killed mid-work, vibed takes nothing with it.
  kill -KILL "$VIBED_PID"
  wait "$VIBED_PID" 2>/dev/null || true
  start_vibed "$socket" "$WORK/ag-vibed-2.log"
  # The killed one's socket is still there until the new one replaces it.
  wait_for "$WORK/ag-vibed-2.log" "listening on"
  out=$(rpc '{"jsonrpc":"2.0","id":5,"method":"agents.list","params":{}}')
  echo "after a restart: $out"
  expect "\"ref\":\"$ref\"" "the agent"
  expect '"state":"question"' "its state, rebuilt from its transcript"
  expect '"activity":"awaiting_answer"' "its activity, rebuilt"

  out=$(rpc "{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"agents.delete\",\"params\":{\"ref\":\"$ref\"}}")
  echo "delete with an unpushed commit: $out"
  expect "1 commit that isn't pushed" "the refusal"
  [ -d "$worktree" ] || { echo "the refused delete removed the worktree"; return 1; }

  git -C "$worktree" push -q origin "$ref"
  out=$(rpc "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"agents.delete\",\"params\":{\"ref\":\"$ref\"}}")
  echo "delete once pushed: $out"
  expect '"state":"exited"' "the deleted agent"
  [ ! -e "$worktree" ] || { echo "the worktree is left"; return 1; }
  [ ! -e "$home/VibeOS/agents/$ref" ] || { echo "its directory is left"; return 1; }
  # Nothing of it in VibeOS's folders, nor a socket or lock in run/.
  left=$(find "$home/VibeOS/worktrees" "$home/VibeOS/agents" "$run" -mindepth 1 2>/dev/null)
  [ -z "$left" ] || { echo "left behind: $left"; return 1; }
  git -C "$repo" rev-parse --verify --quiet "refs/heads/$ref" >/dev/null || { echo "the branch went"; return 1; }
  git -C "$origin" rev-parse --verify --quiet "refs/heads/$ref" >/dev/null || { echo "the push didn't land"; return 1; }
  stop_vibed
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

# settle: waits up to 2 minutes for the 1-minute load average to fall below
# half the cores, and says what it was.
settle() {
  local cores load i
  cores=$(sysctl -n hw.ncpu)
  for i in $(seq 1 120); do
    load=$(sysctl -n vm.loadavg | awk '{print $2}')
    if awk -v l="$load" -v c="$cores" 'BEGIN { exit !(l < c / 2) }'; then
      break
    fi
    sleep 1
  done
  echo "load average $load on $cores cores after ${i}s"
}

s_vm_cycle() {
  local marker="$WORK/vm-marker" t
  # A Mac that is itself a VM (a CI runner) can't run nested VMs.
  if [ "$(sysctl -n kern.hv_vmm_present 2>/dev/null || echo 0)" = 1 ]; then
    echo "this Mac is a VM, so it can't run the test guest"
    return $NOT_APPLICABLE
  fi
  rm -rf "$marker"
  # The VM tests hold saves and restores to the spike's numbers, so they
  # start once the build is done and the Mac has settled: its 1-minute
  # load below half its cores, or two minutes, whichever comes first.
  cargo test -p vibe-env --locked --test vm_cycle --no-run
  settle
  VIBE_VM_BIN="$ROOT/build/vibe-vm" VIBE_VM_GUEST="$ROOT/build/test-guest" VIBE_VM_MARKER="$marker" \
    VIBE_SHIM_BIN="$ROOT/target/debug/vibe-shim" \
    cargo test -p vibe-env --locked --test vm_cycle -- --test-threads=1 --nocapture
  local required=(boot_save_restore three_vms_at_once guest_question_survives_sleep)
  if [ -n "${VIBE_CLAUDE_BIN:-}" ] && [ -n "${CLAUDE_CODE_OAUTH_TOKEN:-}" ]; then
    required+=(guest_claude_question_survives_sleep)
  else
    echo "guest_claude_question_survives_sleep not asked for: set VIBE_CLAUDE_BIN and CLAUDE_CODE_OAUTH_TOKEN"
  fi
  for t in "${required[@]}"; do
    [ -f "$marker/$t" ] || { echo "$t was skipped, not run"; return 1; }
  done
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
step guest "vibe-guest: static aarch64-unknown-linux-musl, clippy for musl" s_guest
mac_step test-guest "test guests: kernel, initrd, initrd-guest, modloop" s_test_guest
step cross "cargo check for macOS and Linux, arm64 and x86_64" s_cross
step smoke "vibed hello world via vibe hello and vibe rpc" s_smoke build
step registry "registry via vibe rpc survives a vibed restart" s_registry build
step sessions "a session outlives a killed vibed and is re-adopted" s_sessions build
step agents "agents start, outlive vibed, and delete to nothing but the branch" s_agents build
mac_step swift-app-build "swift build swift/VibeOS" s_swift_app_build
mac_step swift-app-test "swift test swift/VibeOS (Swift contract tests)" s_swift_app_test swift-app-build
mac_step swift-vm-build "swift build swift/vibe-vm" s_swift_vm_build
mac_step swift-vm-test "swift test swift/vibe-vm" s_swift_vm_test swift-vm-build
mac_step sign "ad hoc signing; vibe-vm gets the virtualization entitlement" s_sign swift-app-build swift-vm-build
mac_step vm-check "vibe-vm check, signed and unsigned" s_vm_check sign
mac_step vm-cycle "VMs boot, save and restore; vibe-guest's question survives" s_vm_cycle build sign test-guest
mac_step live-contract "Swift client against a live vibed" s_live_contract build swift-app-test
mac_step app-launch "VibeOS.app launches and says hello to vibed" s_app_launch build sign

echo
echo "================================ summary ================================"
failed=0
skipped=0
na=0
for i in "${!IDS[@]}"; do
  printf '%-5s %-16s %s\n' "${STATUS[$i]}" "${IDS[$i]}" "${NOTES[$i]}"
  case ${STATUS[$i]} in
    FAIL) failed=$((failed + 1)) ;;
    SKIP) skipped=$((skipped + 1)) ;;
    N/A) na=$((na + 1)) ;;
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
elif [ $na -gt 0 ]; then
  echo "PASS, but $na steps had nothing to check on this machine (N/A above)."
else
  echo "PASS: every step ran and passed."
fi
