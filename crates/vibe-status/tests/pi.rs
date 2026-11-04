//! Recorded Pi sessions (`tests/fixtures/pi/`) read through agent-tail's own
//! Pi adapter: busy while it thinks and on a long tool, never a question, as Pi
//! has no approvals; done at the end of its turn; errored and limited shown;
//! and its conversation read back.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use vibe_core::model::{Activity, AgentKind, AgentState, MessageRole, StopReason};
use vibe_status::{Status, Tail, Timestamp};

const ID: &str = "0b5c3f6e-0000-4000-8000-0000000000bb";
const DEBOUNCE: Duration = Duration::from_secs(60);

fn fixture(name: &str) -> Vec<String> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/pi/{name}.jsonl"));
    fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn cwd_of(lines: &[String]) -> String {
    let header: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    header["cwd"].as_str().unwrap().to_owned()
}

fn time_of(line: &str) -> f64 {
    let entry: serde_json::Value = serde_json::from_str(line).unwrap();
    Timestamp::parse_rfc3339(entry["timestamp"].as_str().unwrap())
        .unwrap()
        .as_secs_f64()
}

/// A tail on a directory holding the session's file, as Pi names it.
struct Fixture {
    _dir: tempfile::TempDir,
    file: std::path::PathBuf,
    tail: Tail,
    lines: Vec<String>,
    written: usize,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let lines = fixture(name);
        let dir = tempfile::tempdir().unwrap();
        let file = dir
            .path()
            .join(format!("2026-10-25T01-19-19-235Z_{ID}.jsonl"));
        let tail = Tail::of(
            AgentKind::Pi,
            dir.path().to_owned(),
            Path::new(&cwd_of(&lines)),
            ID,
            DEBOUNCE,
        );
        Fixture {
            _dir: dir,
            file,
            tail,
            lines,
            written: 0,
        }
    }

    /// Appends the fixture's lines up to `upto`, and reads them.
    fn write_to(&mut self, upto: usize) {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file)
            .unwrap();
        for line in &self.lines[self.written..upto] {
            writeln!(f, "{line}").unwrap();
        }
        // The file's time is the last line's, as if written just then.
        let at = Timestamp::from_secs_f64(time_of(&self.lines[upto - 1]));
        f.set_modified(at.to_system_time()).unwrap();
        self.written = upto;
        self.tail.poll();
    }

    /// The status `offset` seconds after the line written last.
    fn status(&self, offset: f64) -> Status {
        let at = time_of(&self.lines[self.written - 1]) + offset;
        self.tail.status(Timestamp::from_secs_f64(at))
    }

    fn line_with(&self, needle: &str) -> usize {
        self.lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no line with {needle}"))
    }
}

#[test]
fn nothing_written_yet_is_busy_starting() {
    let dir = tempfile::tempdir().unwrap();
    let mut tail = Tail::of(
        AgentKind::Pi,
        dir.path().to_owned(),
        Path::new("/w/q"),
        ID,
        DEBOUNCE,
    );
    tail.poll();
    assert_eq!(tail.transcript(), None);
    let s = tail.status(Timestamp::from_secs_f64(1000.0));
    assert_eq!(s.activity, Activity::Unknown);
    assert_eq!(s.state(), AgentState::Busy);
    assert_eq!(tail.session_id(), ID);
}

