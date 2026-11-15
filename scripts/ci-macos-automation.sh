#!/usr/bin/env bash
# For CI on a hosted macOS runner only (.github/workflows/ci.yml): lets the
# processes this job runs under send Apple events to Terminal, as
# scripts/check-full.sh's terminal-attach step needs.
#
# macOS asks the user before one app drives another (Privacy & Security →
# Automation, kept in the user's TCC database), and a hosted runner has
# nobody to click Allow. Its image allows this job's processes to drive
# Finder and System Events, but not Terminal, so every Apple event to
# Terminal waits on a prompt nobody answers and times out (-1712). tccd
# names the process it asks for (on macos-26, /opt/hca/hosted-compute-agent,
# which the job runs under), so this allows that one and each other process
# this job runs under: no other app, and nothing but Terminal. The runner's
# image has SIP off and its TCC database is the runner user's to write, as
# the image's own allowances were made there.
#
# Refuses to run anywhere but a macOS VM in GitHub Actions: on anyone's own
# Mac, that's theirs to allow.
set -euo pipefail

say() { echo "ci-macos-automation.sh: $*" >&2; }

if [ "${GITHUB_ACTIONS:-}" != true ] || [ "$(uname)" != Darwin ] ||
  [ "$(sysctl -n kern.hv_vmm_present 2>/dev/null || echo 0)" != 1 ]; then
  say "only for a hosted macOS runner in GitHub Actions"
  exit 1
fi

db="$HOME/Library/Application Support/com.apple.TCC/TCC.db"
target=com.apple.Terminal
[ -w "$db" ] || { say "can't write $db"; exit 1; }

# Terminal's code identity, as the image's own allowances to it record it.
identity=$(sqlite3 "$db" "SELECT count(*) FROM access WHERE indirect_object_identifier = '$target'
  AND indirect_object_code_identity IS NOT NULL")
[ "$identity" -gt 0 ] || { say "no allowance in $db records Terminal's code identity to copy"; exit 1; }

granted=0
pid=$$
while [ "$pid" -gt 1 ]; do
  exe=$(ps -o comm= -p "$pid")
  case $exe in
    *"'"*) say "skipping $exe: a quote in its path" ;;
    /*)
      # A row like the image's own (an allowance by path), for Terminal.
      sqlite3 "$db" "
        BEGIN;
        DROP TABLE IF EXISTS temp.allow;
        CREATE TEMP TABLE allow AS SELECT * FROM access
          WHERE service = 'kTCCServiceAppleEvents' AND client_type = 1 LIMIT 1;
        UPDATE temp.allow SET client = '$exe', csreq = NULL, auth_value = 2,
          indirect_object_identifier = '$target',
          indirect_object_code_identity = (SELECT indirect_object_code_identity FROM access
            WHERE indirect_object_identifier = '$target'
              AND indirect_object_code_identity IS NOT NULL LIMIT 1);
        DELETE FROM access WHERE service = 'kTCCServiceAppleEvents' AND client = '$exe'
          AND indirect_object_identifier = '$target';
        INSERT INTO access SELECT * FROM temp.allow;
        COMMIT;"
      echo "allowed $exe to send Apple events to Terminal"
      granted=$((granted + 1))
      ;;
  esac
  pid=$(ps -o ppid= -p "$pid" | tr -d ' ')
done
[ "$granted" -gt 0 ] || { say "found no process to allow"; exit 1; }
