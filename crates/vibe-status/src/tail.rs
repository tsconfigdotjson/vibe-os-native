//! One agent's transcript, followed with agent-tail's reducer.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_tail::{
    ChunkCtx, EventKind as TailEvent, Harness, ParseMode, READ_CHUNK, ReducerOptions,
    SessionReducer, SessionState, Timestamp,
};
use vibe_core::model::{Activity, AgentState, Certainty, StopReason, WakeupKind};
use vibe_core::rpc;
use vibe_core::time::rfc3339;

/// How much of a transcript is read to find the working directory it
/// records. Claude writes it on its first entries.
const CWD_PROBE: u64 = 256 * 1024;

/// One agent's transcript, read as it's appended to.
///
/// Claude Code is launched with `--session-id`, so the transcript is known
/// exactly: `<project dir>/<session id>.jsonl`. A `/clear` starts a new
/// session, in a new file beside it; the tail then follows the newest
/// session whose recorded working directory is the agent's worktree. Each
/// worktree has exactly one agent, so that's always this agent's. Subagents'
/// transcripts, under `<session id>/subagents/`, are fed as its helpers.
#[derive(Debug)]
pub struct Tail {
    project_dir: PathBuf,
    worktree: String,
    /// The session the agent was launched with.
    launched: String,
    idle_after: Duration,
    followed: Option<Followed>,
    /// Counts up each time the tail starts on a transcript afresh: another
    /// session, or the same file replaced.
    generation: u64,
    /// The working directory each candidate transcript records.
    cwds: HashMap<PathBuf, String>,
}

#[derive(Debug)]
struct Followed {
    path: PathBuf,
    session_id: String,
    reducer: SessionReducer,
    /// The transcript's length and mtime when it was last read.
    seen: Option<(u64, SystemTime)>,
    helpers: HashMap<PathBuf, Helper>,
}

/// A subagent's transcript: how much of it was fed, and its `.meta.json`
/// as last read.
#[derive(Debug, Default)]
struct Helper {
    offset: u64,
    meta: Option<(SystemTime, u64)>,
}

/// A point in an agent's transcript to wait for an answer after: what
/// `vibe_wait` waits from (PRD: MCP interface). Take it before the send.
/// The default is before anything was read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mark {
    generation: u64,
    received: u64,
    last_at: Option<Timestamp>,
}

impl Tail {
    /// A tail on the transcripts in `project_dir`, Claude's directory for
    /// sessions run in `worktree`, of an agent launched as `session_id`.
    /// agent-tail's idle window is `idle_debounce`, the host's.
    pub fn new(
        project_dir: PathBuf,
        worktree: &Path,
        session_id: &str,
        idle_debounce: Duration,
    ) -> Tail {
        Tail {
            project_dir,
            worktree: worktree.to_string_lossy().into_owned(),
            launched: session_id.to_owned(),
            idle_after: idle_debounce,
            followed: None,
            generation: 0,
            cwds: HashMap::new(),
        }
    }

    /// The session it follows: the launch's, or a newer one after a
    /// `/clear`.
    pub fn session_id(&self) -> &str {
        self.followed
            .as_ref()
            .map_or(&self.launched, |f| &f.session_id)
    }

    /// The transcript it follows, once there is one.
    pub fn transcript(&self) -> Option<&Path> {
        self.followed.as_ref().map(|f| f.path.as_path())
    }

    /// Moves it to another project directory: Claude's directory for a
    /// long path is only known once it exists.
    pub fn set_project_dir(&mut self, dir: PathBuf) {
        if dir != self.project_dir {
            self.project_dir = dir;
            self.followed = None;
            self.generation += 1;
        }
    }

    /// Sets agent-tail's idle window to a new idle debounce. The
    /// transcript is read again from the start on the next poll, with the
    /// new window.
    pub fn set_idle_debounce(&mut self, idle_debounce: Duration) {
        if idle_debounce == self.idle_after {
            return;
        }
        self.idle_after = idle_debounce;
        if let Some(f) = &mut self.followed {
            f.reducer = reducer(&f.path, self.idle_after);
            f.seen = None;
            f.helpers.clear();
        }
    }

    /// Reads what was written since the last poll: a newer session if
    /// there is one, what was appended to the transcript, and its
    /// subagents'. Says whether anything was read.
    pub fn poll(&mut self) -> bool {
        let mut changed = self.choose();
        let idle_after = self.idle_after;
        let Some(f) = &mut self.followed else {
            return changed;
        };
        match f.read(idle_after) {
            Fed::Nothing => {}
            Fed::Appended => changed = true,
            Fed::Replaced => {
                changed = true;
                self.generation += 1;
            }
        }
        changed |= f.read_helpers(&self.project_dir);
        changed
    }

