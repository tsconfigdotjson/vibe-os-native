//! One agent's transcript, followed with agent-tail's reducer.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
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
/// records, and when it began. Claude writes both on its first entries.
const CWD_PROBE: u64 = 256 * 1024;

/// How long a pending tool call goes without a result before agent-tail
/// guesses it's waiting on a permission prompt.
fn stall_after() -> Duration {
    ReducerOptions::default().stall_after
}

/// How long an inferred permission prompt must be quiet before it counts
/// as at rest (PRD: Going to sleep, Permission prompts): a long silent tool
/// run looks the same until then.
pub const INFERRED_QUIET: Duration = Duration::from_secs(5 * 60);

/// One agent's transcript, read as it's appended to.
///
/// Claude Code is launched with `--session-id`, so the transcript is known
/// exactly: `<project dir>/<session id>.jsonl`. A `/clear` starts a new
/// session, in a new file beside it; the tail then follows the newest
/// session whose recorded working directory is the agent's worktree. Each
/// worktree has exactly one agent, so that's always this agent's. It moves
/// only forward, to a session that began later, and only once something in
/// it says what the agent is doing: a `/clear` alone writes nothing that
/// does. Subagents' transcripts, under `<session id>/subagents/`, are fed as
/// its helpers.
#[derive(Debug)]
pub struct Tail {
    project_dir: PathBuf,
    worktree: String,
    /// The session the agent was launched with.
    launched: String,
    /// The idle debounce.
    debounce: Duration,
    followed: Option<Followed>,
    /// A newer session in the worktree, read until it says something.
    candidate: Option<Followed>,
    /// Counts up each time the tail starts on a transcript afresh: another
    /// session, or the same file replaced.
    generation: u64,
    /// What each transcript records of itself, once its working directory
    /// is known.
    probes: HashMap<PathBuf, Probe>,
}

/// A transcript's working directory, and the time of its first entry.
#[derive(Debug, Clone)]
struct Probe {
    cwd: String,
    began: Option<Timestamp>,
}

