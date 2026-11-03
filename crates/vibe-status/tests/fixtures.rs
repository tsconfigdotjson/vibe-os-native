//! Recorded Claude Code transcripts (`tests/fixtures/`) give the right states,
//! activities, titles, wakeups and stops, read a few lines at a time as Claude
//! appends them; and a tail started afresh on each, as after a restart of
//! `vibed`, rebuilds the same status.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use vibe_core::model::{Activity, AgentState, Certainty, StopReason, WakeupKind};
use vibe_status::{Status, Tail, Timestamp};

const ID: &str = "0b5c3f6e-0000-4000-8000-0000000000aa";
const DEBOUNCE: Duration = Duration::from_secs(60);

/// What a status should be at one point of a fixture.
struct Check {
    /// Lines of the transcript written so far.
    after_line: usize,
    /// Seconds after that line's own time.
    offset: f64,
    state: AgentState,
    activity: Activity,
    certainty: Certainty,
    /// Nothing written for the idle debounce.
    quiet: bool,
    also: fn(&Status),
}

fn check(
    after_line: usize,
    offset: f64,
    state: AgentState,
    activity: Activity,
    certainty: Certainty,
) -> Check {
    Check {
        after_line,
        offset,
        state,
        activity,
        certainty,
        quiet: offset > DEBOUNCE.as_secs_f64(),
        also: |_| {},
    }
}

fn with(mut c: Check, also: fn(&Status)) -> Check {
    c.also = also;
    c
}

fn fixture(name: &str) -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{name}.jsonl"));
    fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A line's own time, if it has one.
fn time_of(line: &str) -> Option<Timestamp> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    Timestamp::parse_rfc3339(v["timestamp"].as_str()?)
}

/// Replays `name` into a transcript as Claude would write it: the lines up
/// to each check appended, the file's mtime that of the last line with a
/// time, and the status taken that many seconds after it, as agent-tail's
/// own checkpoints are.
fn replay(name: &str, checks: &[Check]) {
    let lines = fixture(name);
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("projects/-work-app");
    fs::create_dir_all(&project).unwrap();
    let transcript = project.join(format!("{ID}.jsonl"));
    let mut tail = Tail::new(project.clone(), Path::new("/work/app"), ID, DEBOUNCE);
    let mut written = 0;
    // Before any line with a time, the first one's.
    let mut last_time = lines.iter().find_map(|l| time_of(l));
    for c in checks {
        assert!(c.after_line <= lines.len(), "{name}: {} lines", lines.len());
        if c.after_line > written {
            let mut f = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&transcript)
                .unwrap();
            for line in &lines[written..c.after_line] {
                writeln!(f, "{line}").unwrap();
                last_time = time_of(line).or(last_time);
            }
            let at = last_time.expect("a line with a time");
            f.set_modified(at.to_system_time()).unwrap();
            written = c.after_line;
            assert!(tail.poll(), "{name}: read after line {written}");
        }
        let now = Timestamp::from_secs_f64(last_time.unwrap().as_secs_f64() + c.offset);
        let s = tail.status(now);
        let what = format!("{name} after line {} at +{}s", c.after_line, c.offset);
        assert_eq!(
            (s.state(), s.activity, s.certainty, s.quiet),
            (c.state, c.activity, c.certainty, c.quiet),
            "{what}: {s:?}"
        );
        assert_eq!(s.transcript.as_deref(), Some(transcript.as_path()));
        (c.also)(&s);

        // A tail made now, as a restarted vibed makes one, reads the whole
        // transcript at once and says the same.
        let mut again = Tail::new(project.clone(), Path::new("/work/app"), ID, DEBOUNCE);
        again.poll();
        assert_eq!(again.status(now), s, "{what}: after a restart");
    }
}

#[test]
fn a_session_with_tools_a_title_and_a_permission_prompt() {
    use {Activity::*, AgentState::*, Certainty::*};
    replay(
        "seed-session",
        &[
            check(4, 0.0, Busy, Unknown, Firm),
            with(check(6, 0.0, Busy, Thinking, Firm), |s| {
                assert_eq!(
                    s.title.as_deref(),
                    Some("Add a plan file and run the tests")
                );
                assert!(s.since.is_some());
            }),
            // A tool call with no result for 30 s may be a permission prompt,
            // but it's only a guess: a long tool run looks the same, so it's
            // still at work until the screen or a longer quiet confirms it.
            with(check(24, 30.0, Busy, AwaitingApproval, Inferred), |s| {
                assert!(!s.at_rest());
                assert!(s.unconfirmed_approval());
            }),
            with(check(64, 0.0, Idle, Done, Firm), |s| {
                assert!(s.at_rest());
                assert!(s.line.is_some(), "its latest activity line");
            }),
            // Quiet for the debounce, and still done.
            with(check(67, 901.0, Idle, Done, Firm), |s| {
                assert_eq!(
                    s.title.as_deref(),
                    Some("Add a plan file and run the tests")
                );
            }),
        ],
    );
}

#[test]
fn an_ask_user_question_is_a_question_until_answered() {
    use {Activity::*, AgentState::*, Certainty::*};
    replay(
        "ask-user",
        &[
            with(check(26, 0.0, Question, AwaitingAnswer, Firm), |s| {
                assert!(s.at_rest(), "safe to sleep on");
            }),
            // Frozen and quiet well past the debounce: still asking.
            check(26, 600.0, Question, AwaitingAnswer, Firm),
            check(38, 0.0, Idle, Done, Firm),
        ],
    );
}

