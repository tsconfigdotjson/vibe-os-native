//! The status engine in `vibed`: one [`Tail`] per agent, on the transcript
//! its coding agent writes, in its Claude home or Pi's sessions (PRD: State and lifecycle,
//! Matching an agent to its transcript). Only those transcripts are read,
//! never a scan of the whole home.
//!
//! [`Core::spawn_status`] polls every agent's transcript on each
//! file-system event and every second, and publishes `agent.state` and
//! `agent.status` events as they change. Calls that report on agents read
//! the transcript up to now first, so they never lag the loop. Tails start
//! empty in a new `vibed` and read each transcript whole: a restart
//! rebuilds every state from the transcripts.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::unix::AsyncFd;
use vibe_core::limits::{self, HostLimits};
use vibe_core::model::{AgentKind, AgentState, EventKind, LOCAL_HOST};
use vibe_core::rpc::{self, ErrorObject};
use vibe_core::store::AgentRecord;
use vibe_status::{Mark, POLL_EVERY, Status, Tail, Timestamp, Watcher};

use crate::core::{Core, lock, store_error};

/// How long after a file-system event the transcripts are read, so a burst
/// of writes is read once.
const SETTLE: Duration = Duration::from_millis(50);

/// How long a Pi with nothing to show for itself has written nothing before its
/// screen is read for why.
const PI_SCREEN_AFTER: Duration = Duration::from_secs(3);

/// Beside a Pi agent's other files: when it was launched with nothing to do.
const PI_AT_REST: &str = "pi-at-rest";

/// This machine's clock, which is what a transcript's file time is by.
pub(crate) fn wall_clock() -> Timestamp {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    Timestamp::from_secs_f64(now.as_secs_f64())
}

/// Notes that a Pi agent was just launched `at_rest` with nothing to do, or not.
/// Pi writes nothing on a resume, so a turn the transcript left open stays open
/// there; this says it's over ([`Status::at_rest_since`]). In a file, for a
/// `vibed` that restarts and reads the transcript afresh.
pub(crate) fn note_pi_at_rest(handle: &vibe_env::Handle, at_rest: bool) {
    let path = handle.agent_dir.join(PI_AT_REST);
    let done = if at_rest {
        std::fs::write(&path, wall_clock().as_secs_f64().to_string())
    } else {
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    };
    if let Err(e) = done {
        crate::log!("vibed: can't note {}: {e}", path.display());
    }
}

impl Core {
    fn now_ts(&self) -> Timestamp {
        Timestamp::from_secs_f64(self.now() as f64)
    }

    /// How long another host gets to answer a listing or a lookup. `host.toml`
    /// is read on every use; one that can't be read gives the default, as
    /// admission says why it can't be.
    pub(crate) fn peer_read(&self) -> Duration {
        let secs =
            limits::load(&self.layout.host_config()).map_or(limits::PEER_READ, |l| l.peer_read);
        Duration::from_secs(secs)
    }

    /// The idle debounce, as above.
    pub(crate) fn idle_debounce(&self) -> Duration {
        let secs = limits::load(&self.layout.host_config())
            .map_or(HostLimits::default().idle_debounce, |l| l.idle_debounce);
        Duration::from_secs(secs)
    }

    /// What `a`'s transcript says it's doing now: what was written since
    /// it was last read is read first. If a `/clear` moved it to another
    /// session, the agent's record follows, so a resume continues that one.
    pub(crate) async fn agent_status(&self, a: &mut AgentRecord, debounce: Duration) -> Status {
        let status = self
            .with_tail(a, debounce, |t, now| t.status(now))
            .await
            .unwrap_or_else(Status::unknown);
        if a.agent == AgentKind::Pi {
            self.pi_read_status(a, debounce, status).await.0
        } else {
            status
        }
    }

    /// What Pi said as it gave up on `a`, if that's how its session ended: with a
    /// model it won't take, say, it's gone before it writes or draws anything, and
    /// this is all that says why. Pi has the last word on what a model is (PRD:
    /// Profiles), so this is how it's heard. A new session makes it old news.
    async fn pi_gave_up_with(&self, a: &AgentRecord) -> Option<String> {
        if a.state == AgentState::Asleep
            || self.is_busy(&a.agent_ref)
            || !self
                .layout
                .agent_dir(&a.agent_ref)
                .join(vibe_core::shim::RECORD_FILE)
                .is_file()
            || self.session_runs(&a.agent_ref).await
        {
            return None;
        }
        let (layout, agent_ref) = (self.layout.clone(), a.agent_ref.clone());
        let record =
            tokio::task::spawn_blocking(move || crate::sessions::read_record(&layout, &agent_ref))
                .await
                .ok()??;
        // Ended on its own and not well: not told to go, and not a quit.
        if record.exit.code.unwrap_or(0) == 0 {
            return None;
        }
        vibe_status::pi_error_at_exit(&record.output.screen)
    }