    /// The paths whose changes are worth a poll: the project directory
    /// (its parent until it exists), the transcript, and its subagents'.
    pub fn watch_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if self.project_dir.is_dir() {
            paths.push(self.project_dir.clone());
        } else if let Some(parent) = self.project_dir.parent() {
            paths.push(parent.to_owned());
        }
        if let Some(f) = &self.followed {
            paths.push(f.path.clone());
            let helpers = subagents_dir(&self.project_dir, &f.session_id);
            if helpers.is_dir() {
                paths.push(helpers);
            }
            paths.extend(f.helpers.keys().cloned());
        }
        paths
    }

    /// What the agent is doing as of `now`, from what's been read.
    pub fn status(&self, now: Timestamp) -> Status {
        let Some(f) = &self.followed else {
            return Status::unknown();
        };
        let state = f.reducer.state(now);
        if state.activity != agent_tail::Activity::Idle {
            return Status::from_state(&state, false, &f.path);
        }
        // agent-tail says only that nothing was written for its idle window,
        // the idle debounce. What the agent was doing as the window closed
        // is still what it's doing: a question stays a question.
        let last = [state.last_at, state.last_write]
            .into_iter()
            .flatten()
            .map(Timestamp::as_secs_f64)
            .fold(f64::NEG_INFINITY, f64::max);
        if !last.is_finite() {
            return Status::from_state(&state, true, &f.path);
        }
        // A millisecond inside it, as its end is a sum of floats.
        let at = Timestamp::from_secs_f64(last + self.idle_after.as_secs_f64() - 0.001);
        Status::from_state(&f.reducer.state(at), true, &f.path)
    }

    /// Where the transcript is now, for [`Tail::answered`]. Poll first, so
    /// it's after everything already written.
    pub fn mark(&self) -> Mark {
        Mark {
            generation: self.generation,
            received: self.followed.as_ref().map_or(0, |f| f.reducer.received()),
            last_at: self
                .followed
                .as_ref()
                .and_then(|f| f.reducer.state(Timestamp::EPOCH).last_at),
        }
    }

    /// Whether the agent has answered what was sent after `mark`: its
    /// transcript has moved on since, and it's no longer busy (PRD: MCP
    /// interface, `vibe_wait`). Quiet alone isn't an answer: right after a
    /// send, the transcript still ends where the last turn did.
    pub fn answered(&self, mark: &Mark, now: Timestamp) -> bool {
        let Some(f) = &self.followed else {
            return false;
        };
        let moved = self.generation != mark.generation
            || (f.reducer.received() > mark.received
                && f.reducer.state(now).last_at != mark.last_at);
        moved && self.status(now).state() != AgentState::Busy
    }

    /// Picks the transcript to follow, and says whether it changed.
    fn choose(&mut self) -> bool {
        let current_id = self.session_id().to_owned();
        let current = self.project_dir.join(format!("{current_id}.jsonl"));
        let mut best = file_mtime(&current).map(|m| (m, current.clone(), current_id));
        if let Ok(dir) = fs::read_dir(&self.project_dir) {
            for e in dir.flatten() {
                let path = e.path();
                if path == current {
                    continue;
                }
                let Some(id) = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_suffix(".jsonl"))
                    .map(str::to_owned)
                else {
                    continue;
                };
                // Not followed through a symlink, as agent-tail doesn't.
                let Some(mtime) = e
                    .metadata()
                    .ok()
                    .filter(fs::Metadata::is_file)
                    .and_then(|m| m.modified().ok())
                else {
                    continue;
                };
                if best.as_ref().is_some_and(|(m, _, _)| mtime <= *m) {
                    continue;
                }
                if self.recorded_cwd(&path).as_deref() != Some(self.worktree.as_str()) {
                    continue;
                }
                best = Some((mtime, path, id));
            }
        }
        let Some((_, path, session_id)) = best else {
            return false;
        };
        if self.followed.as_ref().is_some_and(|f| f.path == path) {
            return false;
        }
        self.followed = Some(Followed {
            reducer: reducer(&path, self.idle_after),
            path,
            session_id,
            seen: None,
            helpers: HashMap::new(),
        });
        self.generation += 1;
        true
    }

    /// The working directory a transcript records, if it has yet.
    fn recorded_cwd(&mut self, path: &Path) -> Option<String> {
        if let Some(cwd) = self.cwds.get(path) {
            return Some(cwd.clone());
        }
        let mut bytes = Vec::new();
        File::open(path)
            .and_then(|f| f.take(CWD_PROBE).read_to_end(&mut bytes))
            .ok()?;
        let cwd = agent_tail::parse_lines(Harness::ClaudeCode, &bytes, ParseMode::Lean).find_map(
            |e| match e.kind {
                TailEvent::SessionInfo { cwd: Some(cwd), .. } if !e.sidechain => Some(cwd),
                _ => None,
            },
        )?;
        self.cwds.insert(path.to_owned(), cwd.clone());
        Some(cwd)
    }
}

