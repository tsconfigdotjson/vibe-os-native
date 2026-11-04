# Recorded Claude Code transcripts

Real Claude Code session transcripts, copied from agent-tail's fixture corpus
at commit `0b5c3f6` (`fixtures/claude-code/<version>/linux/<scenario>/input/session.jsonl`),
where they were captured and redacted. agent-tail is MIT licensed, copyright
(c) 2026 agent-tail contributors.

| File | agent-tail scenario | What it is |
| --- | --- | --- |
| `seed-session.jsonl` | `2.1/linux/seed-session` | A prompt, Bash, Read and Write calls, a generated title, a cost entry |
| `ask-user.jsonl` | `2.1.311/linux/ci-claude_sigstop_wake_answer` | An `AskUserQuestion` left pending, frozen with `SIGSTOP`, woken, and answered |
| `bash-approval.jsonl` | `2.1.311/linux/ci-claude_sigstop_wake_approval` | A Bash call waiting on its permission prompt, frozen, woken, and approved |
| `done.jsonl` | `2.1.311/linux/ci-claude_sigstop_wake_done` | A finished turn, frozen, woken, and a second turn |
| `hard-kill-mid-tool.jsonl` | `2.1.312/linux/ci-claude_hard_kill_mid_tool` | A bypass-mode Bash call when Claude was killed |
| `usage-limit.jsonl` | `2.1.312/linux/ci-claude_usage_limit` | A usage-limit stop, with when it resets |
| `loop-wakeup.jsonl` | `2.1.314/linux/scheduled-wakeup` | A `/loop` `ScheduleWakeup`: pending with its due time, then fired |
| `cron-recurring.jsonl` | `2.1.314/linux/scheduled-wakeup-cron-recurring` | A recurring `CronCreate` job that stays pending after it fires |

To keep them small, strings longer than 4 KB are cut to their first 4 KB,
as agent-tail's lean parse does before reading them anyway; nothing else is
changed. `tests/fixtures.rs` replays each one a few lines at a time.

# Recorded Pi sessions

`pi/*.jsonl` are real Pi 1.4.0 sessions, copied from agent-tail's fixture
corpus at the same commit (`fixtures/pi/1.4/linux/<scenario>/input/session.jsonl`),
where they were captured and redacted.

| File | agent-tail scenario | What it is |
| --- | --- | --- |
| `plain-qa.jsonl` | `plain-qa` | A prompt and its answer |
| `tool-call.jsonl` | `tool-call` | A tool call, its result and the answer |
| `api-error.jsonl` | `api-error` | A turn that stopped at an API error |
| `interrupted.jsonl` | `interrupted` | A turn the owner interrupted |
| `usage-limit.jsonl` | `ci-pi_usage_limit` | A 429 usage-limit stop |
