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
#   - `vibe doctor` against a live vibed, on this machine as it is: a line
#     for every check, and an exit that says whether one needs the owner
#     (crates/vibe-cli/tests/cli.rs, in the test step, holds its every word
#     to the golden host's, and --fix to running gh auth setup-git)
#   - the registry through `vibe rpc`: add a project, edit a profile, change
#     a host limit, restart vibed, and find all of it
#   - sessions: a shell held by vibe-shim keeps running when vibed is killed,
#     a new vibed adopts it again, and `vibe ls` shows it, then its exit
#   - agents through `vibe rpc`, on vibe-env's test backend with its stand-in
#     for Claude Code: a start makes the worktree, the branch and the
#     transcript where Claude writes it; the status engine reads the
#     question in that transcript as `question`; the agent outlives a killed
#     vibed, and the new one rebuilds that state from the transcript; at
#     rest with nothing attached it falls asleep on vibed's own watch once
#     a 1 s idle_debounce has passed, and a send wakes it to answer its
#     question; a delete is refused while a commit is unpushed, and once
#     it's pushed, leaves nothing behind but the branch
#   - sleep, wake and admission (#6, #47), one scripted scenario on every
#     backend (crates/vibed/tests/sleep.rs): idle -> asleep once the
#     debounce has passed; a send, and an attach, wake it; a start past
#     the host's memory goes ahead, as memory is a ceiling, not a
#     reservation, and one past max_awake_agents sleeps an idle agent
#     first; a firm /loop wakeup fires on time from a sleeping agent. The test
#     step runs it on the test backend (keeping memory, and saving to
#     disk) and, on a Mac, the macos backend; the cgroups step on this
#     machine's cgroups; the linux-vm step on real VMs. The test step also
#     covers the snapshot budget, low disk and critical memory pressure
#   - MCP, as a client drives it, on the test backend: vibed serves it on
#     the port host.toml names and refuses a call without an API key;
#     mcp.get gives its URL and mcp.set turns Tailscale on and off; then,
#     with a key from `vibe key create`, `vibe mcp` carries VibeOS's round
#     over stdio: start an agent on a question, wait for it, read it, answer
#     it with keys, wait for that answer, read it, and delete the agent. On
#     a Mac, vibed serves no tunnel socket (run/tunnel.sock) while no host is
#     the external address, and listens on no TCP port beyond loopback
#     (lsof says so)
#   - a Linux host over SSH, with every process real but the network
#     (crates/vibe-cli/tests/linux_host.rs): a stand-in ssh runs commands in a
#     home of the host's own and a stand-in systemctl runs its vibed; Add
#     Linux host installs over it with the real install scripts, the link
#     comes up over `ssh <host> vibe rpc`, an agent started there through the
#     Mac's MCP shows in the Mac's listing with its ssh attach line, the
#     Mac's API key is pushed to the host, whose own MCP serves it with that
#     key, and a pulled network shows the host down and its agent
#     unavailable until it reconnects on its own. Then external access
#     (#18) the same way: `vibe host external` makes the host the external
#     address, served over HTTPS with a certificate made for the test and
#     reached by `ssh -R` (a stand-in relay) from the Mac's tunnel socket;
#     a client with curl, the URL and a key starts an agent on the host and
#     one on the Mac and talks to both, gets 401 without a key, is logged
#     by the key's name (never the key), sees the Mac's agents unavailable
#     while the Mac is offline and the host's still answering, and is cut
#     off on the host when the key is revoked on the Mac; the Mac listens on
#     no public port. (crates/vibed/tests/external.rs, in the test step,
#     covers the same in one process, plus rate limits, the Mac refusing a
#     host's own key and a revoked one even before the host hears, and the
#     tunnel's supervision.)
#   - a real Linux server, when VIBE_TEST_LINUX_HOST is an SSH destination
#     that logs in with a key (N/A otherwise): the Linux builds of vibed
#     (scripts/host-bundle.sh, which needs zig and cargo-zigbuild, or
#     VIBE_HOST_BUNDLE), installed over real ssh, linked, listed through
#     the Mac's MCP, and a hung link noticed and taken up again. With
#     VIBE_TEST_LINUX_PROJECT naming a project on the server, an agent
#     started there through MCP too, in a cgroup of its own. Then the
#     cgroups backend's own tests (crates/vibe-env/tests/cgroups.rs), built
#     for the server with cargo-zigbuild and run there with the bundle's
#     vibe-shim: the conformance suite on its real cgroups, and a memory hog
#     stopped by its own limit while its neighbour runs on. Every test agent
#     there has a memory limit of tens of MB, and nothing is left behind
#   - external access through a real server (#18), when VIBE_TEST_LINUX_HOST
#     is set and VIBE_TEST_EXTERNAL_DOMAIN is a public DNS name pointing at
#     it, with port 443 open there and usable by its user (N/A otherwise):
#     installed and linked as in the real-host step, made the external
#     address, it gets a real certificate from Let's Encrypt (kept on the
#     server, so a rerun doesn't ask again) and the Mac's real `ssh -R`
#     tunnel comes up; then a client on the internet with
#     https://<domain>/mcp and a key starts and talks to an agent on the
#     Mac, and one on the server when VIBE_TEST_LINUX_PROJECT names a
#     project there; it gets nothing without a key, nothing once the key is
#     revoked, and the Mac listens on no public port
#   - the Linux host's cgroups backend on this machine, on Linux with a
#     systemd user manager (N/A otherwise; on a Mac, the real-host step runs
#     it on a server): the same tests, and a linux agent through vibed in a
#     cgroup of its own, held to an edited max_processes by the kernel,
#     paused, frozen asleep and woken where it was, then deleted to nothing;
#     and the sleep scenario on its cgroups
#   - the macos backend (crates/vibe-env/tests/macos.rs, which the test step
#     runs everywhere): on a Mac again, with a fork bomb of about a thousand
#     processes, and PRIO_DARWIN_BG seen in `ps` as the memory warning
#     puts an agent in the background and lets it go
#   - a macos agent through `vibe rpc`, on the real macos backend with the
#     stand-in for Claude Code (VIBE_CLAUDE), with vibed in a LaunchAgent's
#     bare environment: the agent has the PATH of the owner's interactive
#     login shell, not launchd's; its transcript lands in the
#     Mac's own ~/.claude, the status engine reads its question, vibed holds
#     an IOPM assertion against idle sleep while it's awake (pmset says so)
#     and lets go once it falls asleep, a send wakes it, a max_processes
#     edited in host.toml pauses it at once, and a delete leaves nothing of
#     it there or in VibeOS's folders but the branch
#   - both Swift packages (swift/VibeOS, swift/vibe-vm): build and test,
#     including the Swift half of the Rust<->Swift contract test, and the
#     menu's model, Terminal attach and notifications against stand-ins
#   - the app icon (swift/VibeOS/Support/AppIcon.iconset): every size, a PNG
#     of the right size in the icon's two colours, and on a Mac within a
#     hair of AppIcon.svg rendered afresh by scripts/app-icon.swift
#   - the release's tools, which need no Mac: Sparkle's Ed25519 signing and
#     its appcast (scripts/sparkle.py self-test: RFC 8032's vectors, and the
#     cryptography package's answers where it's installed); the release
#     workflow running on version tags alone, reading the secrets the
#     release needs and printing none; the versions in Cargo.toml and
#     Info.plist agreeing, and its usage descriptions (Apple Events, Local
#     Network); the base release in images.rs and base-image.sh; vibed's
#     LaunchAgent naming the app and its vibed; and the entitlements
#   - a release build, ad hoc signed (scripts/release-macos.sh), with
#     Sparkle built in and a key made for the check: the DMG holds the app
#     and Applications; the app holds VibeOS, vibed, vibe, vibe-shim and
#     vibe-vm for Apple Silicon, vibed's LaunchAgent and Sparkle.framework,
#     and the Linux builds when cargo-zigbuild is there; its signature
#     verifies, vibe-vm keeps the virtualization entitlement and the app the
#     Apple Events one; Info.plist has the version, the appcast's URL and
#     the public key of the key the DMG is signed with; the appcast names
#     this DMG, its size and a signature that verifies; and the app as
#     shipped launches, Sparkle and all, and draws its welcome. It fetches
#     Sparkle the first time, so it needs the network. Developer ID signing
#     and notarization need the secrets, so release CI does those
#   - ad hoc signing, with the virtualization entitlement on vibe-vm, and
#     the app icon in the bundle
#   - every surface of the app (the menu in each state, the first run's
#     welcome and install notes among them, the menu bar glyph, Settings'
#     panes, the sheets) drawn on canned data in light and dark
#     by `VibeOS --render-surfaces`: none blank, each different in dark, and
#     kept in build/check/surfaces/ for a look. scripts/surfaces.sh takes
#     the same as real screenshots, materials and all
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
#   - the Linux base image (scripts/base-image.sh, with Docker): its kernel,
#     initrd and Debian image, built into build/base-image/
#   - linux agents in real VMs on that image: the linux backend passes the
#     vibe-env conformance suite (crates/vibe-env/tests/linux_conformance.rs),
#     and an agent from the starter profile answers on Opus in auto mode,
#     sleeps, and carries on its conversation after the wake, with Claude
#     Code's managed layer downloaded from downloads.claude.ai
#     (crates/vibed/tests/linux_vm.rs). It needs the Claude token: from
#     CLAUDE_CODE_OAUTH_TOKEN, or what `vibe setup-token` kept in the
#     Keychain. Then the toolbox: gcloud and a login made in it reach a new
#     agent, See logins names the logins its save found in the real guest
#     (gh, git, an SSH key), an agent asleep on the old template restores,
#     and [update] and
#     Rebuild template make new templates (crates/vibed/tests/toolbox_vm.rs,
#     which needs the network). And the sleep scenario on real VMs, with the
#     stand-in for Claude: saved to disk once idle, restored by a send and
#     an attach, slept early to make room for a start, and woken 30 s ahead
#     of its /loop, which fires on time. N/A on a Mac that is itself a VM,
#     like vm-cycle.
#   - the Swift client against a live vibed, its event stream included
#   - the menu against a live vibed on the test backend: an agent started
#     over the socket appears in it and turns `question`, from vibed's events
#     alone, and Interrupt, attach and Delete reach it
#   - VibeOS.app launching and saying hello to a live vibed
#   - Terminal attach with the real Terminal: two clicks on an agent's row,
#     through `VibeOS --attach`, give one tab attached to it, and a third
#     from a new process raises that tab. It needs Automation permission to
#     control Terminal for the app this script runs in; macOS asks once.
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
  # A release-build step that failed with its DMG still mounted.
  if [ -d "$WORK/release-dmg" ] && [ "$OS" = Darwin ]; then
    hdiutil detach -force "$WORK/release-dmg" >/dev/null 2>&1
  fi
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
  local host target var
  host=$(rustc -vV | sed -n 's/^host: //p')
  # C that dependencies build (ring, for the external address's TLS) needs
  # the target's C toolchain, which a Mac checking Linux doesn't have. A
  # check links nothing, so a stand-in compiler and archiver that make
  # empty outputs are enough, as the target's C is never used.
  write_c_stub "$WORK/cross-cc"
  for target in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
    [ "$target" = "$host" ] && continue
    rustup target add "$target"
    var=$(printf '%s' "$target" | tr - _)
    # The bundled SQLite is C too: skip it and use the bindings alone.
    env "CC_$var=$WORK/cross-cc" "AR_$var=$WORK/cross-cc" LIBSQLITE3_SYS_USE_PKG_CONFIG=1 \
      cargo check --workspace --all-targets --locked --target "$target"
  done
}

