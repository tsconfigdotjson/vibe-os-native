//! The status engine in `vibed`: one [`Tail`] per agent, on the transcript
//! Claude writes in the agent's Claude home (PRD: State and lifecycle,
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
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::unix::AsyncFd;
use vibe_core::limits::{self, HostLimits};
use vibe_core::model::{AgentState, EventKind, LOCAL_HOST};
use vibe_core::rpc::{self, ErrorObject};
use vibe_core::store::AgentRecord;
use vibe_status::{Mark, POLL_EVERY, Status, Tail, Timestamp, Watcher};

use crate::core::{Core, lock, store_error};

/// How long after a file-system event the transcripts are read, so a burst
/// of writes is read once.
const SETTLE: Duration = Duration::from_millis(50);

impl Core {
    fn now_ts(&self) -> Timestamp {
        Timestamp::from_secs_f64(self.now() as f64)
    }

    /// The idle debounce, which is agent-tail's idle window.
    fn idle_debounce(&self) -> Duration {
        let secs = limits::load(&self.layout.host_config())
            .map_or(HostLimits::default().idle_debounce, |l| l.idle_debounce);
        Duration::from_secs(secs)
    }

    /// What `a`'s transcript says it's doing now: what was written since
    /// it was last read is read first. If a `/clear` moved it to another
    /// session, the agent's record follows, so a resume continues that one.
    pub(crate) fn agent_status(&self, a: &mut AgentRecord) -> Status {
        let debounce = self.idle_debounce();
        let now = self.now_ts();
        let (status, session_id) = {
            let mut tails = lock(&self.tails);
            // Its Claude home is in its compartment's handle; until that's
            // written, there's nothing to read.
            let Ok(handle) = Core::handle_of(a) else {
                return Status::unknown();
            };
            let worktree = PathBuf::from(&a.worktree);
            let tail = tails.entry(a.agent_ref.clone()).or_insert_with(|| {
                Tail::new(
                    handle.claude.project_dir(&worktree),
                    &worktree,
                    &a.session_id,
                    debounce,
                )
            });
            if tail.transcript().is_none() {
                // Claude's directory for a long path is known once it's made.
                tail.set_project_dir(handle.claude.project_dir(&worktree));
            }
            tail.set_idle_debounce(debounce);
            tail.poll();
            (tail.status(now), tail.session_id().to_owned())
        };
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
        status
    }

    /// Forgets an agent's tail, once the agent is gone.
    pub(crate) fn forget_status(&self, agent_ref: &str) {
        lock(&self.tails).remove(agent_ref);
    }

    /// The state clients see: `vibed`'s own `asleep` and `paused` while
    /// they hold, `exited` once its session has gone, and otherwise what
    /// its transcript says.
    pub(crate) async fn observed_state(&self, a: &AgentRecord, status: &Status) -> AgentState {
        let runs = self.session_runs(&a.agent_ref).await;
        match (runs, a.state) {
            // Frozen or saved to disk; a send wakes it (#6).
            (_, AgentState::Asleep) => AgentState::Asleep,
            (true, AgentState::Paused) => AgentState::Paused,
            (false, _) => AgentState::Exited,
            (true, _) => status.state(),
        }
    }

    /// Where `agent_ref`'s transcript is now, to wait for the answer to a
    /// send from: take it just before the send (PRD: MCP interface,
    /// `vibe_wait`).
    pub fn mark_send(&self, host: Option<&str>, agent_ref: &str) -> Result<Mark, ErrorObject> {
        let host = self.host(host)?;
        let mut a = self.store().agent(host, agent_ref).map_err(store_error)?;
        self.agent_status(&mut a);
        Ok(lock(&self.tails)
            .get(agent_ref)
            .map(Tail::mark)
            .unwrap_or_default())
    }

    /// Whether `agent_ref` has answered the send `mark` was taken before:
    /// its transcript has moved on since, and it's at rest or asking
    /// something. Quiet alone isn't an answer. An agent that exited won't
    /// answer; its state says so.
    pub fn answered(
        &self,
        host: Option<&str>,
        agent_ref: &str,
        mark: &Mark,
    ) -> Result<bool, ErrorObject> {
        let host = self.host(host)?;
        let mut a = self.store().agent(host, agent_ref).map_err(store_error)?;
        self.agent_status(&mut a);
        let now = self.now_ts();
        Ok(lock(&self.tails)
            .get(agent_ref)
            .is_some_and(|t| t.answered(mark, now)))
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
        published.retain(|r, _| refs.contains(r.as_str()));
        let mut paths = Vec::new();
        for mut a in records {
            // A start, delete or resume under way records its own state.
            if a.starting || self.is_busy(&a.agent_ref) {
                continue;
            }
            let status = self.agent_status(&mut a);
            if let Some(tail) = lock(&self.tails).get(&a.agent_ref) {
                paths.extend(tail.watch_paths());
            }
            let wire = status.to_rpc();
            let changed = published.get(&a.agent_ref) != Some(&wire);
            if !full && !changed {
                continue;
            }
            let state = self.observed_state(&a, &status).await;
            if self.is_busy(&a.agent_ref) {
                continue;
            }
            if state != a.state {
                self.set_state(&mut a, state);
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

    fn is_busy(&self, agent_ref: &str) -> bool {
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