#[derive(Debug)]
struct Followed {
    path: PathBuf,
    session_id: String,
    reducer: SessionReducer,
    /// The transcript's length, mtime and inode when it was last read.
    seen: Option<(u64, SystemTime, (u64, u64))>,
    helpers: HashMap<PathBuf, Helper>,
    /// Entries read since the first read that are the user's: a prompt
    /// that starts a turn, or an interrupt.
    inputs: u64,
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
    inputs: u64,
    /// Whether the agent was at work when it was taken.
    busy: bool,
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
            debounce: idle_debounce,
            followed: None,
            candidate: None,
            generation: 0,
            probes: HashMap::new(),
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
            self.candidate = None;
            self.generation += 1;
        }
    }

    /// Sets agent-tail's idle window to a new idle debounce. The
    /// transcript is read again from the start on the next poll, with the
    /// new window.
    pub fn set_idle_debounce(&mut self, idle_debounce: Duration) {
        if idle_debounce == self.debounce {
            return;
        }
        self.debounce = idle_debounce;
        let window = self.window();
        for f in [&mut self.followed, &mut self.candidate]
            .into_iter()
            .flatten()
        {
            f.reset(window);
        }
    }

    /// agent-tail's idle window: the idle debounce, but never shorter than
    /// its wait before guessing a permission prompt, or a short debounce
    /// would hide every prompt behind `idle`.
    fn window(&self) -> Duration {
        self.debounce.max(stall_after() + Duration::from_secs(1))
    }

    /// Reads what was written since the last poll: a newer session if
    /// there is one, what was appended to the transcript, and its
    /// subagents'. Says whether anything was read.
    pub fn poll(&mut self) -> bool {
        let mut changed = self.choose();
        let window = self.window();
        let Some(f) = &mut self.followed else {
            return changed;
        };
        match f.read(window) {
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
    /// (its parent until it exists) and the transcript. Subagents' are
    /// left to the poll, so each agent costs a descriptor or two where
    /// watching needs one per file.
    pub fn watch_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if self.project_dir.is_dir() {
            paths.push(self.project_dir.clone());
        } else if let Some(parent) = self.project_dir.parent() {
            paths.push(parent.to_owned());
        }
        for f in [&self.followed, &self.candidate].into_iter().flatten() {
            paths.push(f.path.clone());
        }
        paths
    }

    /// What the agent is doing as of `now`, from what's been read.
    pub fn status(&self, now: Timestamp) -> Status {
        let Some(f) = &self.followed else {
            return Status::unknown();
        };
        let state = f.reducer.state(now);
        let last = [state.last_at, state.last_write]
            .into_iter()
            .flatten()
            .reduce(|a, b| if b > a { b } else { a });
        let quiet =
            last.is_some_and(|t| now.as_secs_f64() - t.as_secs_f64() > self.debounce.as_secs_f64());
        if state.activity != agent_tail::Activity::Idle {
            return Status::from_state(&state, quiet, last, &f.path, now);
        }
        let Some(last) = last else {
            return Status::from_state(&state, quiet, None, &f.path, now);
        };
        // agent-tail says only that nothing was written for its idle window.
        // What the agent was doing as the window closed is still what it's
        // doing: a question stays a question. A millisecond inside it, as
        // its end is a sum of floats.
        let at = Timestamp::from_secs_f64(last.as_secs_f64() + self.window().as_secs_f64() - 0.001);
        Status::from_state(&f.reducer.state(at), quiet, Some(last), &f.path, now)
    }

    /// Where the transcript is now, for [`Tail::answered`]. Poll first, so
    /// it's after everything already written.
    pub fn mark(&self, now: Timestamp) -> Mark {
        let f = self.followed.as_ref();
        Mark {
            generation: self.generation,
            received: f.map_or(0, |f| f.reducer.received()),
            last_at: f.and_then(|f| f.reducer.state(Timestamp::EPOCH).last_at),
            inputs: f.map_or(0, |f| f.inputs),
            // At work by the transcript alone: an inferred permission
            // prompt isn't, as approving one writes no prompt of the user's.
            busy: state_of(self.status(now).activity) == AgentState::Busy,
        }
    }

    /// Whether the agent has answered what was sent after `mark` (PRD: MCP
    /// interface, `vibe_wait`). Quiet alone isn't an answer: right after a
    /// send, the transcript still ends where the last turn did.
    ///
    /// Its transcript must have moved on since; if it was at work when the
    /// mark was taken, by a prompt or interrupt of the user's, as the end
    /// of the turn it was in is no answer to a send it hasn't read yet. And
    /// it must have settled: firmly at rest or asking something, or waiting
    /// on what may be a permission prompt for [`INFERRED_QUIET`], as a long
    /// silent tool run looks the same until then.
    pub fn answered(&self, mark: &Mark, now: Timestamp) -> bool {
        self.moved_since(mark) && self.status(now).settled(now)
    }

    /// What `vibe_wait` takes for an answer to what was sent after `mark`:
    /// the transcript moved on since, as for [`Tail::answered`], and the
    /// agent has stopped. A permission prompt counts as soon as agent-tail
    /// infers one, not after [`INFERRED_QUIET`]: that longer wait is for
    /// sleeping, and a wait can't outlast it. The caller says it may be a
    /// long silent tool run instead. Unless `background` is set, a turn
    /// that ended with work still running in the background counts too, as
    /// a server left running never finishes.
    pub fn answered_after(&self, mark: &Mark, now: Timestamp, background: bool) -> bool {
        if !self.moved_since(mark) {
            return false;
        }
        let status = self.status(now);
        status.settled(now)
            || status.activity == Activity::AwaitingApproval
            || !background && status.activity == Activity::BackgroundWork
    }

    /// Whether the transcript has moved on since `mark`; if the agent was
    /// at work when the mark was taken, by a prompt or interrupt of the
    /// user's, as the end of the turn it was in is no answer to a send it
    /// hasn't read yet.
    fn moved_since(&self, mark: &Mark) -> bool {
        let Some(f) = &self.followed else {
            return false;
        };
        self.generation != mark.generation
            || if mark.busy {
                f.inputs > mark.inputs
            } else {
                f.reducer.received() > mark.received
                    && f.reducer.state(Timestamp::EPOCH).last_at != mark.last_at
            }
    }

    /// Picks the transcript to follow, and says whether it changed.
    fn choose(&mut self) -> bool {
        let current_id = self.session_id().to_owned();
        let current = self.project_dir.join(format!("{current_id}.jsonl"));
        let current_mtime = file_mtime(&current);
        let current_began = current_mtime
            .and_then(|_| self.probe(&current))
            .and_then(|p| p.began);
        // The newest session in the worktree that began after this one.
        let mut newer: Option<(SystemTime, Option<Timestamp>, PathBuf, String)> = None;
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
                if current_mtime.is_some_and(|m| mtime <= m)
                    || newer.as_ref().is_some_and(|(m, ..)| mtime <= *m)
                {
                    continue;
                }
                let Some(probe) = self.probe(&path) else {
                    continue;
                };
                if probe.cwd != self.worktree {
                    continue;
                }
                // A session that began before this one is an old one written
                // to again, such as a late title: never gone back to.
                if let (Some(began), Some(current)) = (probe.began, current_began)
                    && began <= current
                {
                    continue;
                }
                newer = Some((mtime, probe.began, path, id));
            }
        }
        let Some((_, _, path, session_id)) = newer else {
            self.candidate = None;
            if self.followed.is_none() && current_mtime.is_some() {
                self.follow(Followed::new(current, current_id, self.window()));
                return true;
            }
            return false;
        };
        let window = self.window();
        if self.candidate.as_ref().is_none_or(|c| c.path != path) {
            self.candidate = Some(Followed::new(path, session_id, window));
        }
        let candidate = self.candidate.as_mut().expect("just set");
        candidate.read(window);
        // Before anything moves in it, a new session is only a `/clear` (or
        // a start): keep to the one that says what the agent last did.
        let said =
            candidate.reducer.state(Timestamp::EPOCH).activity != agent_tail::Activity::Unknown;
        if said || current_mtime.is_none() {
            let candidate = self.candidate.take().expect("just set");
            self.follow(candidate);
            return true;
        }
        if self.followed.is_none() && current_mtime.is_some() {
            self.follow(Followed::new(current, current_id, window));
            return true;
        }
        false
    }

    fn follow(&mut self, f: Followed) {
        self.followed = Some(f);
        self.generation += 1;
    }

    /// The working directory a transcript records, and when it began, once
    /// it records them.
    fn probe(&mut self, path: &Path) -> Option<Probe> {
        if let Some(p) = self.probes.get(path) {
            return Some(p.clone());
        }
        let mut bytes = Vec::new();
        File::open(path)
            .and_then(|f| f.take(CWD_PROBE).read_to_end(&mut bytes))
            .ok()?;
        let mut cwd = None;
        let mut began = None;
        for e in agent_tail::parse_lines(Harness::ClaudeCode, &bytes, ParseMode::Lean) {
            if e.sidechain {
                continue;
            }
            began = began.or(e.at);
            if let TailEvent::SessionInfo { cwd: Some(c), .. } = e.kind {
                cwd = cwd.or(Some(c));
            }
            if cwd.is_some() && began.is_some() {
                break;
            }
        }
        let probe = Probe { cwd: cwd?, began };
        self.probes.insert(path.to_owned(), probe.clone());
        Some(probe)
    }
}