# write_c_stub <path>: a C compiler and archiver for `cargo check` of
# another target. It makes whatever output it's asked for, empty: `-o FILE`
# for a compile, the `.a` for an archive; `-E` (cc-rs asking what it is)
# prints nothing.
write_c_stub() {
  cat >"$1" <<'STUB'
#!/bin/sh
out=
while [ $# -gt 0 ]; do
  case $1 in
    -o) out=$2; shift 2 ;;
    -o?*) out=${1#-o}; shift ;;
    *.a) [ -n "$out" ] || out=$1; shift ;;
    *) shift ;;
  esac
done
if [ -n "$out" ]; then : >"$out"; fi
exit 0
STUB
  chmod +x "$1"
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
  # A 1G agent: its memory is only its ceiling, so nothing is set aside.
  out=$(rpc \
    "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"projects.add\",\"params\":{\"path\":\"$repo\"}}" \
    '{"jsonrpc":"2.0","id":2,"method":"profiles.update","params":{"project":"app","profile":"software-eng-linux","memory":1073741824}}' \
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

  # On a question with nothing attached, it's idle: once the debounce has
  # passed, vibed's own watch puts it to sleep, and a send wakes it where
  # it was, to answer.
  out=$(rpc '{"jsonrpc":"2.0","id":9,"method":"limits.set","params":{"idle_debounce":1}}')
  case $out in *'"error"'*) echo "limits.set failed: $out"; return 1 ;; esac
  for i in $(seq 1 150); do
    out=$(rpc '{"jsonrpc":"2.0","id":10,"method":"agents.list","params":{}}')
    case $out in *'"state":"asleep"'*) break ;; esac
    sleep 0.1
  done
  echo "idle: $out"
  expect '"state":"asleep"' "the agent asleep once idle"
  grep -q "$ref is asleep (idle for 1s, nothing attached)" "$WORK/ag-vibed-2.log" ||
    { echo "vibed didn't say why it slept"; return 1; }
  out=$(rpc "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"agents.send\",\"params\":{\"ref\":\"$ref\",\"keys\":[\"r\",\"e\",\"d\",\"Enter\"]}}")
  echo "send: $out"
  expect '"woke":true' "the wake"
  wait_for "$transcript" "User answered: red"
  out=$(rpc '{"jsonrpc":"2.0","id":12,"method":"limits.set","params":{"idle_debounce":600}}' \
    '{"jsonrpc":"2.0","id":13,"method":"agents.list","params":{}}')
  echo "woken: $out"
  case $out in *'"state":"asleep"'*) echo "still asleep after the send"; return 1 ;; esac

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

# mcp_call <id> <json> <text>: sends a request down the bridge, waits for
# its response in the bridge's output, and checks it holds <text>.
mcp_call() {
  printf '%s\n' "$2" >&7
  local i line
  # Longer than any wait it asks for, so a slow machine isn't a failure.
  for i in $(seq 1 900); do
    line=$(grep -E "\"id\":$1[,}]" "$MCP_OUT" | grep -v '"method"' | head -n 1 || true)
    [ -n "$line" ] && break
    sleep 0.1
  done
  echo "$1: $line"
  [ -n "$line" ] || { echo "no response to $1 in 90 s; the bridge said:"; cat "$MCP_OUT"; return 1; }
  case $line in *"$3"*) ;; *) echo "response $1 doesn't say: $3"; return 1 ;; esac
  case $line in *'"isError":true'*|*'"error":'*) echo "response $1 is an error"; return 1 ;; esac
}