    /// When the last thing typed to `a` was, if its transcript has shown nothing
    /// of it since. Any later turn fails as a first one does: an answered agent
    /// reads as done, and goes on reading so.
    async fn pi_unanswered_send(
        &self,
        a: &mut AgentRecord,
        debounce: Duration,
    ) -> Option<Timestamp> {
        let at = *lock(&self.pi_sent_at).get(&a.agent_ref)?;
        let mark = lock(&self.sends).get(&a.agent_ref).cloned()?;
        let moved = self
            .with_tail(a, debounce, move |t, _| t.moved_since(&mark))
            .await?;
        (!moved).then_some(at)
    }

    /// What a Pi's transcript can't say, and whether the screen had to say it.
    /// It writes nothing on a resume, so a turn left open from before stays open
    /// there. And a turn that fails before it starts, for want of a login, writes
    /// nothing and shows only on its screen. Either would read as it did before:
    /// busy for ever, or a send that never got an answer. And one that gave up
    /// before it drew anything has only what it left behind to say why.
    pub(crate) async fn pi_read_status(
        &self,
        a: &mut AgentRecord,
        debounce: Duration,
        status: Status,
    ) -> (Status, bool) {
        if let Some(error) = self.pi_gave_up_with(a).await {
            return (status.failed_on_screen(error), true);
        }
        let sent = self.pi_unanswered_send(a, debounce).await;
        if !status.open_turn() && sent.is_none() {
            return (status, false);
        }
        let Ok(handle) = Core::handle_of(a) else {
            return (status, false);
        };
        let at_rest = std::fs::read_to_string(handle.agent_dir.join(PI_AT_REST))
            .ok()
            .and_then(|t| t.trim().parse::<f64>().ok())
            .map(Timestamp::from_secs_f64);
        let status = match at_rest {
            Some(at) => status.at_rest_since(at),
            None => status,
        };
        let now = wall_clock();
        let look = match sent {
            Some(at) => now.as_secs_f64() - at.as_secs_f64() >= PI_SCREEN_AFTER.as_secs_f64(),
            None => status.pi_may_have_failed(now, PI_SCREEN_AFTER),
        };
        if !look {
            return (status, false);
        }
        let socket = self.layout.session_socket(&a.agent_ref);
        let screen = tokio::task::spawn_blocking(move || crate::talk::screen_now(&socket))
            .await
            .ok()
            .and_then(Result::ok);
        match screen.and_then(|o| vibe_status::pi_error_on_screen(&o.screen)) {
            Some(error) => (status.failed_on_screen(error), true),
            None => (status, false),
        }
    }