#[test]
fn a_plain_answer_goes_thinking_then_done() {
    let mut f = Fixture::new("plain-qa");
    let user = f.line_with(r#""role":"user""#);
    f.write_to(user + 1);
    let s = f.status(1.0);
    assert_eq!(s.activity, Activity::Thinking);
    assert_eq!(s.state(), AgentState::Busy);
    let total = f.lines.len();
    f.write_to(total);
    let s = f.status(5.0);
    assert_eq!(s.activity, Activity::Done);
    assert_eq!(s.state(), AgentState::Idle);
    assert_eq!(s.title.as_deref(), Some("say hello"));
    assert!(s.transcript.is_some());
    assert_eq!(f.tail.session_id(), ID, "the file's name says whose it is");
}

/// The false hand-up of #46 can't happen on Pi: agent-tail marks it as having
/// no approvals, so a long, silent tool run stays busy however long it takes.
#[test]
fn a_long_tool_is_busy_and_never_a_question() {
    let mut f = Fixture::new("tool-call");
    let call = f.line_with(r#""stopReason":"toolUse""#);
    f.write_to(call + 1);
    for offset in [1.0, 30.0, 400.0, 3600.0] {
        let s = f.status(offset);
        assert_eq!(s.activity, Activity::RunningTool, "at {offset}s");
        assert_eq!(s.state(), AgentState::Busy, "at {offset}s");
        assert!(!s.inferred_approval(), "at {offset}s");
    }
    let total = f.lines.len();
    f.write_to(total);
    assert_eq!(f.status(5.0).activity, Activity::Done);
    // It may quietly have stood for hours; it's still only done.
    assert_eq!(f.status(4000.0).state(), AgentState::Idle);
}

#[test]
fn an_api_error_is_errored_and_at_rest() {
    let mut f = Fixture::new("api-error");
    let total = f.lines.len();
    f.write_to(total);
    let s = f.status(5.0);
    assert_eq!(s.activity, Activity::Errored);
    assert_eq!(s.state(), AgentState::Idle);
    assert_eq!(s.stop_reason, None);
    assert!(s.error.is_some());
}

#[test]
fn a_usage_limit_is_shown_as_one() {
    let mut f = Fixture::new("usage-limit");
    let total = f.lines.len();
    f.write_to(total);
    let s = f.status(5.0);
    assert_eq!(s.activity, Activity::Errored);
    assert_eq!(s.stop_reason, Some(StopReason::UsageLimit));
    assert_eq!(s.state(), AgentState::Idle);
}

#[test]
fn an_interrupted_turn_is_interrupted() {
    let mut f = Fixture::new("interrupted");
    let total = f.lines.len();
    f.write_to(total);
    let s = f.status(5.0);
    assert_eq!(s.activity, Activity::Interrupted);
    assert_eq!(s.state(), AgentState::Idle);
}

/// Pi's `/new` starts a new session in a new file; the tail follows it, as
/// after Claude's `/clear`, and the resume goes with it.
#[test]
fn a_new_session_in_the_worktree_is_followed() {
    let mut f = Fixture::new("plain-qa");
    let total = f.lines.len();
    f.write_to(total);
    assert_eq!(f.tail.session_id(), ID);
    // Written later, in the same directory, by the same worktree.
    let newer = "11111111-0000-4000-8000-0000000000cc";
    let dir = f.file.parent().unwrap().to_owned();
    let mut body = f.lines.clone();
    for line in &mut body {
        *line = line.replace("2026-10-2", "2026-11-2");
    }
    std::thread::sleep(Duration::from_millis(20));
    fs::write(
        dir.join(format!("2026-11-25T01-19-19-235Z_{newer}.jsonl")),
        body.join("\n") + "\n",
    )
    .unwrap();
    f.tail.poll();
    assert_eq!(f.tail.session_id(), newer);
    // Another worktree's, or none at all, isn't.
    let other = "22222222-0000-4000-8000-0000000000dd";
    let cwd = cwd_of(&f.lines);
    let theirs = body
        .join("\n")
        .replace(&cwd, "/somewhere/else")
        .replace("2026-11-2", "2026-12-2");
    std::thread::sleep(Duration::from_millis(20));
    fs::write(
        dir.join(format!("2026-12-25T00-00-00-000Z_{other}.jsonl")),
        theirs,
    )
    .unwrap();
    f.tail.poll();
    assert_eq!(f.tail.session_id(), newer);
}

#[test]
fn its_conversation_is_read_back() {
    let text = fixture("tool-call").join("\n");
    let messages = vibe_status::messages::parse(&text);
    let said: Vec<(MessageRole, &str)> =
        messages.iter().map(|m| (m.role, m.text.as_str())).collect();
    assert_eq!(said.len(), 3, "{said:?}");
    assert_eq!(said[0], (MessageRole::User, "use the tool"));
    assert_eq!(said[1].0, MessageRole::Tool);
    assert!(said[1].1.contains("bash"), "{}", said[1].1);
    assert_eq!(said[2], (MessageRole::Assistant, "The tool ran."));
    // The system prompt and the tool's result (three messages, above) aren't its conversation.
    assert!(
        !said
            .iter()
            .any(|(_, t)| t.contains("expert coding assistant"))
    );

    // A turn that failed says why, rather than saying nothing.
    let text = fixture("usage-limit").join("\n");
    let last = vibe_status::messages::parse(&text).pop().unwrap();
    assert_eq!(last.role, MessageRole::Assistant);
    assert!(last.text.starts_with("Error: 429"), "{}", last.text);
}