s_mcp() {
  local socket="$WORK/mc/run/vibed.sock" home="$WORK/mc/home" origin="$WORK/mc/origin"
  local repo="$WORK/mc/src/app" port key out code bridge
  git init -q "$origin"
  git -C "$origin" -c user.name=t -c user.email=t@example.com commit -q --allow-empty -m first
  git clone -q "$origin" "$repo"
  # A port of its own, so a vibed already serving 7878 here isn't the one
  # asked.
  port=$((20000 + RANDOM % 30000))
  mkdir -p "$home/.config/vibeos"
  printf 'mcp_port = %s\n' "$port" >"$home/.config/vibeos/host.toml"
  export VIBE_TEST_BACKEND="$ROOT/crates/vibe-env/src/stand-in.sh"
  start_vibed "$socket" "$WORK/mc-vibed.log"
  wait_for "$WORK/mc-vibed.log" "mcp: serving http://127.0.0.1:$port/mcp"
  out=$(rpc \
    "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"projects.add\",\"params\":{\"path\":\"$repo\"}}" \
    '{"jsonrpc":"2.0","id":2,"method":"profiles.update","params":{"project":"app","profile":"software-eng-linux","memory":1073741824}}')
  case $out in *'"error"'*) echo "setup failed: $out"; return 1 ;; esac
  # No key, no MCP, even on loopback.
  code=$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' "http://127.0.0.1:$port/mcp")
  echo "without a key: HTTP $code"
  [ "$code" = 401 ] || { echo "a call without a key wasn't refused"; return 1; }
  # Where it's served, as Settings asks; and the Tailscale switch, written
  # to host.toml.
  out=$(rpc '{"jsonrpc":"2.0","id":3,"method":"mcp.get","params":{}}' \
    '{"jsonrpc":"2.0","id":4,"method":"mcp.set","params":{"tailscale":true}}')
  echo "mcp: $out"
  expect "\"url\":\"http://127.0.0.1:$port/mcp\"" "the loopback URL"
  expect '"tailscale":true' "Tailscale turned on"
  grep -q '^mcp_tailscale = true' "$home/.config/vibeos/host.toml" || { echo "not in host.toml"; return 1; }
  out=$(rpc '{"jsonrpc":"2.0","id":5,"method":"mcp.set","params":{"tailscale":false}}')
  expect '"tailscale_urls":[]' "Tailscale turned off"
  key=$(HOME="$home" target/debug/vibe --socket "$socket" key create check)
  case $key in vk_*) ;; *) echo "no key made: $key"; return 1 ;; esac

  MCP_OUT="$WORK/mc-bridge.out"
  mkfifo "$WORK/mc-in"
  HOME="$home" VIBE_API_KEY="$key" target/debug/vibe mcp <"$WORK/mc-in" >"$MCP_OUT" 2>"$WORK/mc-bridge.err" &
  bridge=$!
  echo "$bridge" >>"$WORK/pids"
  exec 7>"$WORK/mc-in"
  mcp_call 101 '{"jsonrpc":"2.0","id":101,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"check-full","version":"1"}}}' '"vibe-os-native"'
  printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}' >&7
  mcp_call 102 '{"jsonrpc":"2.0","id":102,"method":"tools/call","params":{"name":"vibe_list_projects","arguments":{}}}' 'app (on local)'
  mcp_call 103 '{"jsonrpc":"2.0","id":103,"method":"tools/call","params":{"name":"vibe_start_agent","arguments":{"project":"app","profile":"software-eng-linux","prompt":"question","name":"mcp-check"}}}' 'started mcp-check'
  mcp_call 104 '{"jsonrpc":"2.0","id":104,"method":"tools/call","params":{"name":"vibe_wait","arguments":{"agents":"mcp-check","timeout":60}}}' 'has answered: it is question'
  mcp_call 105 '{"jsonrpc":"2.0","id":105,"method":"tools/call","params":{"name":"vibe_read_output","arguments":{"agent":"mcp-check"}}}' 'Red or blue?'
  mcp_call 106 '{"jsonrpc":"2.0","id":106,"method":"tools/call","params":{"name":"vibe_send","arguments":{"agent":"mcp-check","keys":["B","l","u","e","Enter"]}}}' 'pressed B l u e Enter'
  mcp_call 107 '{"jsonrpc":"2.0","id":107,"method":"tools/call","params":{"name":"vibe_wait","arguments":{"agents":"mcp-check","timeout":60}}}' 'has answered: it is idle'
  mcp_call 108 '{"jsonrpc":"2.0","id":108,"method":"tools/call","params":{"name":"vibe_read_messages","arguments":{"agent":"mcp-check","last":1}}}' 'assistant: You chose Blue.'
  mcp_call 109 '{"jsonrpc":"2.0","id":109,"method":"tools/call","params":{"name":"vibe_delete_agent","arguments":{"agent":"mcp-check"}}}' 'deleted mcp-check; its branch mcp-check is kept'
  # stdin's end ends the bridge.
  exec 7>&-
  wait "$bridge" || { echo "vibe mcp exited with $?"; cat "$WORK/mc-bridge.err"; return 1; }
  [ ! -e "$home/VibeOS/worktrees/app/mcp-check" ] || { echo "the worktree is left"; return 1; }
  git -C "$repo" rev-parse --verify --quiet refs/heads/mcp-check >/dev/null || { echo "the branch went"; return 1; }
  # External access: with no host the external address, the tunnel's
  # socket isn't there at all, and nothing listens beyond loopback.
  local tunnel="$home/Library/Application Support/VibeOS/run/tunnel.sock" listening
  [ ! -e "$tunnel" ] || { echo "$tunnel is served with no external address"; return 1; }
  if [ "$OS" = Darwin ] && ! command -v lsof >/dev/null; then
    echo "no lsof to see what vibed listens on"
    return 1
  fi
  if command -v lsof >/dev/null; then
    listening=$(lsof -nP -a -p "$VIBED_PID" -iTCP -sTCP:LISTEN 2>/dev/null | awk 'NR > 1 {print $9}' || true)
    echo "vibed listens on: $listening"
    if printf '%s\n' "$listening" | grep -v -e '^$' -e '^127\.' -e '^\[::1\]' | grep -q .; then
      echo "vibed listens beyond loopback"
      return 1
    fi
  fi
  out=$(HOME="$home" target/debug/vibe --socket "$socket" key list)
  echo "keys: $out"
  case $out in *"check  vk_"*"last used"*) ;; *) echo "the key doesn't say it was used"; return 1 ;; esac
  # The log names the key that called, and never holds the key itself.
  grep -q 'mcp: key "check" called vibe_start_agent' "$WORK/mc-vibed.log" || { echo "calls aren't logged"; return 1; }
  if grep -qF "$key" "$WORK/mc-vibed.log" "$home/.local/share/vibeos/logs/vibed.log" "$home/Library/Logs/VibeOS/vibed.log" 2>/dev/null; then
    echo "the key is in a log"
    return 1
  fi
  stop_vibed
}

# The macos backend's own tests, with a bigger fork bomb than the test
# step's, still bounded so a governor that failed wouldn't take the Mac down.
s_macos_governor() {
  VIBE_FORK_BOMB_DEPTH=8 cargo test -p vibe-env --locked --test macos -- --test-threads=1 --nocapture
}

