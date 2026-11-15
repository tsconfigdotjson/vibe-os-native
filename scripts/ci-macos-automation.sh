#!/usr/bin/env bash
# Why this exists: scripts/check-full.sh's terminal-attach step drives
# Terminal with Apple events, and on a GitHub-hosted macOS runner every one
# of them timed out (-1712). macOS asks the user before one app drives
# another (Privacy & Security → Automation, kept in the user's TCC
# database), and a hosted runner has nobody to click Allow. tccd asks on
# behalf of the runner's agent, /opt/hca/hosted-compute-agent, which every
# process of the job runs under; the runner image already allows it to drive
# Finder and System Events, but not Terminal.
#
# So this allows the runner's agent, and nothing else, to send Apple events
# to Terminal (com.apple.Terminal), and to no other app, in the runner
# user's TCC database: a row like the image's own allowances, which SIP
# (off on the image) lets the runner write. The repo owner approved this,
# scoped so (#171).
#
# It's run only by the macOS job in .github/workflows/ci.yml, never by
# check-full.sh, and refuses to run anywhere but a GitHub-hosted runner in
# GitHub Actions: on anyone's own Mac, that permission is theirs to give.
set -euo pipefail

say() { echo "ci-macos-automation.sh: $*" >&2; }

if [ "${GITHUB_ACTIONS:-}" != true ] || [ "${RUNNER_ENVIRONMENT:-}" != github-hosted ]; then
  say "refusing: only for a GitHub-hosted runner in GitHub Actions" \
    "(GITHUB_ACTIONS=${GITHUB_ACTIONS:-unset}, RUNNER_ENVIRONMENT=${RUNNER_ENVIRONMENT:-unset})"
  exit 1
fi
if [ "$(uname)" != Darwin ]; then
  say "refusing: only for a macOS runner (this is $(uname))"
  exit 1
fi

db="$HOME/Library/Application Support/com.apple.TCC/TCC.db"
target=com.apple.Terminal
agent_name=hosted-compute-agent
[ -w "$db" ] || { say "can't write $db"; exit 1; }

# The runner's agent: the process of that name this job runs under.
agent=
pid=$$
while [ "$pid" -gt 1 ]; do
  exe=$(ps -o comm= -p "$pid")
  if [ "${exe##*/}" = "$agent_name" ]; then
    agent=$exe
    break
  fi
  pid=$(ps -o ppid= -p "$pid" | tr -d ' ')
done
case $agent in
  "") say "this job doesn't run under a $agent_name, so there's nothing to allow; the runner image has changed"; exit 1 ;;
  /*[!A-Za-z0-9/._-]*) say "won't allow $agent: an unexpected character in its path"; exit 1 ;;
  /*) ;;
  *) say "won't allow $agent: not a full path"; exit 1 ;;
esac

# Terminal's code identity, as the image's own allowances to it record it.
known=$(sqlite3 "$db" "SELECT count(*) FROM access WHERE indirect_object_identifier = '$target'
  AND indirect_object_code_identity IS NOT NULL")
[ "$known" -gt 0 ] || { say "no allowance in $db records Terminal's code identity to copy"; exit 1; }

# A copy of one of the image's own path allowances, made the agent's and
# Terminal's.
sqlite3 "$db" "
  BEGIN;
  DROP TABLE IF EXISTS temp.allow;
  CREATE TEMP TABLE allow AS SELECT * FROM access
    WHERE service = 'kTCCServiceAppleEvents' AND client_type = 1 LIMIT 1;
  UPDATE temp.allow SET client = '$agent', csreq = NULL, auth_value = 2,
    indirect_object_identifier = '$target',
    indirect_object_code_identity = (SELECT indirect_object_code_identity FROM access
      WHERE indirect_object_identifier = '$target'
        AND indirect_object_code_identity IS NOT NULL LIMIT 1);
  DELETE FROM access WHERE service = 'kTCCServiceAppleEvents' AND client = '$agent'
    AND indirect_object_identifier = '$target';
  INSERT INTO access SELECT * FROM temp.allow;
  COMMIT;"
echo "allowed $agent to send Apple events to $target"
sqlite3 "$db" "SELECT service, client, auth_value, indirect_object_identifier FROM access
  WHERE client = '$agent' AND indirect_object_identifier = '$target'"