/// What a read of the transcript found.
enum Fed {
    Nothing,
    Appended,
    /// It was shorter than what had been read: replaced or truncated, so
    /// it was read again from the start.
    Replaced,
}

impl Followed {
    fn read(&mut self, idle_after: Duration) -> Fed {
        let Ok(meta) = fs::metadata(&self.path) else {
            return Fed::Nothing;
        };
        let (len, mtime) = (meta.len(), meta.modified().unwrap_or(UNIX_EPOCH));
        if self.seen == Some((len, mtime)) {
            return Fed::Nothing;
        }
        let mut read = Fed::Appended;
        if len < self.reducer.received() {
            self.reducer = reducer(&self.path, idle_after);
            self.helpers.clear();
            read = Fed::Replaced;
        }
        let Ok(mut file) = File::open(&self.path) else {
            return Fed::Nothing;
        };
        if file.seek(SeekFrom::Start(self.reducer.consumed())).is_err() {
            return Fed::Nothing;
        }
        agent_tail::read_from(
            &mut file,
            &mut self.reducer,
            READ_CHUNK,
            Timestamp::from(mtime),
            &self.path,
        );
        self.seen = Some((len, mtime));
        read
    }

    /// Feeds what the session's subagents appended, and their
    /// `.meta.json`, as agent-tail's own tracker does.
    fn read_helpers(&mut self, project_dir: &Path) -> bool {
        let dir = subagents_dir(project_dir, &self.session_id);
        let Ok(entries) = fs::read_dir(&dir) else {
            return false;
        };
        let mut changed = false;
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "jsonl") {
                continue;
            }
            let Some(meta) = e.metadata().ok().filter(fs::Metadata::is_file) else {
                continue;
            };
            let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
            let len = meta.len();
            let id = path.to_string_lossy().into_owned();
            let helper = self.helpers.entry(path.clone()).or_default();
            if len < helper.offset {
                helper.offset = 0;
            }
            if len > helper.offset {
                let mut bytes = Vec::new();
                let read = File::open(&path).and_then(|mut f| {
                    f.seek(SeekFrom::Start(helper.offset))?;
                    f.read_to_end(&mut bytes)
                });
                if read.is_ok() {
                    let ctx = ChunkCtx {
                        offset: helper.offset,
                        mtime: Timestamp::from(mtime),
                        path: &path,
                    };
                    helper.offset = match self.reducer.feed_helper(&id, &bytes, ctx) {
                        Ok(()) => helper.offset + bytes.len() as u64,
                        Err(_) => 0,
                    };
                    changed = true;
                }
            }
            // `finished` or `stoppedByUser` there is a firm end, even while
            // the helper's last entry is a pending tool.
            let meta_path = path.with_extension("meta.json");
            if let Ok(m) = fs::metadata(&meta_path) {
                let seen = (m.modified().unwrap_or(UNIX_EPOCH), m.len());
                if helper.meta != Some(seen)
                    && let Ok(bytes) = fs::read(&meta_path)
                {
                    self.reducer.feed_helper_meta(&id, &bytes);
                    helper.meta = Some(seen);
                    changed = true;
                }
            }
            self.reducer.helper_written(Timestamp::from(mtime));
        }
        changed
    }
}

fn reducer(path: &Path, idle_after: Duration) -> SessionReducer {
    let options = ReducerOptions {
        idle_after,
        ..ReducerOptions::default()
    };
    let mut r = SessionReducer::new(Harness::ClaudeCode, options);
    r.set_path(path);
    r
}

fn subagents_dir(project_dir: &Path, session_id: &str) -> PathBuf {
    project_dir.join(session_id).join("subagents")
}