s_macos_agents() {
  local socket="$WORK/ma/run/vibed.sock" home="$WORK/ma/home" origin="$WORK/ma/origin"
  local repo="$WORK/ma/src/app" ref worktree session transcript i left run
  run="$home/Library/Application Support/VibeOS/run"
  git init -q "$origin"
  # Its setup says the PATH the agent has.
  mkdir -p "$origin/.vibe"
  echo 'echo "PATH=$PATH"' >"$origin/.vibe/setup.sh"
  git -C "$origin" add .vibe
  git -C "$origin" -c user.name=t -c user.email=t@example.com commit -q -m first
  git clone -q "$origin" "$repo"
  # The Mac's own Claude state, as the owner has it before any agent; and
  # a directory their interactive shell puts on PATH, as nvm or pyenv do.
  mkdir -p "$home/.claude/projects" "$home/.config/fish"
  echo '{}' >"$home/.claude.json"
  echo 'export PATH="$HOME/shell-bin:$PATH"' >"$home/.zshrc"
  echo 'export PATH="$HOME/shell-bin:$PATH"' >"$home/.bash_profile"
  echo 'set -gx PATH $HOME/shell-bin $PATH' >"$home/.config/fish/config.fish"
  # vibed as launchd starts a LaunchAgent: launchd's short PATH, and little
  # else in its environment.
  mkdir -p "$(dirname "$socket")"
  chmod 700 "$(dirname "$socket")"
  env -i HOME="$home" USER="$USER" LOGNAME="$USER" PATH=/usr/bin:/bin:/usr/sbin:/sbin \
    VIBE_CLAUDE="$ROOT/crates/vibe-env/src/stand-in.sh" \
    target/debug/vibed --socket "$socket" 2>"$WORK/ma-vibed.log" &
  VIBED_PID=$! VIBED_SOCKET=$socket
  echo "$VIBED_PID" >>"$WORK/pids"
  wait_file "$socket"
  out=$(rpc \
    "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"projects.add\",\"params\":{\"path\":\"$repo\"}}" \
    '{"jsonrpc":"2.0","id":2,"method":"profiles.update","params":{"project":"app","profile":"software-eng-mac","memory":1073741824}}' \
    '{"jsonrpc":"2.0","id":4,"method":"agents.start","params":{"project":"app","profile":"software-eng-mac","prompt":"fork 3 commit question"}}')
  echo "start: $out"
  case $out in *'"error"'*) echo "a call failed"; return 1 ;; esac
  expect '"environment":"macos"' "a macos agent"
  ref=$(json_field "$out" 4 ref)
  worktree=$(json_field "$out" 4 worktree)
  session=$(json_field "$out" 4 session_id)
  echo "agent $ref in $worktree, session $session"
  grep -q "macos agents run natively" "$WORK/ma-vibed.log"
  # Its PATH is the login shell's, not launchd's.
  grep "^PATH=" "$home/VibeOS/agents/$ref/setup.log"
  grep -q "^PATH=$home/shell-bin:" "$home/VibeOS/agents/$ref/setup.log" ||
    { echo "the agent doesn't have the login shell's PATH"; return 1; }
  # In the Mac's own ~/.claude, not one of its own.
  transcript="$home/.claude/projects/$(printf '%s' "$worktree" | tr -c 'A-Za-z0-9' '-')/$session.jsonl"
  wait_file "$transcript"
  [ ! -e "$home/VibeOS/agents/$ref/claude" ] || { echo "it has a Claude home of its own"; return 1; }
  wait_for "$transcript" AskUserQuestion
  for i in $(seq 1 200); do
    [ "$(git -C "$repo" log -1 --format=%s "$ref")" = work ] && break
    sleep 0.1
  done
  out=$(rpc '{"jsonrpc":"2.0","id":5,"method":"agents.list","params":{}}')
  echo "asking: $out"
  expect '"state":"question"' "the question, from the transcript"

  # Awake, it holds the Mac from idle sleep, as pmset shows for this
  # vibed's pid; asleep once idle (SIGSTOP), it lets go; a send wakes it.
  held() { pmset -g assertions | grep -E "pid $VIBED_PID\(.*VibeOS: agents are awake" >/dev/null; }
  for i in $(seq 1 100); do held && break; sleep 0.1; done
  held || { echo "no IOPM assertion from vibed (pid $VIBED_PID) while an agent is awake"; pmset -g assertions; return 1; }
  out=$(rpc '{"jsonrpc":"2.0","id":51,"method":"limits.set","params":{"idle_debounce":1}}')
  case $out in *'"error"'*) echo "limits.set failed: $out"; return 1 ;; esac
  for i in $(seq 1 150); do
    out=$(rpc '{"jsonrpc":"2.0","id":52,"method":"agents.list","params":{}}')
    case $out in *'"state":"asleep"'*) break ;; esac
    sleep 0.1
  done
  echo "idle: $out"
  expect '"state":"asleep"' "the agent asleep once idle"
  for i in $(seq 1 100); do held || break; sleep 0.1; done
  if held; then echo "the IOPM assertion outlived every awake agent"; pmset -g assertions; return 1; fi
  out=$(rpc "{\"jsonrpc\":\"2.0\",\"id\":53,\"method\":\"agents.send\",\"params\":{\"ref\":\"$ref\",\"keys\":[\"r\",\"e\",\"d\",\"Enter\"]}}")
  echo "send: $out"
  expect '"woke":true' "the wake"
  wait_for "$transcript" "User answered: red"
  out=$(rpc '{"jsonrpc":"2.0","id":54,"method":"limits.set","params":{"idle_debounce":600}}')

  # Four processes against an edited limit of two: paused at once.
  out=$(rpc '{"jsonrpc":"2.0","id":6,"method":"limits.set","params":{"max_processes":2}}')
  case $out in *'"error"'*) echo "limits.set failed: $out"; return 1 ;; esac
  for i in $(seq 1 200); do
    out=$(rpc '{"jsonrpc":"2.0","id":7,"method":"agents.list","params":{}}')
    case $out in *'"state":"paused"'*) break ;; esac
    sleep 0.1
  done
  echo "over its limit: $out"
  expect '"state":"paused"' "the pause"
  grep -q "$ref: paused with" "$WORK/ma-vibed.log"
  [ -f "$home/VibeOS/agents/$ref/governor.json" ] || { echo "the pause isn't kept"; return 1; }

  git -C "$worktree" push -q origin "$ref"
  out=$(rpc "{\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"agents.delete\",\"params\":{\"ref\":\"$ref\"}}")
  echo "delete: $out"
  expect '"state":"exited"' "the deleted agent"
  [ ! -e "$worktree" ] || { echo "the worktree is left"; return 1; }
  [ ! -e "$(dirname "$transcript")" ] || { echo "its transcripts are left"; return 1; }
  if grep -q "$worktree" "$home/.claude.json"; then echo "Claude still trusts its worktree"; return 1; fi
  left=$(find "$home/VibeOS/worktrees" "$home/VibeOS/agents" "$run" "$home/.claude/projects" -mindepth 1 2>/dev/null)
  [ -z "$left" ] || { echo "left behind: $left"; return 1; }
  git -C "$repo" rev-parse --verify --quiet "refs/heads/$ref" >/dev/null || { echo "the branch went"; return 1; }
  stop_vibed
}

s_swift_app_build() { swift build --package-path swift/VibeOS; }

# The Swift half of the contract test runs here; the live suite stays off.
s_swift_app_test() { env -u VIBE_TEST_SOCKET swift test --package-path swift/VibeOS; }

s_swift_vm_build() { swift build --package-path swift/vibe-vm; }

s_swift_vm_test() { swift test --package-path swift/vibe-vm; }

s_sign() {
  scripts/sign-macos.sh
  [ -s build/VibeOS.app/Contents/Resources/AppIcon.icns ] || { echo "the app has no icon"; return 1; }
}

