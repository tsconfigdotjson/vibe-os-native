#!/usr/bin/env bash
# A real Linux host's state, the same after a test as before it:
#
#   scripts/keep-host-state.sh DEST -- COMMAND...
#
# A test of a real host runs a Mac of its own against it, which can change the host's
# external address. So this notes that address and the API keys there before COMMAND,
# and after it, however it ends, including by a signal: puts the address back as it
# was, revokes any key left under a real test's name, and says if the address isn't
# back, a key has gone or an unknown key appeared. Only SIGKILL can stop that, and what
# it found is the first thing it prints, to put back by hand.
#
# It exits with COMMAND's status, or 1 if the host isn't as it was. If it can't tell
# how the host is before, it runs nothing. The real-host tests refuse to run without
# it: this sets VIBE_TEST_HOST_STATE_KEPT for them.
#
# For this script's own tests: VIBE_SSH is the ssh to use, KEEP_HOST_URL the address to
# ask instead of https://DOMAIN/mcp, and KEEP_HOST_CURL_ARGS what curl needs to.
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
test_keys="real-host-test external-test"

# What a line of `vibe key` says is its prefix, last on the line as a name could say
# anything: NAME  vk_abc…  made WHEN ...
prefixes() {
  sed -n 's/.*  \(vk_[a-zA-Z0-9_-]*\)…  made .*/\1/p'
}

# look: prints the domain, or `off`, then on a second line the keys' prefixes. A host
# with no VibeOS has neither. Fails if it can't be told.
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

# serving DOMAIN: whether it answers there at all. Without a key, that's a 401.
serving() {
  local url=${KEEP_HOST_URL:-https://$1/mcp} i
  for i in $(seq 1 30); do
    # shellcheck disable=SC2086
    serving_said=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 ${KEEP_HOST_CURL_ARGS:-} "$url") || true
    case $serving_said in 2* | 4*) return 0 ;; esac
    sleep 2
  done
  return 1
}

# A reader of this output that has gone mustn't stop this before it's put things back.
# A handler, not an ignore, as what's run should get what it would have got.
trap : PIPE

before=$(look) || {
  echo "keep-host-state: can't tell how $dest is, so nothing was run" >&2
  exit 1
}
was_domain=$(first_line "$before")
was_keys=$(second_line "$before")
was_vibed=$(vibed_version)
echo "keep-host-state: $dest before: external address $was_domain; keys: ${was_keys:-none}; vibed: ${was_vibed:-none}"

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
  local code=$? now now_domain now_keys k line name said i looked now_vibed wrong=
  # Not `trap -`: another Ctrl-C, or the terminal closing, mustn't stop this halfway.
  trap '' INT TERM HUP
  trap - EXIT
  [ "$status" -ne 0 ] || status=$code
  stop_child
  if ! now=$(look_soon); then
    echo "keep-host-state: can't tell how $dest is now. It was: external address $was_domain; keys: ${was_keys:-none}" >&2
    exit 1
  fi
  now_domain=$(first_line "$now")
  if [ "$now_domain" != "$was_domain" ]; then
    echo "keep-host-state: $dest's external address is $now_domain: putting back $was_domain"
    # More than once, as its vibed may be restarting, or just slow to answer, but not
    # for ever: signals are ignored here, so nothing else would end it.
    i=$((SECONDS + 300))
    while :; do
      said=$(ssh_to "$vibe host external local $was_domain" 2>&1) && said=
      if looked=$(look_soon); then
        now=$looked
        now_domain=$(first_line "$now")
      fi
      [ "$now_domain" != "$was_domain" ] || { said=; break; }
      [ "$SECONDS" -lt "$i" ] || break
      sleep 5
    done
    [ -z "$said" ] || echo "keep-host-state: putting it back, $dest said: $said" >&2
  fi
  if [ "$now_domain" != "$was_domain" ]; then
    wrong=1
    echo "keep-host-state: $dest's external address should be $was_domain, and is $now_domain" >&2
  elif [ "$was_domain" != off ] && ! serving "$was_domain"; then
    wrong=1
    echo "keep-host-state: $dest's external address is $was_domain, and nothing answers there: the last try got HTTP ${serving_said:-nothing}" >&2
  fi
  now_keys=$(second_line "$now")
  for k in $was_keys; do
    case " $now_keys " in
      *" $k "*) ;;
      *) wrong=1; echo "keep-host-state: $dest has lost the API key $k" >&2 ;;
    esac
  done
  for k in $now_keys; do
    case " $was_keys " in *" $k "*) continue ;; esac
    line=$(ssh_to "$vibe key" | grep -F "  $k…  made " | sed -n 1p)
    name=${line%%  "$k"…*}
    case " $test_keys " in
      *" $name "*)
        echo "keep-host-state: revoking $name, which a test left on $dest"
        ssh_to "$vibe key revoke $k" >/dev/null || {
          wrong=1
          echo "keep-host-state: can't revoke $name ($k) on $dest" >&2
        }
        ;;
      *)
        wrong=1
        echo "keep-host-state: $dest has a key it didn't have, left as it may be someone's: $name ($k)" >&2
        ;;
    esac
  done
  [ -z "$wrong" ] || exit 1
  now_vibed=$(vibed_version)
  if [ "$now_vibed" != "$was_vibed" ]; then
    # Not put back: a newer one's registry can't be read by an older one.
    echo "keep-host-state: $dest's external address and keys are as they were; its vibed isn't: it was ${was_vibed:-none} and is ${now_vibed:-none}, which a test can't undo"
  else
    echo "keep-host-state: $dest is as it was"
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