#[test]
fn a_bash_permission_prompt_is_an_inferred_question() {
    use {Activity::*, AgentState::*, Certainty::*};
    replay(
        "bash-approval",
        &[
            check(13, 5.0, Busy, RunningTool, Firm),
            check(13, 30.0, Busy, AwaitingApproval, Inferred),
            // Past INFERRED_QUIET (and the debounce), it's what it was: an
            // inferred prompt, now a question, which sleeps only after the
            // longer quiet the caller holds it to.
            with(
                check(13, 400.0, Question, AwaitingApproval, Inferred),
                |s| {
                    assert!(!s.at_rest());
                },
            ),
            check(18, 0.0, Idle, Done, Firm),
        ],
    );
}

#[test]
fn a_finished_turn_then_another() {
    use {Activity::*, AgentState::*, Certainty::*};
    replay(
        "done",
        &[
            check(12, 0.0, Idle, Done, Firm),
            check(12, 120.0, Idle, Done, Firm),
            check(19, 0.0, Idle, Done, Firm),
        ],
    );
}

#[test]
fn a_long_silent_tool_in_bypass_mode_stays_busy() {
    use {Activity::*, AgentState::*, Certainty::*};
    replay(
        "hard-kill-mid-tool",
        &[
            check(12, 0.0, Busy, RunningTool, Firm),
            // No one can be asked in bypass mode, so it's never a prompt.
            check(12, 30.0, Busy, RunningTool, Firm),
            // Nothing written past the debounce, but a running tool is never
            // taken for rest.
            with(check(12, 901.0, Busy, RunningTool, Firm), |s| {
                assert!(!s.at_rest());
            }),
        ],
    );
}

#[test]
fn a_usage_limit_stop_says_until_when() {
    use {Activity::*, AgentState::*, Certainty::*};
    let limited = |s: &Status| {
        assert_eq!(s.stop_reason, Some(StopReason::UsageLimit));
        assert_eq!(s.resets_at, Some(Timestamp::from_secs_f64(1_793_159_400.0)));
        assert_eq!(s.error.as_deref(), Some("rate_limit"));
        assert!(s.at_rest(), "a limited agent sleeps like any idle one");
        assert_eq!(
            s.limited_until().as_deref(),
            Some("limited until 2026-10-28T03:50:00Z")
        );
        let wire = s.to_rpc();
        assert_eq!(wire.stop_reason, Some(StopReason::UsageLimit));
        assert_eq!(wire.resets_at.as_deref(), Some("2026-10-28T03:50:00Z"));
    };
    replay(
        "usage-limit",
        &[
            with(check(1, 5.0, Idle, Errored, Firm), limited),
            with(check(3, 5.0, Idle, Errored, Firm), limited),
        ],
    );
}

#[test]
fn a_loop_wakeup_is_pending_with_its_due_time_until_it_fires() {
    use {Activity::*, AgentState::*, Certainty::*};
    let pending = |s: &Status| {
        let [w] = s.pending_wakeups.as_slice() else {
            panic!("one wakeup: {:?}", s.pending_wakeups);
        };
        assert_eq!(
            (w.kind, w.certainty, w.recurring),
            (WakeupKind::Loop, Certainty::Firm, false)
        );
        assert_eq!(w.due_at, Some(Timestamp::from_secs_f64(1_793_324_520.0)));
        assert_eq!(w.reason.as_deref(), Some("Watching the deploy."));
        let wire = &s.to_rpc().pending_wakeups[0];
        assert_eq!(wire.due_at.as_deref(), Some("2026-10-30T01:42:00Z"));
    };
    replay(
        "loop-wakeup",
        &[
            with(check(25, 0.0, Busy, Thinking, Firm), pending),
            with(check(35, 0.0, Idle, Done, Firm), pending),
            with(check(38, 0.0, Idle, Done, Firm), |s| {
                assert!(s.pending_wakeups.is_empty(), "it fired");
            }),
            with(check(45, 901.0, Idle, Done, Firm), |s| {
                assert!(s.pending_wakeups.is_empty());
            }),
        ],
    );
}

#[test]
fn a_recurring_cron_job_stays_pending() {
    use {Activity::*, AgentState::*, Certainty::*};
    fn one_cron(s: &Status, certainty: Certainty, due: bool) {
        let [w] = s.pending_wakeups.as_slice() else {
            panic!("one wakeup: {:?}", s.pending_wakeups);
        };
        assert_eq!(
            (w.kind, w.certainty, w.recurring, w.due_at.is_some()),
            (WakeupKind::Cron, certainty, true, due)
        );
        assert_eq!(w.reason.as_deref(), Some("Check the deploy."));
    }
    // Armed, with its next fire time.
    fn armed(s: &Status) {
        one_cron(s, Firm, true);
    }
    // Once it has fired, still pending, with no due time recorded.
    fn fired(s: &Status) {
        one_cron(s, Inferred, false);
    }
    replay(
        "cron-recurring",
        &[
            with(check(38, 0.0, Busy, Thinking, Firm), armed),
            with(check(45, 0.0, Idle, Done, Firm), armed),
            with(check(48, 0.0, Idle, Done, Firm), fired),
            with(check(55, 0.0, Idle, Done, Firm), fired),
            with(check(59, 0.0, Idle, Done, Firm), fired),
            with(check(59, 901.0, Idle, Done, Firm), fired),
        ],
    );
}