# The app icon's checked-in iconset: every size iconutil wants, at its size,
# with AppIcon.svg's ink in the corner and its cyan at 40%,50%. That much is
# python3's standard library, so it runs on Linux too; on a Mac the iconset
# is also rendered afresh from the SVG and compared, within a hair.
s_app_icon() {
  local set=swift/VibeOS/Support/AppIcon.iconset name size got
  for name in 16x16:16 16x16@2x:32 32x32:32 32x32@2x:64 128x128:128 128x128@2x:256 \
    256x256:256 256x256@2x:512 512x512:512 512x512@2x:1024; do
    size=${name#*:}
    name=icon_${name%:*}.png
    [ -s "$set/$name" ] || { echo "no $name"; return 1; }
    got=$(python3 - "$set/$name" <<'PY'
import struct, sys, zlib
d = open(sys.argv[1], "rb").read()
i, idat = 8, b""
while i < len(d):
    n, t = struct.unpack(">I4s", d[i:i + 8])
    if t == b"IHDR":
        w, h, depth, kind = struct.unpack(">IIBB", d[i + 8:i + 18])
    if t == b"IDAT":
        idat += d[i + 8:i + 8 + n]
    i += 12 + n
bpp = {(8, 2): 3, (8, 6): 4}[depth, kind]
stride, raw, prev, rows = w * bpp, zlib.decompress(idat), bytearray(w * bpp), []
for y in range(h // 2 + 1):  # rows down to the one sampled
    f, row = raw[y * (stride + 1)], bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
    for x in range(stride):
        a, b = row[x - bpp] if x >= bpp else 0, prev[x]
        c = prev[x - bpp] if x >= bpp else 0
        p = a + b - c
        row[x] = (row[x] + [0, a, b, (a + b) // 2,
                            a if abs(p - a) <= min(abs(p - b), abs(p - c)) else b if abs(p - b) <= abs(p - c) else c][f]) & 255
    rows.append(row)
    prev = row
px = lambda x, y: "%02x%02x%02x" % tuple(rows[y][x * bpp:x * bpp + 3])
print(w, h, px(0, 0), px(w * 2 // 5, h // 2))
PY
)
    [ "$got" = "$size $size 0d1117 56cfe1" ] || { echo "$name is $got, not $size $size 0d1117 56cfe1"; return 1; }
  done
  if [ "$OS" = Darwin ]; then
    swift scripts/app-icon.swift --check swift/VibeOS/Support/AppIcon.svg "$set"
  fi
  echo "the iconset is complete"
}

# Every surface, drawn offscreen on canned data in light and dark. The app
# exits non-zero if one comes out blank; each must differ in dark.
s_surfaces() {
  local out="$LOGS/surfaces" app="$ROOT/build/VibeOS.app/Contents/MacOS/VibeOS" name look n=0
  rm -rf "$out"
  "$app" --render-surfaces "$out" || { echo "a surface didn't draw"; return 1; }
  [ -s "$out/manifest.txt" ] || { echo "no manifest"; return 1; }
  for name in menu menu-status menu-welcome menu-welcome-fresh menu-install-move menu-install-login-items \
    menu-quiet menu-offline menu-bar new-agent new-agent-empty add-host add-host-local-network \
    add-host-done settings-projects settings-keys settings-hosts settings-toolbox settings-limits \
    settings-general settings-project settings-projects-first-run new-key new-key-made external-address; do
    for look in light dark; do
      [ -s "$out/$name-$look.png" ] || { echo "no $name-$look.png"; return 1; }
    done
    if cmp -s "$out/$name-light.png" "$out/$name-dark.png"; then
      echo "$name looks the same in light and dark"
      return 1
    fi
    n=$((n + 1))
  done
  echo "$n surfaces drew in light and dark, in build/check/surfaces/"
}

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

s_base_image() { scripts/base-image.sh build/base-image; }

s_linux_vm() {
  local marker="$WORK/linux-marker" t token=${CLAUDE_CODE_OAUTH_TOKEN:-}
  if [ "$(sysctl -n kern.hv_vmm_present 2>/dev/null || echo 0)" = 1 ]; then
    echo "this Mac is a VM, so it can't run linux agents"
    return $NOT_APPLICABLE
  fi
  if [ -z "$token" ]; then
    token=$(security find-generic-password -s VibeOS -a claude-setup-token -w 2>/dev/null) || {
      echo "no Claude token: run \`target/debug/vibe setup-token\` or set CLAUDE_CODE_OAUTH_TOKEN"
      return 1
    }
  fi
  rm -rf "$marker"
  cargo test -p vibe-env --locked --test linux_conformance --no-run
  cargo test -p vibed --locked --test linux_vm --no-run
  cargo test -p vibed --locked --test toolbox_vm --no-run
  cargo test -p vibed --locked --test sleep --no-run
  settle
  export VIBE_VM_BIN="$ROOT/build/vibe-vm" VIBE_BASE_RELEASES="file://$ROOT/build/base-image"
  export VIBE_VM_MARKER="$marker"
  cargo test -p vibe-env --locked --test linux_conformance -- --test-threads=1 --nocapture
  CLAUDE_CODE_OAUTH_TOKEN="$token" \
    cargo test -p vibed --locked --test linux_vm -- --test-threads=1 --nocapture
  cargo test -p vibed --locked --test toolbox_vm -- --test-threads=1 --nocapture
  cargo test -p vibed --locked --test sleep -- --test-threads=1 --nocapture on_real_vms
  for t in linux_conformance linux_agent toolbox_vm sleep_scenario; do
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

s_menu_live() {
  # Short: an agent's session socket is under its home's Library.
  local socket="$WORK/ml/run/vibed.sock" repo="$WORK/ml/src/app" marker="$WORK/ml-marker"
  git init -q "$repo"
  git -C "$repo" -c user.name=t -c user.email=t@example.com commit -q --allow-empty -m first
  export VIBE_TEST_BACKEND="$ROOT/crates/vibe-env/src/stand-in.sh"
  start_vibed "$socket" "$WORK/ml-vibed.log"
  rm -f "$marker"
  VIBE_TEST_SOCKET="$VIBED_SOCKET" VIBE_TEST_REPO="$repo" VIBE_TEST_MARKER="$marker" \
    swift test --package-path swift/VibeOS --filter LiveMenuTests
  [ -f "$marker" ] || { echo "the live menu test was skipped, not run"; return 1; }
  stop_vibed
}

# terminal_tabs: how many tabs Terminal has. Bounded, as a Terminal that
# isn't answering would otherwise hold osascript for two minutes.
terminal_tabs() {
  osascript \
    -e 'with timeout of 10 seconds' -e 'tell application "Terminal"' -e 'set n to 0' \
    -e 'repeat with w in windows' -e 'set n to n + (count of tabs of w)' -e 'end repeat' \
    -e 'return n' -e 'end tell' -e 'end timeout'
}

# terminal_up: starts Terminal if it isn't running, through Launch Services
# as Finder would, and waits up to 30 s for it to answer Apple events.
terminal_up() {
  local i
  # Outside the logged-in GUI session (over SSH, say) Terminal can start
  # without ever answering.
  if [ "$(launchctl managername 2>/dev/null)" != Aqua ]; then
    echo "not in the logged-in GUI session (launchctl managername: $(launchctl managername 2>/dev/null)); run this from Terminal on the Mac"
    return 1
  fi
  open -b com.apple.Terminal
  for i in $(seq 1 60); do
    osascript -e 'with timeout of 2 seconds' -e 'tell application "Terminal" to count windows' \
      -e 'end timeout' >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  echo "Terminal is running but doesn't answer Apple events after 30 s"
  return 1
}

# attached <home> <ref>: the line `vibe ls` gives <ref> once one client is
# attached, waiting up to 20 s for it.
attached() {
  local i line
  for i in $(seq 1 200); do
    line=$(ls_sessions "$1" | grep "^$2 " || true)
    case $line in *" 1 attached ("*) echo "$line"; return 0 ;; esac
    sleep 0.1
  done
  echo "$2 never had one client attached: $line"
  return 1
}

s_terminal_attach() {
  local socket="$WORK/ta/run/vibed.sock" home="$WORK/ta/home" repo="$WORK/ta/src/app"
  local app="$ROOT/build/VibeOS.app/Contents/MacOS/VibeOS" ref before after line tty
  git init -q "$repo"
  git -C "$repo" -c user.name=t -c user.email=t@example.com commit -q --allow-empty -m first
  export VIBE_TEST_BACKEND="$ROOT/crates/vibe-env/src/stand-in.sh"
  start_vibed "$socket" "$WORK/ta-vibed.log"
  out=$(rpc \
    "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"projects.add\",\"params\":{\"path\":\"$repo\"}}" \
    '{"jsonrpc":"2.0","id":2,"method":"profiles.update","params":{"project":"app","profile":"software-eng-mac","memory":1073741824}}' \
    '{"jsonrpc":"2.0","id":4,"method":"agents.start","params":{"project":"app","profile":"software-eng-mac","prompt":"question"}}')
  echo "start: $out"
  case $out in *'"error"'*) echo "a call failed"; return 1 ;; esac
  ref=$(json_field "$out" 4 ref)
  [ -n "$ref" ] || { echo "no agent"; return 1; }
  export VIBE_SOCKET="$VIBED_SOCKET" VIBE_CLI="$ROOT/target/debug/vibe"
  terminal_up
  before=$(terminal_tabs)
  # Two clicks in a row, as fast as one process makes them.
  "$app" --attach "$ref" --attach "$ref"
  line=$(attached "$home" "$ref")
  echo "after two clicks: $line"
  tty=$(printf '%s\n' "$line" | sed -n 's/.*1 attached (\(.*\)).*/\1/p')
  after=$(terminal_tabs)
  echo "Terminal tabs: $before before, $after after"
  [ "$after" -eq $((before + 1)) ] || { echo "two clicks should open exactly one tab"; return 1; }
  # A third, from a process that didn't open the tab, raises it by its tty.
  out=$("$app" --attach "$ref")
  echo "$out"
  expect "raised its tab" "the raise"
  line=$(attached "$home" "$ref")
  [ "$(terminal_tabs)" -eq "$after" ] || { echo "the third click opened another tab"; return 1; }
  # Detach by ending the attach, then close its window, which then asks
  # nothing.
  pkill -f "$VIBE_CLI --socket $VIBED_SOCKET attach $ref" || true
  sleep 1
  osascript - "$tty" <<'EOF' || true
on run argv
  with timeout of 10 seconds
  tell application "Terminal"
    repeat with w in (every window)
      repeat with t in tabs of w
        if tty of t is item 1 of argv then
          close w
          exit repeat
        end if
      end repeat
    end repeat
  end tell
  end timeout
end run
EOF
  out=$(rpc "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"agents.delete\",\"params\":{\"ref\":\"$ref\",\"force\":true}}")
  case $out in *'"error"'*) echo "the delete failed: $out"; return 1 ;; esac
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

# vibe doctor against a live vibed, on this machine as it is: whatever it
# finds, a line for every check, and exit 0 (ready) or 1 (one needs you).
s_doctor() {
  local rc=0 lines
  start_vibed "$WORK/doctor/run/vibed.sock" "$WORK/doctor-vibed.log"
  out=$(HOME="$WORK/doctor/home" target/debug/vibe --socket "$VIBED_SOCKET" doctor) || rc=$?
  echo "$out"
  echo "exit $rc"
  [ $rc -eq 0 ] || [ $rc -eq 1 ] || { echo "vibe doctor exited $rc"; return 1; }
  lines=$(printf '%s\n' "$out" | grep -cE '^(ok|!!|--|\.\.)  ')
  # git, gh, git through gh, Claude Code and its login, Tailscale; and on a
  # Mac, system sleep (and the Linux images, with vibe-vm beside vibed).
  [ "$lines" -ge 6 ] || { echo "$lines checks, not 6 or more"; return 1; }
  case $out in
    *"This host is ready for agents."* | *"the rest is under way."*) [ $rc -eq 0 ] ;;
    *" need you."* | *" needs you."*) [ $rc -eq 1 ] ;;
    *) echo "no summary"; return 1 ;;
  esac
  stop_vibed
}

