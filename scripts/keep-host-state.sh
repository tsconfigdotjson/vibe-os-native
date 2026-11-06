#!/usr/bin/env bash
# A real Linux host's state, the same after a test as before it:
#
#   scripts/keep-host-state.sh DEST -- COMMAND...
#
# A test of a real host runs a Mac of its own against it, which installs this build there
# and can change its API keys. So this notes the keys there before COMMAND, and after it,
# however it ends, including by a signal: revokes any key left under a real test's name,
# and says if a key has gone or an unknown key appeared. Only SIGKILL can stop that, and
# what it found is the first thing it prints, to put back by hand.
#
# It exits with COMMAND's status, or 1 if the host isn't as it was. If it can't tell how
# the host is before, it runs nothing. Nor does it if the host still serves External
# Address, from a build before that was retired (#143): this build going on there would
# end it, which is for its owner to choose, with `vibe host external local off` there
# once its clients have moved. The real-host tests refuse to run without this: it sets
# VIBE_TEST_HOST_STATE_KEPT for them.
#
# For this script's own tests: VIBE_SSH is the ssh to use.
set -u

dest=${1:?usage: keep-host-state.sh DEST -- COMMAND...}
shift
[ "${1:-}" = -- ] && shift
[ $# -gt 0 ] || { echo "keep-host-state: no command" >&2; exit 2; }

vibe=.local/lib/vibeos/vibe
ssh_to() {
  "${VIBE_SSH:-ssh}" -o BatchMode=yes -o ConnectTimeout=15 -o ServerAliveInterval=15 \
    -o ServerAliveCountMax=4 -- "$dest" "$1" </dev/null
}

# The names the tests in crates/vibe-cli/tests/linux_host.rs give their keys.
test_keys="real-host-test"

# What a line of `vibe key` says is its prefix, last on the line as a name could say
# anything: NAME  vk_abc…  made WHEN ...
prefixes() {
  sed -n 's/.*  \(vk_[a-zA-Z0-9_-]*\)…  made .*/\1/p'
}

# look: prints the External Address a build from before #143 says it serves, or `off`,
# then on a second line the keys' prefixes. A host with no VibeOS has neither. Fails if it
# can't be told.
look() {
  local said domain
  said=$(ssh_to "if [ -x $vibe ]; then echo --here--; $vibe host && echo --keys-- && $vibe key; else echo --none--; fi") || return 1
  case $(printf '%s\n' "$said" | sed -n 1p) in
    --none--) printf 'off\n\n'; return 0 ;;
    --here--) ;;
    *) return 1 ;;
  esac
  domain=$(printf '%s\n' "$said" | awk '$0 == "--keys--" { exit } { print }' |
    sed -n 's|^local .*external address https://\([^/ ]*\)/mcp.*|\1|p')
  printf '%s\n' "${domain:-off}"
  printf '%s\n' "$said" | awk 'seen { print } $0 == "--keys--" { seen = 1 }' | prefixes |
    sort -u | tr '\n' ' '
  printf '\n'
}

# What's installed there, for saying if a test changed it.
vibed_version() { ssh_to ".local/lib/vibeos/vibed --version" 2>/dev/null; }

first_line() { printf '%s\n' "$1" | sed -n 1p; }
second_line() { printf '%s\n' "$1" | sed -n 2p; }

# look_soon: the same, giving a host that's restarting a while to answer.
look_soon() {
  local i
  for i in $(seq 1 30); do
    look && return 0
    sleep 2
  done
  return 1
}

# A reader of this output that has gone mustn't stop this before it's put things back.
# A handler, not an ignore, as what's run should get what it would have got.
trap : PIPE

# say LINE: says it in a subshell of its own, whether anyone's reading or not. A Mac's
# stdio keeps what it couldn't write, and a shell holding that writes it ahead of the
# next thing, which `look` would then read as what the host said.
say() { (printf '%s\n' "$1") 2>/dev/null; }

before=$(look) || {
  echo "keep-host-state: can't tell how $dest is, so nothing was run" >&2
  exit 1
}
was_domain=$(first_line "$before")
if [ "$was_domain" != off ]; then
  echo "keep-host-state: $dest still serves External Address at $was_domain, and a test would install a build that doesn't, so nothing was run. Once its clients have moved, run on it: vibe host external local off" >&2
  exit 1
fi
was_keys=$(second_line "$before")
was_vibed=$(vibed_version)
say "keep-host-state: $dest before: keys: ${was_keys:-none}; vibed: ${was_vibed:-none}"

child=
stop_child() {
  [ -n "$child" ] || return 0
  kill -TERM -- "-$child" 2>/dev/null
  local i
  for i in $(seq 1 100); do
    kill -0 "$child" 2>/dev/null || break
    sleep 0.1
  done
  kill -KILL -- "-$child" 2>/dev/null
  wait "$child" 2>/dev/null
  child=
}

status=0
put_back() {
  local code=$? now now_keys k line name now_vibed wrong=
  # Not `trap -`: another Ctrl-C, or the terminal closing, mustn't stop this halfway.
  trap '' INT TERM HUP
  trap - EXIT
  [ "$status" -ne 0 ] || status=$code
  stop_child
  if ! now=$(look_soon); then
    say "keep-host-state: can't tell how $dest is now. It had keys: ${was_keys:-none}" >&2
    exit 1
  fi
  now_keys=$(second_line "$now")
  for k in $was_keys; do
    case " $now_keys " in
      *" $k "*) ;;
      *) wrong=1; say "keep-host-state: $dest has lost the API key $k" >&2 ;;
    esac
  done
  for k in $now_keys; do
    case " $was_keys " in *" $k "*) continue ;; esac
    line=$(ssh_to "$vibe key" | grep -F "  ${k}…  made " | sed -n 1p)
    name=${line%%  "$k"…*}
    case " $test_keys " in
      *" $name "*)
        say "keep-host-state: revoking $name, which a test left on $dest"
        ssh_to "$vibe key revoke $k" >/dev/null || {
          wrong=1
          say "keep-host-state: can't revoke $name ($k) on $dest" >&2
        }
        ;;
      *)
        wrong=1
        say "keep-host-state: $dest has a key it didn't have, left as it may be someone's: $name ($k)" >&2
        ;;
    esac
  done
  [ -z "$wrong" ] || exit 1
  now_vibed=$(vibed_version)
  if [ "$now_vibed" != "$was_vibed" ]; then
    # Not put back: a newer one's registry can't be read by an older one.
    say "keep-host-state: $dest's keys are as they were; its vibed isn't: it was ${was_vibed:-none} and is ${now_vibed:-none}, which a test can't undo"
  else
    say "keep-host-state: $dest is as it was"
  fi
  exit "$status"
}
trap put_back EXIT
trap "trap '' INT TERM HUP; status=130; exit 130" INT
trap "trap '' INT TERM HUP; status=143; exit 143" TERM
trap "trap '' INT TERM HUP; status=129; exit 129" HUP

# In a process group of its own, so that all of it can be stopped, and so that what's
# run isn't left behind telling the host otherwise after this has put it back.
set -m
VIBE_TEST_HOST_STATE_KEPT=1 "$@" </dev/null &
child=$!
set +m
wait "$child"
status=$?
child=
exit "$status"
