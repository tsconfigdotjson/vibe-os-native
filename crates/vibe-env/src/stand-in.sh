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
transcript="$dir/$id.jsonl"
printf '{"type":"user","sessionId":"%s","cwd":"%s","resumed":%s}\n' "$id" "$cwd" "$resumed" >>"$transcript"
# record <type> <message> [<more fields>]: an entry as Claude Code writes
# one, now. Its milliseconds count entries, so no two have the same time, as
# Claude's don't.
seq=0 who=$$
record() {
  seq=$((seq+1))
  printf '{"type":"%s","sessionId":"%s","cwd":"%s","timestamp":"%s.%03dZ","uuid":"%s-%s","message":%s%s}\n' \
    "$1" "$id" "$cwd" "$(date -u +%Y-%m-%dT%H:%M:%S)" $((seq % 1000)) "$who" "$seq" "$2" "${3:-}" >>"$transcript"
}
json() { printf '"%s"' "$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g')"; }
prompt() { record user "{\"role\":\"user\",\"content\":$(json "$1")}"; }
reply() {
  record assistant "{\"id\":\"msg-$who-$seq\",\"role\":\"assistant\",\"stop_reason\":\"end_turn\",\"content\":[{\"type\":\"text\",\"text\":$(json "$1")}]}"
}
if [ "$resumed" = true ]; then echo "Resumed $id."; fi
case " $prompt " in *" tick "*)
  ( i=0; while :; do i=$((i+1)); echo "$i" >.vibe-ticks; sleep 0.1; done ) &
esac
case " $prompt " in *" fork "*)
  n=${prompt#*fork }; n=${n%% *}; i=0
  while [ "$i" -lt "$n" ]; do sleep 600 & i=$((i+1)); done
  echo "Forked $n."
esac
case " $prompt " in *" fill "*)
  n=${prompt#*fill }; n=${n%% *}
  head -c $((n * 1048576)) /dev/zero >.vibe-fill && echo "Filled $n MiB."
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
# A question is written to the transcript as Claude writes one, and so is
# every turn after it: an AskUserQuestion, its answer, and a reply to each
# line sent.
talk=false
case " $prompt " in *" question "*)
  talk=true
  prompt "$prompt"
  record assistant "{\"id\":\"msg-$$-ask\",\"role\":\"assistant\",\"stop_reason\":\"tool_use\",\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu-$$\",\"name\":\"AskUserQuestion\",\"input\":{\"questions\":[{\"question\":\"Red or blue?\",\"header\":\"Colour\",\"multiSelect\":false,\"options\":[{\"label\":\"Red\"},{\"label\":\"Blue\"}]}]}}]}"
  echo "Red or blue?"; read -r answer
  record user "{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"toolu-$$\",\"content\":$(json "User answered: $answer")}]}"
  echo "You chose $answer."
  reply "You chose $answer."
  ;;
*" talk "*|*" loop "*)
  talk=true
  echo "Working on: $prompt"
  prompt "$prompt"
  case " $prompt " in *" loop "*)
    # A /loop wakeup, as Claude Code arms one with ScheduleWakeup: firm
    # once its result gives the time it's due. It fires by the wall clock,
    # as a frozen or restored clock would make a plain sleep late.
    n=${prompt#*loop }; n=${n%% *}; due=$(($(date +%s) + n))
    record assistant "{\"id\":\"msg-$$-loop\",\"role\":\"assistant\",\"stop_reason\":\"tool_use\",\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu-$$-loop\",\"name\":\"ScheduleWakeup\",\"input\":{\"delaySeconds\":$n,\"prompt\":\"loop check\",\"reason\":\"Checking again in ${n}s.\"}}]}"
    record user "{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"toolu-$$-loop\",\"content\":\"Next wakeup scheduled (in ${n}s).\"}]}" \
      ",\"toolUseResult\":{\"scheduledFor\":${due}000,\"clampedDelaySeconds\":$n,\"wasClamped\":false}"
    echo "Loop due at $due."
    (
      who="$$-loop"
      while [ "$(date +%s)" -lt "$due" ]; do sleep 1; done
      printf '{"type":"system","subtype":"scheduled_task_fire","sessionId":"%s","cwd":"%s","timestamp":"%s.000Z","uuid":"%s-fire","taskKind":"loop","cronKind":"loop","prompt":"loop check"}\n' \
        "$id" "$cwd" "$(date -u +%Y-%m-%dT%H:%M:%S)" "$who" >>"$transcript"
      prompt "loop check"
      reply "Loop fired."
      echo "Loop fired at $(date +%s)."
    ) &
  esac
  reply "Done."
  ;;
*)
  if [ -n "$prompt" ]; then echo "Working on: $prompt"; fi
esac
echo "Ready."
while read -r line; do
  echo "> $line"
  if [ "$talk" = true ]; then prompt "$line"; reply "> $line"; fi
done