/// What a read of the transcript found.
enum Fed {
    Nothing,
    Appended,
    /// It was shorter than what had been read, or another file: replaced
    /// or truncated, so it was read again from the start.
    Replaced,
}

impl Followed {
    fn new(path: PathBuf, session_id: String, window: Duration) -> Followed {
        Followed {
            reducer: reducer(&path, window),
            path,
            session_id,
            seen: None,
            helpers: HashMap::new(),
            inputs: 0,
        }
    }

    /// Starts over, to be read again from the start.
    fn reset(&mut self, window: Duration) {
        self.reducer = reducer(&self.path, window);
        self.seen = None;
        self.helpers.clear();
    }

    fn read(&mut self, window: Duration) -> Fed {
        let Ok(meta) = fs::metadata(&self.path) else {
            return Fed::Nothing;
        };
        let (len, mtime) = (meta.len(), meta.modified().unwrap_or(UNIX_EPOCH));
        let inode = (meta.dev(), meta.ino());
        if self.seen == Some((len, mtime, inode)) {
            return Fed::Nothing;
        }
        let first = self.seen.is_none();
        let mut read = Fed::Appended;
        let replaced = self.seen.is_some_and(|(_, _, i)| i != inode);
        if replaced || len < self.reducer.received() {
            self.reset(window);
            read = Fed::Replaced;
        }
        let Ok(mut file) = File::open(&self.path) else {
            return Fed::Nothing;
        };
        let before = self.reducer.consumed();
        if file.seek(SeekFrom::Start(before)).is_err() {
            return Fed::Nothing;
        }
        agent_tail::read_from(
            &mut file,
            &mut self.reducer,
            READ_CHUNK,
            Timestamp::from(mtime),
            &self.path,
        );
        // The user's entries among the lines just taken in, after the first
        // read: what an answer to a send made while at work waits for.
        let after = self.reducer.consumed();
        if !first && !matches!(read, Fed::Replaced) && after > before {
            let mut bytes = Vec::new();
            let taken = file
                .seek(SeekFrom::Start(before))
                .and_then(|_| (&mut file).take(after - before).read_to_end(&mut bytes));
            if taken.is_ok() {
                self.inputs += agent_tail::parse_lines(Harness::ClaudeCode, &bytes, ParseMode::Lean)
                    .filter(|e| !e.sidechain && is_input(&e.kind))
                    .count() as u64;
            }
        }
        self.seen = Some((len, mtime, inode));
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

/// Whether an entry is the user's: a prompt that starts a turn, or an
/// interrupt.
fn is_input(kind: &TailEvent) -> bool {
    match kind {
        TailEvent::UserPrompt { starts_turn, .. } => *starts_turn,
        TailEvent::TurnEnd { reason, .. } => {
            matches!(reason, agent_tail::TurnEndReason::Interrupted)
        }
        _ => false,
    }
}

fn reducer(path: &Path, window: Duration) -> SessionReducer {
    let options = ReducerOptions {
        idle_after: window,
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
    /// What quiet is measured from: the later of the last entry that moved
    /// the activity and the last write.
    pub quiet_since: Option<Timestamp>,
    /// Nothing has been written for the idle debounce. `activity` is still
    /// what it was before.
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
    /// When it was read: what an inferred permission prompt's quiet is
    /// measured to. None before a transcript is found.
    pub as_of: Option<Timestamp>,
    /// The agent's screen shows a permission dialog (see
    /// [`approval_on_screen`]), which confirms an inferred one at once.
    /// Set by the caller, which can read the screen.
    pub prompt_on_screen: bool,
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

/// The state clients see for a firm activity, while the agent's session
/// runs (PRD: MCP interface, States). Asleep, paused and exited are
/// `vibed`'s. An inferred permission prompt is [`Status::state`]'s call.
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
            quiet_since: None,
            quiet: false,
            title: None,
            line: None,
            pending_wakeups: Vec::new(),
            stop_reason: None,
            resets_at: None,
            error: None,
            transcript: None,
            as_of: None,
            prompt_on_screen: false,
        }
    }

    fn from_state(
        s: &SessionState,
        quiet: bool,
        quiet_since: Option<Timestamp>,
        transcript: &Path,
        now: Timestamp,
    ) -> Status {
        let activity = activity(s.activity);
        Status {
            activity,
            certainty: certainty(s.certainty),
            // Before an entry that moves it, agent-tail's `since` is only
            // the file's mtime.
            since: s.since.filter(|_| activity != Activity::Unknown),
            last_write: s.last_write,
            quiet_since,
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
            as_of: Some(now),
            prompt_on_screen: false,
        }
    }

    /// The state clients see, while the agent's session runs: what
    /// [`state_of`] says, but an inferred permission prompt is still busy
    /// until it's confirmed (see [`Status::asks_approval`]). Claude in auto
    /// mode writes nothing while a long tool call runs, so 20 s of quiet
    /// mid-call looks just like a prompt; only a real one raises the hand.
    pub fn state(&self) -> AgentState {
        if self.activity == Activity::AwaitingApproval && !self.asks_approval() {
            return AgentState::Busy;
        }
        state_of(self.activity)
    }

    /// Whether it's surely waiting on a permission prompt: agent-tail says
    /// so firmly, the screen shows the dialog, or the guess has stood for
    /// [`INFERRED_QUIET`] (PRD: Going to sleep, Permission prompts).
    pub fn asks_approval(&self) -> bool {
        match (self.activity, self.certainty) {
            (Activity::AwaitingApproval, Certainty::Firm) => true,
            (Activity::AwaitingApproval, Certainty::Inferred) => {
                self.prompt_on_screen || self.as_of.is_some_and(|now| self.inferred_quiet(now))
            }
            _ => false,
        }
    }

    /// An inferred permission prompt not yet confirmed: what the caller
    /// should look at the screen for.
    pub fn unconfirmed_approval(&self) -> bool {
        self.activity == Activity::AwaitingApproval && !self.asks_approval()
    }

    /// Quiet for [`INFERRED_QUIET`] as of `now`.
    fn inferred_quiet(&self, now: Timestamp) -> bool {
        self.quiet_since
            .is_some_and(|t| now.as_secs_f64() - t.as_secs_f64() >= INFERRED_QUIET.as_secs_f64())
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

    /// Firmly at rest or asking something, or waiting on what may be a
    /// permission prompt for [`INFERRED_QUIET`]: what an answer to a send
    /// waits for.
    pub fn settled(&self, now: Timestamp) -> bool {
        if self.at_rest() {
            return true;
        }
        match (self.activity, self.certainty) {
            (Activity::AwaitingApproval, Certainty::Firm) => true,
            (Activity::AwaitingApproval, Certainty::Inferred) => self.inferred_quiet(now),
            _ => false,
        }
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
        // prompt: a guess, and it says so. Until the guess has stood for
        // INFERRED_QUIET it's still at work, as a long tool run looks the
        // same; past that it's a question, and stays one past the window.
        let s = tail.status(t("2026-10-30T12:00:30Z"));
        assert_eq!(
            (s.activity, s.certainty, s.state()),
            (
                Activity::AwaitingApproval,
                Certainty::Inferred,
                AgentState::Busy
            )
        );
        assert!(s.unconfirmed_approval() && !s.asks_approval());
        for now in ["2026-10-30T12:05:01Z", "2026-10-30T14:00:00Z"] {
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
            assert!(!s.at_rest() && s.asks_approval() && !s.unconfirmed_approval());
        }
        // The dialog on screen confirms the guess at once.
        let mut s = tail.status(t("2026-10-30T12:00:30Z"));
        s.prompt_on_screen = true;
        assert_eq!(s.state(), AgentState::Question);
        assert!(!s.unconfirmed_approval());
    }

    /// The false hand-up of #46: Claude in auto mode runs `sleep 60`, so
    /// its transcript stalls mid tool call for a minute with no prompt on
    /// screen. It's at work all the while, never a question.
    #[test]
    fn an_auto_mode_sleep_60_does_not_raise_the_hand_at_20s() {
        let w = world();
        let path = w.project.join(format!("{ID}.jsonl"));
        let mode = serde_json::json!({"type": "permission-mode", "permissionMode": "auto",
            "sessionId": ID});
        append(&path, &format!("{mode}\n"));
        append(
            &path,
            &user("/w/app", "2026-10-30T12:00:00Z", "wait a minute"),
        );
        let call = serde_json::json!({"type": "assistant", "sessionId": ID, "cwd": "/w/app",
            "permissionMode": "auto", "timestamp": "2026-10-30T12:00:01Z",
            "message": {"id": "m1", "role": "assistant", "stop_reason": "tool_use",
                "content": [{"type": "tool_use", "id": "toolu_s", "name": "Bash",
                    "input": {"command": "sleep 60", "description": "Wait a minute"}}]}});
        append(&path, &format!("{call}\n"));
        let mut tail = Tail::new(w.project.clone(), &w.worktree, ID, Duration::from_secs(60));
        tail.poll();
        let screen = "\
⏺ Bash(sleep 60)
  ⎿  Running…
✻ Waiting… (25s · esc to interrupt)
  ⏵⏵ auto mode on (shift+tab to cycle)
";
        for now in [
            "2026-10-30T12:00:21Z",
            "2026-10-30T12:00:25Z",
            "2026-10-30T12:00:45Z",
            "2026-10-30T12:01:00Z",
        ] {
            let mut s = tail.status(t(now));
            s.prompt_on_screen = crate::approval_on_screen(screen);
            assert_eq!(s.state(), AgentState::Busy, "{now}: {s:?}");
            assert!(!s.asks_approval(), "{now}");
        }
        // The command ends and Claude finishes its turn.
        let result = serde_json::json!({"type": "user", "sessionId": ID, "cwd": "/w/app",
            "timestamp": "2026-10-30T12:01:01Z", "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_s", "content": ""}]}});
        append(&path, &format!("{result}\n"));
        append(&path, &reply("/w/app", "2026-10-30T12:01:03Z", "Waited."));
        tail.poll();
        let s = tail.status(t("2026-10-30T12:01:04Z"));
        assert_eq!((s.activity, s.state()), (Activity::Done, AgentState::Idle));
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
        let mark = tail.mark(t("2026-10-30T12:02:00Z"));
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
        let mark = tail.mark(now);
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
        let mark = tail.mark(t("2026-10-30T12:00:14Z"));
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
        let mark = tail.mark(t("2026-10-30T12:00:02Z"));
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
        // Read on every poll, but not watched: a descriptor each would add
        // up on macOS.
        assert!(!tail.watch_paths().contains(&helper));
        assert!(tail.watch_paths().contains(&path));
        // Its `.meta.json` says it finished.
        fs::write(helper.with_extension("meta.json"), r#"{"finished":true}"#).unwrap();
        assert!(tail.poll());
        assert_eq!(
            tail.status(t("2026-10-30T12:00:06Z")).activity,
            Activity::Done
        );
    }