/// A regular file's mtime, not through a symlink.
fn file_mtime(path: &Path) -> Option<SystemTime> {
    fs::symlink_metadata(path)
        .ok()
        .filter(fs::Metadata::is_file)
        .and_then(|m| m.modified().ok())
}

/// What an agent is doing, as its transcript says.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub activity: Activity,
    pub certainty: Certainty,
    /// When the activity began.
    pub since: Option<Timestamp>,
    /// The newest write to the transcript or a subagent's.
    pub last_write: Option<Timestamp>,
    /// Nothing has been written for agent-tail's idle window, the idle
    /// debounce. `activity` is still what it was as the window closed.
    pub quiet: bool,
    pub title: Option<String>,
    /// The latest thing it did, as a line.
    pub line: Option<String>,
    pub pending_wakeups: Vec<Wakeup>,
    pub stop_reason: Option<StopReason>,
    pub resets_at: Option<Timestamp>,
    pub error: Option<String>,
    /// The transcript it's from; none before one is found.
    pub transcript: Option<PathBuf>,
}

/// A wakeup the agent has armed and that hasn't fired.
#[derive(Debug, Clone, PartialEq)]
pub struct Wakeup {
    pub kind: WakeupKind,
    pub due_at: Option<Timestamp>,
    pub certainty: Certainty,
    pub recurring: bool,
    pub reason: Option<String>,
}

/// The state clients see for an activity, while the agent's session runs
/// (PRD: MCP interface, States). Asleep, paused and exited are `vibed`'s.
pub fn state_of(activity: Activity) -> AgentState {
    match activity {
        Activity::Thinking | Activity::RunningTool | Activity::BackgroundWork => AgentState::Busy,
        // Nothing read yet: Claude is starting on its prompt.
        Activity::Unknown => AgentState::Busy,
        Activity::AwaitingAnswer | Activity::AwaitingApproval => AgentState::Question,
        Activity::Done | Activity::Interrupted | Activity::Errored => AgentState::Idle,
    }
}

impl Status {
    /// Before any transcript is read.
    pub fn unknown() -> Status {
        Status {
            activity: Activity::Unknown,
            certainty: Certainty::Firm,
            since: None,
            last_write: None,
            quiet: false,
            title: None,
            line: None,
            pending_wakeups: Vec::new(),
            stop_reason: None,
            resets_at: None,
            error: None,
            transcript: None,
        }
    }

    fn from_state(s: &SessionState, quiet: bool, transcript: &Path) -> Status {
        let activity = activity(s.activity);
        Status {
            activity,
            certainty: certainty(s.certainty),
            // Before an entry that moves it, agent-tail's `since` is only
            // the file's mtime.
            since: s.since.filter(|_| activity != Activity::Unknown),
            last_write: s.last_write,
            quiet,
            title: Some(s.meta.title.clone()).filter(|t| !t.is_empty()),
            line: s.recent.last().map(ToString::to_string),
            pending_wakeups: s
                .pending_wakeups
                .iter()
                .map(|w| Wakeup {
                    kind: match w.kind {
                        agent_tail::WakeupKind::Loop => WakeupKind::Loop,
                        // A kind this version doesn't know keeps its agent
                        // awake, as a cron job does.
                        _ => WakeupKind::Cron,
                    },
                    due_at: w.due_at,
                    certainty: certainty(w.certainty),
                    recurring: w.recurring,
                    reason: w.reason.clone(),
                })
                .collect(),
            stop_reason: match s.stop_reason {
                Some(agent_tail::StopReason::UsageLimit) => Some(StopReason::UsageLimit),
                _ => None,
            },
            resets_at: s.resets_at,
            error: s.error.clone(),
            transcript: Some(transcript.to_owned()),
        }
    }

    /// The state clients see, while the agent's session runs.
    pub fn state(&self) -> AgentState {
        state_of(self.activity)
    }

    /// Firmly at rest: what an agent may sleep in (PRD: Going to sleep,
    /// At rest). An inferred wait for approval is left to the caller's
    /// longer quiet period.
    pub fn at_rest(&self) -> bool {
        self.certainty == Certainty::Firm
            && matches!(
                self.activity,
                Activity::Done
                    | Activity::Interrupted
                    | Activity::Errored
                    | Activity::AwaitingAnswer
            )
    }

    /// "limited until …", for an agent stopped by a usage limit.
    pub fn limited_until(&self) -> Option<String> {
        if self.stop_reason != Some(StopReason::UsageLimit) {
            return None;
        }
        Some(match self.resets_at {
            Some(t) => format!("limited until {}", rfc3339(secs(t))),
            None => "limited until its usage limit resets".to_owned(),
        })
    }

