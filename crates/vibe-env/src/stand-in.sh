#!/bin/sh
# A stand-in for Claude Code (vibe_env::testing::STAND_IN).
id= resumed=false prompt=
while [ $# -gt 0 ]; do
  case $1 in
    --session-id) id=$2; shift 2 ;;
    --resume) id=$2; resumed=true; shift 2 ;;
    --model|--effort|--permission-mode) shift 2 ;;
    --) shift; prompt=$*; break ;;
    *) shift ;;
  esac
done
cwd=$(pwd -P)
dir="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/projects/$(printf '%s' "$cwd" | tr -c 'A-Za-z0-9' '-')"
mkdir -p "$dir"
printf '{"type":"user","sessionId":"%s","cwd":"%s","resumed":%s}\n' "$id" "$cwd" "$resumed" >>"$dir/$id.jsonl"
if [ "$resumed" = true ]; then echo "Resumed $id."; fi
case " $prompt " in *" tick "*)
  ( i=0; while :; do i=$((i+1)); echo "$i" >.vibe-ticks; sleep 0.1; done ) &
esac
case " $prompt " in *" fork "*)
  n=${prompt#*fork }; n=${n%% *}; i=0
  while [ "$i" -lt "$n" ]; do sleep 600 & i=$((i+1)); done
  echo "Forked $n."
esac
case " $prompt " in *" dirty "*)
  echo scratch >notes.txt; echo "Left notes.txt."
esac
case " $prompt " in *" commit "*)
  echo work >work.txt && git add work.txt &&
    git -c user.name=agent -c user.email=agent@example.com commit -qm work && echo "Committed."
esac
case " $prompt " in *" env "*)
  echo "TMPDIR=$TMPDIR"
  echo "autoSetupRemote=$(git config push.autoSetupRemote) gc.auto=$(git config gc.auto)"
esac
case " $prompt " in *" question "*)
  echo "Red or blue?"; read -r answer; echo "You chose $answer."
  ;;
*)
  if [ -n "$prompt" ]; then echo "Working on: $prompt"; fi
esac
echo "Ready."
while read -r line; do echo "> $line"; done