# The release's tools and settings, which need no Mac.
s_release_tools() {
  python3 -I scripts/sparkle.py self-test
  python3 -I - "$ROOT" <<'PY'
import plistlib, re, sys
root = sys.argv[1]
read = lambda p: open(f"{root}/{p}", encoding="utf-8").read()
fail = []

# The release workflow runs on version tags alone: never a pull request,
# a branch push, a schedule or by hand.
wf = read(".github/workflows/release.yml")
lines = [l.split(" #")[0].rstrip() for l in wf.splitlines() if l.strip() and not l.lstrip().startswith("#")]
on = lines.index("on:")
block = []
for l in lines[on + 1:]:
    if not l.startswith(" "):
        break
    block.append(l)
keys = lambda indent: [l.strip().rstrip(":") for l in block if len(l) - len(l.lstrip()) == indent and l.strip().endswith(":")]
if keys(2) != ["push"] or keys(4) != ["tags"]:
    fail.append(f"release.yml runs on {keys(2)} {keys(4)}, not push: tags alone")
tags = [l.strip()[2:].strip('"') for l in block if l.strip().startswith("- ")]
if not tags or not all(t.startswith("v") for t in tags):
    fail.append(f"release.yml's tags aren't version tags: {tags}")
for word in ["pull_request", "branches", "workflow_dispatch", "schedule"]:
    if word in "\n".join(block):
        fail.append(f"release.yml's trigger has {word}")
# The CI workflow, for its part, never runs on tags.
ci = read(".github/workflows/ci.yml")
if "tags" in ci.split("jobs:")[0]:
    fail.append("ci.yml runs on tags")

# The secrets it reads are the ones the release needs, and none is printed:
# each one echoed or printed goes straight into a pipe or a file.
want = {"MACOS_CERT_P12", "MACOS_CERT_PASSWORD", "APPLE_TEAM_ID", "NOTARY_KEY_ID", "NOTARY_ISSUER_ID",
        "NOTARY_KEY_P8", "APPLE_ID", "APPLE_APP_PASSWORD", "SPARKLE_PRIVATE_KEY"}
got = set(re.findall(r"secrets\.(\w+)", wf))
if got != want:
    fail.append(f"release.yml reads secrets {sorted(got)}, not {sorted(want)}")
secret = {"MACOS_CERT_P12", "MACOS_CERT_PASSWORD", "NOTARY_KEY_P8", "APPLE_APP_PASSWORD", "SPARKLE_PRIVATE_KEY"}
for name in [".github/workflows/release.yml", "scripts/release-macos.sh"]:
    for n, l in enumerate(read(name).splitlines(), 1):
        if l.lstrip().startswith("#"):
            continue
        for s in secret:
            m = re.search(r"\b(echo|printf|print|say|cat)\b[^|>]*\$\{?" + s + r"\b", l)
            if m and not re.search(r"[|>]", l[m.end():]):
                fail.append(f"{name}:{n} prints {s}")
        if re.search(r"set -x|set -o xtrace", l):
            fail.append(f"{name}:{n} traces commands, secrets and all")

# The versions agree: Cargo's, the app's, and its build's.
cargo = re.search(r'^version = "(.+)"$', read("Cargo.toml"), re.M).group(1)
info = plistlib.loads(read("swift/VibeOS/Support/Info.plist").encode())
for key in ["CFBundleShortVersionString", "CFBundleVersion"]:
    if info[key] != cargo:
        fail.append(f"Info.plist's {key} is {info[key]}, Cargo.toml's version {cargo}")
# The usage descriptions macOS shows when it asks: Apple Events, and Local
# Network, without which a Linux host on the local network can't be added
# (issue #39).
for key in ["NSAppleEventsUsageDescription", "NSLocalNetworkUsageDescription"]:
    if not str(info.get(key, "")).strip():
        fail.append(f"Info.plist lacks {key}")
# The base release vibed boots is the one base-image.sh builds.
images = re.search(r"pub const BASE_RELEASE: u32 = (\d+);", read("crates/vibe-env/src/images.rs")).group(1)
script = re.search(r"N=\$\{BASE_RELEASE:-(\d+)\}", read("scripts/base-image.sh")).group(1)
if images != script:
    fail.append(f"images.rs boots base {images}, base-image.sh builds {script}")

# vibed's LaunchAgent: the label the app registers, the vibed inside it.
label = re.search(r'label = "([^"]+)"', read("swift/VibeOS/Sources/VibeMenu/Install.swift")).group(1)
agent = plistlib.loads(read(f"swift/VibeOS/Support/{label}.plist").encode())
if agent.get("Label") != label or agent.get("BundleProgram") != "Contents/MacOS/vibed":
    fail.append(f"the LaunchAgent is {agent.get('Label')} running {agent.get('BundleProgram')}")
if info["CFBundleIdentifier"] not in agent.get("AssociatedBundleIdentifiers", []):
    fail.append("the LaunchAgent isn't associated with the app")
if agent.get("KeepAlive") is not True or agent.get("RunAtLoad") is not True:
    fail.append("the LaunchAgent doesn't keep vibed running from login")
for path, key in [("swift/vibe-vm/vibe-vm.entitlements", "com.apple.security.virtualization"),
                  ("swift/VibeOS/Support/VibeOS.entitlements", "com.apple.security.automation.apple-events")]:
    if plistlib.loads(read(path).encode()).get(key) is not True:
        fail.append(f"{path} lacks {key}")

for f in fail:
    print(f)
if fail:
    sys.exit(1)
print("the release workflow, its secrets, the versions, the LaunchAgent and the entitlements check out")
PY
}