    /// As the socket API sends it.
    pub fn to_rpc(&self) -> rpc::AgentStatus {
        let time = |t: Option<Timestamp>| t.map(|t| rfc3339(secs(t)));
        rpc::AgentStatus {
            activity: self.activity,
            certainty: self.certainty,
            since: time(self.since),
            title: self.title.clone(),
            line: self.line.clone(),
            pending_wakeups: self
                .pending_wakeups
                .iter()
                .map(|w| rpc::Wakeup {
                    kind: w.kind,
                    due_at: time(w.due_at),
                    certainty: w.certainty,
                    recurring: w.recurring,
                    reason: w.reason.clone(),
                })
                .collect(),
            stop_reason: self.stop_reason,
            resets_at: time(self.resets_at),
            error: self.error.clone(),
        }
    }
}

/// Whole seconds, rounded down.
fn secs(t: Timestamp) -> i64 {
    t.as_secs_f64().floor() as i64
}

fn activity(a: agent_tail::Activity) -> Activity {
    use agent_tail::Activity as A;
    match a {
        A::AwaitingAnswer => Activity::AwaitingAnswer,
        A::AwaitingApproval => Activity::AwaitingApproval,
        A::RunningTool => Activity::RunningTool,
        A::Thinking => Activity::Thinking,
        A::BackgroundWork => Activity::BackgroundWork,
        A::Done => Activity::Done,
        A::Interrupted => Activity::Interrupted,
        A::Errored => Activity::Errored,
        // Only for no writes in the idle window, which `Tail::status` looks
        // behind; a turn that's over either way.
        A::Idle => Activity::Done,
        _ => Activity::Unknown,
    }
}