    /// Reads `a`'s transcript up to now, then runs `f` on its tail. Each
    /// agent's tail has a lock of its own, and is read on the blocking
    /// pool: the first read after a restart is the whole transcript. None
    /// until its compartment's handle, which says where its transcripts are,
    /// is written.
    pub(crate) async fn with_tail<T: Send + 'static>(
        &self,
        a: &mut AgentRecord,
        debounce: Duration,
        f: impl FnOnce(&Tail, Timestamp) -> T + Send + 'static,
    ) -> Option<T> {
        let home = Core::handle_of(a).ok()?.home;
        let worktree = PathBuf::from(&a.worktree);
        let tail = Arc::clone(
            lock(&self.tails)
                .entry(a.agent_ref.clone())
                .or_insert_with(|| {
                    Arc::new(Mutex::new(Tail::of(
                        a.agent,
                        home.transcripts_dir(&worktree),
                        &worktree,
                        &a.session_id,
                        debounce,
                    )))
                }),
        );
        let now = self.now_ts();
        let (out, session_id) = tokio::task::spawn_blocking(move || {
            let mut t = lock(&tail);
            if t.transcript().is_none() {
                // Claude's directory for a long path is known once it's made.
                t.set_project_dir(home.transcripts_dir(&worktree));
            }
            t.set_idle_debounce(debounce);
            t.poll();
            (f(&t, now), t.session_id().to_owned())
        })
        .await
        .ok()?;
        if session_id != a.session_id {
            match self
                .store()
                .set_agent_session(&a.host, &a.agent_ref, &session_id)
            {
                Ok(()) => {
                    crate::log!(
                        "vibed: {} is in session {session_id} now (it was {})",
                        a.agent_ref,
                        a.session_id
                    );
                    a.session_id = session_id;
                }
                Err(e) => crate::log!("vibed: can't record {}'s session: {e}", a.agent_ref),
            }
        }
        Some(out)
    }

    /// Forgets an agent's tail, once the agent is gone.
    pub(crate) fn forget_status(&self, agent_ref: &str) {
        lock(&self.tails).remove(agent_ref);
        lock(&self.sends).remove(agent_ref);
        lock(&self.typing).remove(agent_ref);
        lock(&self.asleep_since).running(agent_ref);
        self.pressure_unpause(agent_ref);
    }

    /// The state clients see of an agent whose session runs: what its
    /// transcript says, but a permission prompt agent-tail only guesses at
    /// is what its screen says: a question once the screen shows the
    /// dialog, and busy while it doesn't, so a long silent tool run never
    /// raises the hand. Only a screen that can't be read leaves it to the
    /// guess having stood for `INFERRED_QUIET` (see [`Status::state`]).
    pub(crate) async fn running_state(&self, agent_ref: &str, status: &Status) -> AgentState {
        if !status.inferred_approval() {
            return status.state();
        }
        let socket = self.layout.session_socket(agent_ref);
        let screen = tokio::task::spawn_blocking(move || crate::talk::screen_now(&socket))
            .await
            .ok()
            .and_then(Result::ok);
        let mut status = status.clone();
        status.prompt_on_screen = screen.map(|o| vibe_status::approval_on_screen(&o.screen));
        status.state()
    }

    /// The state clients see: `vibed`'s own `asleep` and `paused` while
    /// they hold, `exited` once its session has gone, and otherwise what
    /// its transcript and screen say ([`Core::running_state`]).
    pub(crate) async fn observed_state(&self, a: &AgentRecord, status: &Status) -> AgentState {
        // Frozen or saved to disk; a send wakes it (#6). Its session isn't
        // asked: a saved one has none.
        if a.state == AgentState::Asleep {
            return AgentState::Asleep;
        }
        let runs = self.session_runs(&a.agent_ref).await;
        match (runs, a.state) {
            (true, AgentState::Paused) => AgentState::Paused,
            (false, _) => AgentState::Exited,
            (true, _) => self.running_state(&a.agent_ref, status).await,
        }
    }

    /// Where `agent_ref`'s transcript is now, to wait for the answer to a
    /// send from: take it just before the send (PRD: MCP interface,
    /// `vibe_wait`).
    pub async fn mark_send(
        &self,
        host: Option<&str>,
        agent_ref: &str,
    ) -> Result<Mark, ErrorObject> {
        let host = self.host(host)?;
        let mut a = self.store().agent(host, agent_ref).map_err(store_error)?;
        let debounce = self.idle_debounce();
        Ok(self
            .with_tail(&mut a, debounce, |t, now| t.mark(now))
            .await
            .unwrap_or_default())
    }

    /// Whether `agent_ref` has answered the send `mark` was taken before
    /// (see [`Tail::answered`]). Quiet alone isn't an answer. An agent that
    /// exited won't answer; its state says so.
    pub async fn answered(
        &self,
        host: Option<&str>,
        agent_ref: &str,
        mark: &Mark,
    ) -> Result<bool, ErrorObject> {
        let host = self.host(host)?;
        let mut a = self.store().agent(host, agent_ref).map_err(store_error)?;
        let debounce = self.idle_debounce();
        let mark = mark.clone();
        Ok(self
            .with_tail(&mut a, debounce, move |t, now| t.answered(&mark, now))
            .await
            .unwrap_or(false))
    }

    /// Reads every agent's transcript, records and publishes what changed,
    /// and returns the paths to watch until the next time. `published` is
    /// each agent's status as last sent. Each agent's session is asked
    /// whether it runs on a `full` refresh, the one every second, and
    /// whenever its status changed; a file-system event alone needs no more.
    pub async fn refresh_status(
        &self,
        published: &mut HashMap<String, rpc::AgentStatus>,
        full: bool,
    ) -> Vec<PathBuf> {
        let Ok(records) = self.store().agents(LOCAL_HOST) else {
            return Vec::new();
        };
        let refs: HashSet<&str> = records.iter().map(|a| a.agent_ref.as_str()).collect();
        lock(&self.tails).retain(|r, _| refs.contains(r.as_str()));
        lock(&self.sends).retain(|r, _| refs.contains(r.as_str()));
        lock(&self.pi_sent_at).retain(|r, _| refs.contains(r.as_str()));
        published.retain(|r, _| refs.contains(r.as_str()));
        let debounce = self.idle_debounce();
        let mut paths = Vec::new();
        for mut a in records {
            // A start, delete or resume under way records its own state.
            if a.starting || self.is_busy(&a.agent_ref) {
                continue;
            }
            let status = self.agent_status(&mut a, debounce).await;
            let tail = lock(&self.tails).get(&a.agent_ref).cloned();
            if let Some(tail) = tail {
                paths.extend(lock(&tail).watch_paths());
            }
            let wire = status.to_rpc();
            let changed = published.get(&a.agent_ref) != Some(&wire);
            if !full && !changed {
                continue;
            }
            let before = published
                .get(&a.agent_ref)
                .map(|s| (s.activity, s.certainty));
            if before != Some((wire.activity, wire.certainty)) {
                crate::log!(
                    "vibed: {} is {} ({}) now{}",
                    a.agent_ref,
                    wire.activity,
                    wire.certainty,
                    before.map_or(String::new(), |(a, c)| format!(", was {a} ({c})"))
                );
            }
            let was = a.state;
            let state = self.observed_state(&a, &status).await;
            // A delete may have finished meanwhile, or a sleep or a wake
            // changed it: then it's left as that made it.
            if self.is_busy(&a.agent_ref) {
                continue;
            }
            if state != was && !self.set_state_if(&mut a, was, state) {
                continue;
            }
            if changed {
                let was_limited = published
                    .get(&a.agent_ref)
                    .is_some_and(|s| s.stop_reason.is_some());
                if let Some(until) = status.limited_until()
                    && !was_limited
                {
                    crate::log!("vibed: {} stopped at a usage limit: {until}", a.agent_ref);
                }
                let mut e = self.event(EventKind::AgentStatus);
                e.project = Some(a.project.clone());
                e.agent = Some(a.agent_ref.clone());
                e.status = Some(wire.clone());
                self.publish(e);
                published.insert(a.agent_ref.clone(), wire);
            }
        }
        paths.sort();
        paths.dedup();
        paths
    }

    pub(crate) fn is_busy(&self, agent_ref: &str) -> bool {
        lock(&self.agents_busy).contains(agent_ref)
    }

    /// Runs [`Core::refresh_status`] on every file-system event in a
    /// transcript, and every second whatever happens.
    pub fn spawn_status(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let core = Arc::clone(self);
        tokio::spawn(async move {
            // SAFETY: the watcher owns its descriptor, and keeps it open and
            // the same for as long as the AsyncFd holds it.
            let watcher = Watcher::new().and_then(|w| {
                unsafe { AsyncFd::register(w) }.map_err(|e| std::io::Error::other(e.to_string()))
            });
            let mut watcher = match watcher {
                Ok(w) => Some(w),
                Err(e) => {
                    crate::log!(
                        "vibed: no file-system events for transcripts ({e}); polling every {}s",
                        POLL_EVERY.as_secs()
                    );
                    None
                }
            };
            let mut published = HashMap::new();
            // When sessions were last all asked, so a stream of events
            // can't put that off.
            let mut last_full: Option<Instant> = None;
            loop {
                let full = last_full.is_none_or(|t| t.elapsed() >= POLL_EVERY);
                if full {
                    last_full = Some(Instant::now());
                }
                let paths = core.refresh_status(&mut published, full).await;
                let Some(w) = &mut watcher else {
                    tokio::time::sleep(POLL_EVERY).await;
                    continue;
                };
                w.get_mut().watch(&paths);
                tokio::select! {
                    ready = w.readable_mut() => {
                        if let Ok(mut guard) = ready {
                            guard.clear_ready();
                            guard.get_inner_mut().drain();
                        }
                        tokio::time::sleep(SETTLE).await;
                    }
                    _ = tokio::time::sleep(POLL_EVERY) => {}
                }
            }
        })
    }
}