# A release build as scripts/release-macos.sh makes it, ad hoc signed, with
# Sparkle and a key made for the check; then everything in its DMG.
s_release_build() {
  local out="$WORK/release" mnt="$WORK/release-dmg" key public version dmg app b archs plist a
  version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
  key=$(python3 -I scripts/sparkle.py keygen)
  public=$(SPARKLE_PRIVATE_KEY="$key" python3 -I scripts/sparkle.py public-key)
  env -u MACOS_SIGN_IDENTITY -u NOTARY_KEY_ID -u APPLE_ID -u RELEASE_REQUIRE SPARKLE_PRIVATE_KEY="$key" \
    scripts/release-macos.sh --out "$out" --tag "v$version"
  dmg="$out/VibeOS-$version.dmg"
  for f in "$dmg" "$out/appcast.xml" "$out/SHA256SUMS"; do [ -s "$f" ] || { echo "no $f"; return 1; }; done
  (cd "$out" && shasum -a 256 -c SHA256SUMS)
  mkdir -p "$mnt"
  hdiutil attach -nobrowse -readonly -mountpoint "$mnt" "$dmg" >/dev/null
  app="$mnt/VibeOS.app"
  [ "$(readlink "$mnt/Applications")" = /Applications ] || { echo "the DMG has no Applications to drag to"; return 1; }
  for b in VibeOS vibed vibe vibe-shim vibe-vm; do
    archs=$(lipo -archs "$app/Contents/MacOS/$b")
    case " $archs " in *" arm64 "*) ;; *) echo "$b is $archs"; return 1 ;; esac
  done
  "$app/Contents/MacOS/vibed" --version | grep -qx "vibed $version"
  "$app/Contents/MacOS/vibe" --version | grep -qx "vibe $version"
  plutil -lint "$app/Contents/Library/LaunchAgents/io.github.tsconfigdotjson.VibeOS.vibed.plist"
  codesign --verify --strict --deep --verbose=2 "$app"
  codesign -d --entitlements - --xml "$app/Contents/MacOS/vibe-vm" 2>/dev/null | grep -q com.apple.security.virtualization
  codesign -d --entitlements - --xml "$app" 2>/dev/null | grep -q com.apple.security.automation.apple-events
  plist="$app/Contents/Info.plist"
  [ "$(plutil -extract CFBundleShortVersionString raw "$plist")" = "$version" ]
  [ "$(plutil -extract CFBundleVersion raw "$plist")" = "$version" ]
  [ "$(plutil -extract SUPublicEDKey raw "$plist")" = "$public" ] || { echo "SUPublicEDKey isn't the key's"; return 1; }
  plutil -extract SUFeedURL raw "$plist" | grep -q '/releases/latest/download/appcast.xml$'
  plutil -extract NSLocalNetworkUsageDescription raw "$plist" | grep -q . || { echo "Info.plist lacks NSLocalNetworkUsageDescription"; return 1; }
  [ -d "$app/Contents/Frameworks/Sparkle.framework" ] || { echo "no Sparkle.framework"; return 1; }
  otool -l "$app/Contents/MacOS/VibeOS" | grep -q '@executable_path/../Frameworks'
  if command -v cargo-zigbuild >/dev/null; then
    for a in x86_64 aarch64; do
      [ -x "$app/Contents/Resources/linux/$a/vibed" ] || { echo "no Linux vibed for $a"; return 1; }
    done
  else
    echo "no cargo-zigbuild here, so no Linux builds to look for"
  fi
  python3 -I - "$ROOT" "$out/appcast.xml" "$dmg" "$version" "$public" <<'PY'
import base64, os, sys
sys.path.insert(0, os.path.join(sys.argv[1], "scripts"))
import sparkle
_, _, appcast, dmg, version, public = sys.argv
got = sparkle.read_appcast(open(appcast).read())
assert got["version"] == got["short"] == version, got
assert got["url"].endswith(f"/releases/download/v{version}/VibeOS-{version}.dmg"), got["url"]
assert got["length"] == os.path.getsize(dmg), got
assert sparkle.verify(base64.b64decode(public), open(dmg, "rb").read(), base64.b64decode(got["signature"]))
print("the appcast names this DMG, and its signature verifies")
PY
  # The app as shipped launches, loading Sparkle from its bundle, and draws.
  "$app/Contents/MacOS/VibeOS" --render-surfaces "$WORK/release-surfaces" --surface menu-welcome
  [ -s "$WORK/release-surfaces/menu-welcome-light.png" ]
  hdiutil detach "$mnt" >/dev/null
}

s_linux_host() {
  cargo test -p vibe-cli --test linux_host --locked -- a_linux_host an_unreachable external_access --test-threads=1
}

s_real_host() {
  if [ -z "${VIBE_TEST_LINUX_HOST:-}" ]; then
    echo "no real Linux server: set VIBE_TEST_LINUX_HOST to an SSH destination that logs in with a key"
    return $NOT_APPLICABLE
  fi
  local bundle=${VIBE_HOST_BUNDLE:-} arch
  arch=$(ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -- "$VIBE_TEST_LINUX_HOST" uname -m)
  case $arch in amd64) arch=x86_64 ;; arm64) arch=aarch64 ;; esac
  if [ -z "$bundle" ]; then
    bundle="$WORK/host-bundle"
    scripts/host-bundle.sh --out "$bundle" --target "$arch"
  fi
  VIBE_HOST_BUNDLE="$bundle" cargo test -p vibe-cli --test linux_host --locked -- real_linux_host --ignored --nocapture
  real_host_cgroups "$VIBE_TEST_LINUX_HOST" "$arch" "$bundle"
}