fn certainty(c: agent_tail::Certainty) -> Certainty {
    match c {
        agent_tail::Certainty::Firm => Certainty::Firm,
        agent_tail::Certainty::Inferred => Certainty::Inferred,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0b5c3f6e-0000-4000-8000-000000000001";

    fn t(s: &str) -> Timestamp {
        Timestamp::parse_rfc3339(s).unwrap()
    }

    fn user(cwd: &str, at: &str, text: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type": "user", "sessionId": ID, "cwd": cwd, "timestamp": at,
                "message": {"role": "user", "content": text}})
        )
    }

    fn reply(cwd: &str, at: &str, text: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type": "assistant", "sessionId": ID, "cwd": cwd, "timestamp": at,
                "message": {"id": format!("msg-{at}"), "role": "assistant", "stop_reason": "end_turn",
                    "content": [{"type": "text", "text": text}]}})
        )
    }

    fn ask(cwd: &str, at: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type": "assistant", "sessionId": ID, "cwd": cwd, "timestamp": at,
                "message": {"id": format!("msg-{at}"), "role": "assistant", "stop_reason": "tool_use",
                    "content": [{"type": "tool_use", "id": "toolu_1", "name": "AskUserQuestion",
                        "input": {"questions": [{"question": "Red or blue?", "header": "Colour",
                            "multiSelect": false, "options": [{"label": "Red"}, {"label": "Blue"}]}]}}]}})
        )
    }

    struct World {
        _dir: tempfile::TempDir,
        project: PathBuf,
        worktree: PathBuf,
    }

    fn world() -> World {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("projects/-w-app");
        fs::create_dir_all(&project).unwrap();
        World {
            project,
            worktree: PathBuf::from("/w/app"),
            _dir: dir,
        }
    }

    /// Appends `text`, and dates the file by its last entry, as Claude's
    /// writes do: agent-tail measures quiet from the later of the two.
    fn append(path: &Path, text: &str) {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(text.as_bytes()).unwrap();
        let last = fs::read_to_string(path)
            .unwrap()
            .lines()
            .rev()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find_map(|v| v["timestamp"].as_str().map(t))
            .unwrap_or_else(|| t("2026-10-30T12:00:00Z"));
        f.set_modified(last.to_system_time()).unwrap();
    }

    fn titled(at: &str, title: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type": "ai-title", "aiTitle": title, "sessionId": ID, "timestamp": at})
        )
    }

    #[test]
    fn no_transcript_is_unknown_and_busy() {
        let w = world();
        let mut tail = Tail::new(w.project.clone(), &w.worktree, ID, Duration::from_secs(60));
        assert!(!tail.poll());
        let s = tail.status(t("2026-10-30T12:00:00Z"));
        assert_eq!(s, Status::unknown());
        assert_eq!(s.state(), AgentState::Busy);
        assert_eq!(
            s.to_rpc(),
            rpc::AgentStatus {
                activity: Activity::Unknown,
                certainty: Certainty::Firm,
                since: None,
                title: None,
                line: None,
                pending_wakeups: vec![],
                stop_reason: None,
                resets_at: None,
                error: None,
            }
        );
    }

    #[test]
    fn reads_only_what_was_appended_and_holds_back_half_a_line() {
        let w = world();
        let path = w.project.join(format!("{ID}.jsonl"));
        let mut tail = Tail::new(w.project.clone(), &w.worktree, ID, Duration::from_secs(60));
        let prompt = user("/w/app", "2026-10-30T12:00:00Z", "fix the build");
        append(&path, &prompt[..20]);
        assert!(tail.poll(), "found the transcript");
        assert_eq!(
            tail.status(t("2026-10-30T12:00:01Z")).activity,
            Activity::Unknown
        );
        append(&path, &prompt[20..]);
        assert!(tail.poll());
        let s = tail.status(t("2026-10-30T12:00:01Z"));
        assert_eq!(s.activity, Activity::Thinking);
        assert_eq!(s.state(), AgentState::Busy);
        assert_eq!(s.title, None, "Claude hasn't titled it yet");
        assert_eq!(s.line.as_deref(), Some("› fix the build"));
        assert_eq!(s.to_rpc().since.as_deref(), Some("2026-10-30T12:00:00Z"));
        assert!(!tail.poll(), "nothing new");
        append(&path, &titled("2026-10-30T12:00:02Z", "Fix the build"));
        append(&path, &reply("/w/app", "2026-10-30T12:00:05Z", "Fixed."));
        assert!(tail.poll());
        let s = tail.status(t("2026-10-30T12:00:06Z"));
        assert_eq!((s.activity, s.state()), (Activity::Done, AgentState::Idle));
        assert_eq!(s.title.as_deref(), Some("Fix the build"));
        assert!(s.at_rest());
    }

    #[test]
    fn a_question_stays_a_question_past_the_idle_window() {
        let w = world();
        let path = w.project.join(format!("{ID}.jsonl"));
        append(&path, &user("/w/app", "2026-10-30T12:00:00Z", "pick one"));
        append(&path, &ask("/w/app", "2026-10-30T12:00:02Z"));
        let mut tail = Tail::new(w.project.clone(), &w.worktree, ID, Duration::from_secs(60));
        tail.poll();
        let soon = tail.status(t("2026-10-30T12:00:10Z"));
        assert_eq!(
            (soon.activity, soon.state(), soon.quiet),
            (Activity::AwaitingAnswer, AgentState::Question, false)
        );
        // An hour on, agent-tail would say idle; it's still asking.
        let later = tail.status(t("2026-10-30T13:00:00Z"));
        assert_eq!(
            (later.activity, later.state(), later.quiet),
            (Activity::AwaitingAnswer, AgentState::Question, true)
        );
        assert_eq!(later.since, soon.since);
        assert!(later.at_rest());
    }

    #[test]
    fn a_silent_tool_run_is_busy_then_maybe_a_permission_prompt() {
        let w = world();
        let path = w.project.join(format!("{ID}.jsonl"));
        append(&path, &user("/w/app", "2026-10-30T12:00:00Z", "test it"));
        let call = serde_json::json!({"type": "assistant", "sessionId": ID, "cwd": "/w/app",
            "timestamp": "2026-10-30T12:00:01Z", "message": {"id": "m1", "role": "assistant",
            "stop_reason": "tool_use", "content": [{"type": "tool_use", "id": "toolu_9",
            "name": "Bash", "input": {"command": "cargo test"}}]}});
        append(&path, &format!("{call}\n"));
        let mut tail = Tail::new(w.project.clone(), &w.worktree, ID, Duration::from_secs(60));
        tail.poll();
        let s = tail.status(t("2026-10-30T12:00:05Z"));
        assert_eq!(
            (s.activity, s.state()),
            (Activity::RunningTool, AgentState::Busy)
        );
        assert_eq!(s.line.as_deref(), Some("$ cargo test"));
        // After 20 s with no result, it may be waiting on a permission
        // prompt: a guess, and it says so. It stays one past the window.
        for now in ["2026-10-30T12:00:30Z", "2026-10-30T14:00:00Z"] {
            let s = tail.status(t(now));
            assert_eq!(
                (s.activity, s.certainty, s.state()),
                (
                    Activity::AwaitingApproval,
                    Certainty::Inferred,
                    AgentState::Question
                ),
                "{now}"
            );
            assert!(!s.at_rest());
        }
    }

    #[test]
    fn after_a_clear_it_follows_the_newest_session_in_its_worktree() {
        let w = world();
        let old = w.project.join(format!("{ID}.jsonl"));
        append(&old, &user("/w/app", "2026-10-30T12:00:00Z", "first"));
        append(&old, &reply("/w/app", "2026-10-30T12:00:01Z", "ok"));
        let mut tail = Tail::new(w.project.clone(), &w.worktree, ID, Duration::from_secs(60));
        tail.poll();
        assert_eq!(tail.session_id(), ID);
        // Claude files sessions by a lossy name of their path: one from
        // another directory may share it, and isn't this agent's.
        std::thread::sleep(Duration::from_millis(20));
        let stranger = w.project.join("5e55e55e-0000-4000-8000-000000000000.jsonl");
        append(
            &stranger,
            &user("/w-app", "2026-10-30T12:01:00Z", "not mine"),
        );
        assert!(!tail.poll());
        assert_eq!(tail.session_id(), ID);
        // `/clear`: a new session in the same worktree.
        std::thread::sleep(Duration::from_millis(20));
        let new_id = "c1ea4ed0-0000-4000-8000-000000000000";
        let new = w.project.join(format!("{new_id}.jsonl"));
        append(&new, &user("/w/app", "2026-10-30T12:02:00Z", "second"));
        append(&new, &titled("2026-10-30T12:02:00Z", "Second"));
        let mark = tail.mark();
        assert!(tail.poll());
        assert_eq!(tail.session_id(), new_id);
        assert_eq!(tail.transcript(), Some(new.as_path()));
        let s = tail.status(t("2026-10-30T12:02:01Z"));
        assert_eq!(
            (s.activity, s.title.as_deref()),
            (Activity::Thinking, Some("Second"))
        );
        assert!(!tail.answered(&mark, t("2026-10-30T12:02:01Z")), "busy");
        append(&new, &reply("/w/app", "2026-10-30T12:02:03Z", "done"));
        tail.poll();
        assert!(tail.answered(&mark, t("2026-10-30T12:02:04Z")));
        // A new tail, as after a restart, finds the same session.
        let mut again = Tail::new(w.project.clone(), &w.worktree, ID, Duration::from_secs(60));
        again.poll();
        assert_eq!(again.session_id(), new_id);
        assert_eq!(
            again.status(t("2026-10-30T12:02:04Z")),
            tail.status(t("2026-10-30T12:02:04Z"))
        );
    }

    #[test]
    fn an_answer_is_a_move_then_rest_not_just_quiet() {
        let w = world();
        let path = w.project.join(format!("{ID}.jsonl"));
        append(&path, &user("/w/app", "2026-10-30T12:00:00Z", "first"));
        append(&path, &reply("/w/app", "2026-10-30T12:00:01Z", "ok"));
        let mut tail = Tail::new(w.project.clone(), &w.worktree, ID, Duration::from_secs(60));
        tail.poll();
        let now = t("2026-10-30T12:00:10Z");
        let mark = tail.mark();
        // Right after the send, it's still done with the last turn.
        assert!(!tail.answered(&mark, now));
        // A line that moves nothing isn't an answer either.
        append(
            &path,
            &format!(
                "{}\n",
                serde_json::json!({"type": "permission-mode", "permissionMode": "auto", "sessionId": ID})
            ),
        );
        tail.poll();
        assert!(!tail.answered(&mark, now));
        append(&path, &user("/w/app", "2026-10-30T12:00:11Z", "second"));
        tail.poll();
        assert!(!tail.answered(&mark, t("2026-10-30T12:00:12Z")), "working");
        // The prompt and its question in one read count too.
        append(&path, &ask("/w/app", "2026-10-30T12:00:13Z"));
        tail.poll();
        assert!(tail.answered(&mark, t("2026-10-30T12:00:14Z")), "asking");
        let mark = tail.mark();
        assert!(!tail.answered(&mark, t("2026-10-30T12:00:14Z")));
    }

    #[test]
    fn a_replaced_transcript_is_read_again() {
        let w = world();
        let path = w.project.join(format!("{ID}.jsonl"));
        append(
            &path,
            &user("/w/app", "2026-10-30T12:00:00Z", "a long first prompt"),
        );
        append(&path, &reply("/w/app", "2026-10-30T12:00:01Z", "ok"));
        let mut tail = Tail::new(w.project.clone(), &w.worktree, ID, Duration::from_secs(60));
        tail.poll();
        let mark = tail.mark();
        fs::remove_file(&path).unwrap();
        append(&path, &user("/w/app", "2026-10-30T12:05:00Z", "short"));
        assert!(tail.poll());
        let s = tail.status(t("2026-10-30T12:05:01Z"));
        assert_eq!(
            (s.activity, s.line.as_deref()),
            (Activity::Thinking, Some("› short"))
        );
        // Busy, so not an answer yet, but it counts as moved.
        append(&path, &reply("/w/app", "2026-10-30T12:05:02Z", "ok"));
        tail.poll();
        assert!(tail.answered(&mark, t("2026-10-30T12:05:03Z")));
    }

    #[test]
    fn a_new_idle_debounce_reads_it_again() {
        let w = world();
        let path = w.project.join(format!("{ID}.jsonl"));
        append(&path, &user("/w/app", "2026-10-30T12:00:00Z", "go"));
        append(&path, &reply("/w/app", "2026-10-30T12:00:01Z", "ok"));
        let mut tail = Tail::new(w.project.clone(), &w.worktree, ID, Duration::from_secs(60));
        tail.poll();
        let at = t("2026-10-30T12:00:31Z");
        assert!(!tail.status(at).quiet);
        tail.set_idle_debounce(Duration::from_secs(10));
        // Read again with the new window on the next poll.
        assert!(tail.poll());
        let s = tail.status(at);
        assert!(s.quiet);
        assert_eq!(s.activity, Activity::Done);
    }

    #[test]
    fn subagents_keep_it_at_work() {
        let w = world();
        let path = w.project.join(format!("{ID}.jsonl"));
        append(&path, &user("/w/app", "2026-10-30T12:00:00Z", "go"));
        append(
            &path,
            &reply("/w/app", "2026-10-30T12:00:01Z", "started a helper"),
        );
        let helpers = w.project.join(ID).join("subagents");
        fs::create_dir_all(&helpers).unwrap();
        let helper = helpers.join("agent-a1.jsonl");
        let call = serde_json::json!({"type": "assistant", "isSidechain": true, "sessionId": ID,
            "cwd": "/w/app", "timestamp": "2026-10-30T12:00:02Z", "message": {"id": "h1",
            "role": "assistant", "stop_reason": "tool_use", "content": [{"type": "tool_use",
            "id": "toolu_h", "name": "Bash", "input": {"command": "sleep 100"}}]}});
        append(&helper, &format!("{call}\n"));
        let mut tail = Tail::new(w.project.clone(), &w.worktree, ID, Duration::from_secs(60));
        tail.poll();
        let s = tail.status(t("2026-10-30T12:00:05Z"));
        assert_eq!(
            (s.activity, s.state()),
            (Activity::BackgroundWork, AgentState::Busy)
        );
        assert!(tail.watch_paths().contains(&helper));
        // Its `.meta.json` says it finished.
        fs::write(helper.with_extension("meta.json"), r#"{"finished":true}"#).unwrap();
        assert!(tail.poll());
        assert_eq!(
            tail.status(t("2026-10-30T12:00:06Z")).activity,
            Activity::Done
        );
    }

    #[test]
    fn states_fold_activities() {
        use Activity::*;
        for (a, s) in [
            (AwaitingAnswer, AgentState::Question),
            (AwaitingApproval, AgentState::Question),
            (RunningTool, AgentState::Busy),
            (Thinking, AgentState::Busy),
            (BackgroundWork, AgentState::Busy),
            (Done, AgentState::Idle),
            (Interrupted, AgentState::Idle),
            (Errored, AgentState::Idle),
            (Unknown, AgentState::Busy),
        ] {
            assert_eq!(state_of(a), s, "{a}");
        }
        assert_eq!(Activity::ALL.len(), 9, "one case per activity");
    }

    #[test]
    fn a_usage_limit_says_until_when() {
        let mut s = Status::unknown();
        assert_eq!(s.limited_until(), None);
        s.stop_reason = Some(StopReason::UsageLimit);
        assert_eq!(
            s.limited_until().as_deref(),
            Some("limited until its usage limit resets")
        );
        s.resets_at = Some(t("2026-10-30T15:00:00.500Z"));
        assert_eq!(
            s.limited_until().as_deref(),
            Some("limited until 2026-10-30T15:00:00Z")
        );
    }
}