    #[test]
    fn background_work_answers_unless_waited_for() {
        let w = world();
        let path = w.project.join(format!("{ID}.jsonl"));
        let mut tail = Tail::new(w.project.clone(), &w.worktree, ID, Duration::from_secs(60));
        let mark = tail.mark(t("2026-10-30T12:00:00Z"));
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
        tail.poll();
        let now = t("2026-10-30T12:00:05Z");
        assert_eq!(tail.status(now).activity, Activity::BackgroundWork);
        assert!(tail.answered_after(&mark, now, false));
        assert!(!tail.answered_after(&mark, now, true));
        assert!(!tail.answered(&mark, now));
        fs::write(helper.with_extension("meta.json"), r#"{"finished":true}"#).unwrap();
        tail.poll();
        let now = t("2026-10-30T12:00:06Z");
        assert!(tail.answered_after(&mark, now, true));
        assert!(tail.answered(&mark, now));
    }

    fn bash(at: &str, id: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type": "assistant", "sessionId": ID, "cwd": "/w/app", "timestamp": at,
                "message": {"id": format!("m-{id}"), "role": "assistant", "stop_reason": "tool_use",
                    "content": [{"type": "tool_use", "id": id, "name": "Bash",
                        "input": {"command": "cargo build"}}]}})
        )
    }

    fn result(at: &str, id: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type": "user", "sessionId": ID, "cwd": "/w/app", "timestamp": at,
                "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id,
                    "content": "ok"}]}})
        )
    }

    #[test]
    fn a_long_silent_tool_run_is_no_answer_until_it_is_quiet_for_long() {
        let w = world();
        let path = w.project.join(format!("{ID}.jsonl"));
        append(&path, &user("/w/app", "2026-10-30T12:00:00Z", "first"));
        append(&path, &reply("/w/app", "2026-10-30T12:00:01Z", "ok"));
        let mut tail = Tail::new(w.project.clone(), &w.worktree, ID, Duration::from_secs(60));
        tail.poll();
        let mark = tail.mark(t("2026-10-30T12:00:02Z"));
        append(&path, &user("/w/app", "2026-10-30T12:00:03Z", "build it"));
        append(&path, &bash("2026-10-30T12:00:04Z", "toolu_b"));
        tail.poll();
        // 30 s without a result: maybe a permission prompt, maybe a long
        // build. Not an answer yet; after five quiet minutes, it is.
        let s = tail.status(t("2026-10-30T12:00:34Z"));
        assert_eq!(
            (s.activity, s.certainty),
            (Activity::AwaitingApproval, Certainty::Inferred)
        );
        assert!(!tail.answered(&mark, t("2026-10-30T12:00:34Z")));
        assert!(!tail.answered(&mark, t("2026-10-30T12:04:00Z")));
        assert!(tail.answered(&mark, t("2026-10-30T12:05:05Z")));
        // A wait, which can't outlast five minutes, takes the prompt once
        // agent-tail infers it, and not before.
        assert!(!tail.answered_after(&mark, t("2026-10-30T12:00:10Z"), false));
        assert!(tail.answered_after(&mark, t("2026-10-30T12:00:34Z"), false));
        assert!(tail.answered_after(&mark, t("2026-10-30T12:00:34Z"), true));
    }

    #[test]
    fn a_short_debounce_still_sees_a_permission_prompt() {
        let w = world();
        let path = w.project.join(format!("{ID}.jsonl"));
        append(&path, &user("/w/app", "2026-10-30T12:00:00Z", "build it"));
        append(&path, &bash("2026-10-30T12:00:01Z", "toolu_b"));
        let mut tail = Tail::new(w.project.clone(), &w.worktree, ID, Duration::ZERO);
        tail.poll();
        let s = tail.status(t("2026-10-30T12:00:10Z"));
        assert_eq!((s.activity, s.quiet), (Activity::RunningTool, true));
        let s = tail.status(t("2026-10-30T12:00:40Z"));
        assert_eq!(
            (s.activity, s.certainty, s.quiet),
            (Activity::AwaitingApproval, Certainty::Inferred, true)
        );
    }

    #[test]
    fn a_send_while_at_work_waits_for_its_own_turn() {
        let w = world();
        let path = w.project.join(format!("{ID}.jsonl"));
        append(&path, &user("/w/app", "2026-10-30T12:00:00Z", "build it"));
        append(&path, &bash("2026-10-30T12:00:01Z", "toolu_b"));
        let mut tail = Tail::new(w.project.clone(), &w.worktree, ID, Duration::from_secs(60));
        tail.poll();
        let mark = tail.mark(t("2026-10-30T12:00:02Z"));
        // The turn it was in ends: no answer to what was sent during it.
        append(&path, &result("2026-10-30T12:00:05Z", "toolu_b"));
        append(&path, &reply("/w/app", "2026-10-30T12:00:06Z", "built"));
        tail.poll();
        assert_eq!(
            tail.status(t("2026-10-30T12:00:07Z")).activity,
            Activity::Done
        );
        assert!(!tail.answered(&mark, t("2026-10-30T12:00:07Z")));
        // Then Claude reads the send, as a prompt of its own, and answers.
        append(
            &path,
            &user("/w/app", "2026-10-30T12:00:08Z", "and test it"),
        );
        append(&path, &reply("/w/app", "2026-10-30T12:00:09Z", "tested"));
        tail.poll();
        assert!(tail.answered(&mark, t("2026-10-30T12:00:10Z")));
    }

    #[test]
    fn a_clear_alone_keeps_the_session_that_says_something() {
        let w = world();
        let old = w.project.join(format!("{ID}.jsonl"));
        append(&old, &user("/w/app", "2026-10-30T12:00:00Z", "first"));
        append(&old, &reply("/w/app", "2026-10-30T12:00:01Z", "ok"));
        let mut tail = Tail::new(w.project.clone(), &w.worktree, ID, Duration::from_secs(60));
        tail.poll();
        // `/clear` writes a new session with only its command, which moves
        // nothing: the agent is still done, not starting.
        std::thread::sleep(Duration::from_millis(20));
        let new_id = "c1ea4ed0-0000-4000-8000-000000000000";
        let new = w.project.join(format!("{new_id}.jsonl"));
        let clear = serde_json::json!({"type": "user", "sessionId": new_id, "cwd": "/w/app",
            "timestamp": "2026-10-30T12:01:00Z", "message": {"role": "user",
            "content": "<command-name>/clear</command-name>\n<command-message>clear</command-message>\n<command-args></command-args>"}});
        append(&new, &format!("{clear}\n"));
        let mark = tail.mark(t("2026-10-30T12:01:00Z"));
        tail.poll();
        assert_eq!(tail.session_id(), ID);
        let s = tail.status(t("2026-10-30T12:01:01Z"));
        assert_eq!((s.activity, s.state()), (Activity::Done, AgentState::Idle));
        assert!(!tail.answered(&mark, t("2026-10-30T12:01:01Z")));
        // The old session written to again later, by a late title, isn't
        // gone back to once the new one says something.
        append(&new, &user("/w/app", "2026-10-30T12:02:00Z", "second"));
        tail.poll();
        assert_eq!(tail.session_id(), new_id);
        std::thread::sleep(Duration::from_millis(20));
        append(&old, &titled("2026-10-30T12:03:00Z", "First"));
        assert!(!tail.poll());
        assert_eq!(tail.session_id(), new_id);
        // Nor after a restart, when the record says the new session.
        let mut again = Tail::new(
            w.project.clone(),
            &w.worktree,
            new_id,
            Duration::from_secs(60),
        );
        again.poll();
        assert_eq!(again.session_id(), new_id);
    }

    #[test]
    fn a_transcript_replaced_by_one_as_long_is_read_again() {
        let w = world();
        let path = w.project.join(format!("{ID}.jsonl"));
        append(&path, &user("/w/app", "2026-10-30T12:00:00Z", "aaaa"));
        let mut tail = Tail::new(w.project.clone(), &w.worktree, ID, Duration::from_secs(60));
        tail.poll();
        let other = w.project.join("other.tmp");
        append(&other, &user("/w/app", "2026-10-30T12:00:05Z", "bbbbbbbb"));
        fs::rename(&other, &path).unwrap();
        assert!(tail.poll());
        assert_eq!(
            tail.status(t("2026-10-30T12:00:06Z")).line.as_deref(),
            Some("› bbbbbbbb")
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