# External access through the real server, with a real certificate and the
# Mac's real ssh tunnel. It reuses the real-host step's bundle when that ran.
s_real_external() {
  if [ -z "${VIBE_TEST_LINUX_HOST:-}" ] || [ -z "${VIBE_TEST_EXTERNAL_DOMAIN:-}" ]; then
    echo "no public Linux server: set VIBE_TEST_LINUX_HOST to an SSH destination that logs in with a key, and VIBE_TEST_EXTERNAL_DOMAIN to a DNS name pointing at it, with port 443 open"
    return $NOT_APPLICABLE
  fi
  local bundle=${VIBE_HOST_BUNDLE:-} arch
  # The real-host step's, when it built one.
  if [ -z "$bundle" ] && [ -d "$WORK/host-bundle" ]; then bundle="$WORK/host-bundle"; fi
  if [ -z "$bundle" ]; then
    arch=$(ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -- "$VIBE_TEST_LINUX_HOST" uname -m)
    case $arch in amd64) arch=x86_64 ;; arm64) arch=aarch64 ;; esac
    bundle="$WORK/host-bundle"
    scripts/host-bundle.sh --out "$bundle" --target "$arch"
  fi
  # Port 443 for vibed, which runs as the server's user (PRD: External
  # access): if it can't have it, the test fails saying how to allow it.
  VIBE_HOST_BUNDLE="$bundle" cargo test -p vibe-cli --test linux_host --locked -- real_external_access --ignored --nocapture
}

# real_host_cgroups <ssh destination> <arch> <bundle>: the cgroups backend's
# tests on the server's own cgroups, built for it statically as the bundle
# is, run there with the bundle's vibe-shim, and removed after.
real_host_cgroups() {
  local dest=$1 arch=$2 bundle=$3 target json bin dir rc
  local ssh_opts=(-o BatchMode=yes -o StrictHostKeyChecking=accept-new)
  command -v cargo-zigbuild >/dev/null || {
    echo "the cgroups tests are built for the server with cargo-zigbuild and zig (brew install zig; cargo install cargo-zigbuild)"
    return 1
  }
  [ -x "$bundle/$arch/vibe-shim" ] || { echo "no $bundle/$arch/vibe-shim"; return 1; }
  target=$arch-unknown-linux-musl
  rustup target add "$target"
  json=$(cargo zigbuild --locked --release --target "$target" -p vibe-env --test cgroups --message-format=json)
  bin=$(printf '%s\n' "$json" | sed -n 's/.*"name":"cgroups".*"executable":"\([^"]*\)".*/\1/p' | tail -n 1)
  [ -n "$bin" ] && [ -f "$bin" ] || { echo "cargo built no cgroups test for $target"; return 1; }
  dir=$(ssh "${ssh_opts[@]}" -- "$dest" 'mktemp -d /tmp/vibe-cgroups.XXXXXX')
  case $dir in /tmp/vibe-cgroups.*) ;; *) echo "no temp dir on $dest: $dir"; return 1 ;; esac
  rc=0
  scp -q "${ssh_opts[@]}" "$bin" "$dest:$dir/cgroups" &&
    scp -q "${ssh_opts[@]}" "$bundle/$arch/vibe-shim" "$dest:$dir/vibe-shim" &&
    ssh "${ssh_opts[@]}" -- "$dest" \
      "cd $dir && chmod +x cgroups vibe-shim && VIBE_TEST_CGROUPS=1 VIBE_SHIM=$dir/vibe-shim ./cgroups --test-threads=1" ||
    rc=$?
  ssh "${ssh_opts[@]}" -- "$dest" "rm -rf $dir" || true
  return $rc
}

# The Linux host's cgroups backend on this machine's own cgroups, where it
# has them. Every test agent's memory limit is tens of MB, and the tests
# keep to slices of their own, which they stop when they're done.
s_cgroups() {
  if [ "$OS" != Linux ]; then
    echo "cgroups are Linux's: on a Mac the real-host step runs these on a Linux server"
    return $NOT_APPLICABLE
  fi
  if ! systemctl --user show --property=Version >/dev/null 2>&1; then
    echo "no systemd user manager here to make agents' cgroups through"
    return $NOT_APPLICABLE
  fi
  VIBE_TEST_CGROUPS=1 cargo test -p vibe-env --test cgroups --locked -- --test-threads=1
  VIBE_TEST_CGROUPS=1 cargo test -p vibed --test agents --locked -- --test-threads=1 a_linux_agent_on_a_linux_host
  VIBE_TEST_CGROUPS=1 cargo test -p vibed --test sleep --locked -- --test-threads=1 on_a_linux_hosts_cgroups
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
step doctor "vibe doctor against a live vibed, on this machine" s_doctor build
step registry "registry via vibe rpc survives a vibed restart" s_registry build
step sessions "a session outlives a killed vibed and is re-adopted" s_sessions build
step agents "agents start, outlive vibed, and delete to nothing but the branch" s_agents build
step mcp "MCP: no key refused; VibeOS's round over vibe mcp, start to delete" s_mcp build
step linux-host "a Linux host over SSH: install, link, an agent via MCP, a pulled network" s_linux_host build
step real-host "a real Linux server over SSH (VIBE_TEST_LINUX_HOST), its cgroups too" s_real_host build
step external "external access via a real server, Let's Encrypt and ssh -R (VIBE_TEST_EXTERNAL_DOMAIN)" s_real_external build
step cgroups "the Linux host's backend: conformance on real cgroups, a memory hog, an agent" s_cgroups build
mac_step macos-governor "the macos backend: conformance, fork bomb, memory, nohup, CPU, disk" s_macos_governor build
mac_step macos-agents "a macos agent on the governor: Mac's ~/.claude, a limit pause, delete" s_macos_agents build
mac_step swift-app-build "swift build swift/VibeOS" s_swift_app_build
mac_step swift-app-test "swift test swift/VibeOS (Swift contract tests)" s_swift_app_test swift-app-build
mac_step swift-vm-build "swift build swift/vibe-vm" s_swift_vm_build
mac_step swift-vm-test "swift test swift/vibe-vm" s_swift_vm_test swift-vm-build
step app-icon "the app icon: the checked-in iconset, every size, as AppIcon.svg renders" s_app_icon
step release-tools "the release: Sparkle's signing, tag-only CI and its secrets, versions" s_release_tools
mac_step sign "ad hoc signing; vibe-vm gets the virtualization entitlement" s_sign swift-app-build swift-vm-build
mac_step surfaces "every surface draws on canned data, in light and dark" s_surfaces sign
mac_step vm-check "vibe-vm check, signed and unsigned" s_vm_check sign
mac_step vm-cycle "VMs boot, save and restore; vibe-guest's question survives" s_vm_cycle build sign test-guest
mac_step base-image "the Linux base image: kernel, initrd, Debian image" s_base_image
mac_step linux-vm "linux agents in VMs: conformance, Claude on Opus across a sleep, the toolbox" s_linux_vm build sign base-image
mac_step live-contract "Swift client against a live vibed" s_live_contract build swift-app-test
mac_step menu-live "the menu follows a live agent through vibed's events" s_menu_live build swift-app-test
mac_step app-launch "VibeOS.app launches and says hello to vibed" s_app_launch build sign
mac_step terminal-attach "two clicks give one Terminal tab attached to the agent" s_terminal_attach build sign
mac_step release-build "a release DMG, ad hoc: the bundle, Sparkle, the appcast, a launch" s_release_build release-tools sign

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
