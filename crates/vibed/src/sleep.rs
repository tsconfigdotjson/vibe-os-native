//! Going to sleep, and waking for a `/loop` (PRD: State and lifecycle,
//! Going to sleep). An agent at rest with nobody watching sleeps on its own
//! once the idle debounce has passed: frozen, or saved to disk, through its
//! backend's `Sleeper`. Policy here reads only the `vibe-env` traits and
//! `Capabilities`.
//!
//! **Idle** is all of:
//!
//! - **at rest**: a firm `done`, `interrupted`, `errored` (a usage limit
//!   included) or `awaiting_answer`; or an inferred `awaiting_approval`,
//!   which is how Claude records a permission prompt, once it has been quiet
//!   for five minutes (`vibe_status::INFERRED_QUIET`), with no process started since it began and its
//!   CPU near zero. Never `thinking`, `running_tool` or `background_work`;
//! - **no unseen processes**: nothing in its tree started more than
//!   [`UNSEEN_AFTER`] after Claude, but for what Claude started in that
//!   time (its MCP servers) and their own children. That catches a `nohup`
//!   job or a container left running;
//! - **wakeups**: none pending, or only firm `/loop` wakeups with a due
//!   time, each due later than the wake comes for it. A cron job (recurring
//!   ones included), an inferred wakeup or one with no due time keeps it
//!   awake;
//! - **nothing attached**;
//! - **the debounce**: all of that has held for `idle_debounce`, from both
//!   the transcript's last write and when this `vibed` first saw it.
//!
//! An asleep agent whose firm `/loop` wakeup is due within
//! [`WAKE_BEFORE_DUE`] is woken, so the loop fires on time. A send or an
//! attach wakes one too ([`crate::talk`], [`Core::attach_session`]).
//!
//! The same rule, without the debounce, picks the agents put to sleep
//! early, oldest first: to make room under `max_awake_agents`, and under
//! memory pressure. Never to make room in memory for a start (#47).

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use vibe_core::model::{Activity, AgentState, Certainty, LOCAL_HOST, WakeupKind};
use vibe_core::store::AgentRecord;
use vibe_env::{Backend, Handle, Proc};
use vibe_status::{Status, Timestamp};

use crate::agents::{blocking, size};
use crate::core::{Core, lock};
use crate::sessions::{self, Probe};

/// How long after Claude starts its MCP servers may still start.
pub const UNSEEN_AFTER: Duration = Duration::from_secs(30);

/// How long before a firm `/loop` wakeup is due its agent is woken: a
/// restore takes at most a few seconds.
pub const WAKE_BEFORE_DUE: Duration = Duration::from_secs(30);

/// How often the idle rules are looked at.
pub const SLEEP_EVERY: Duration = Duration::from_secs(1);

/// How long a woken agent is left awake whatever it does, so a send can
/// type into it and an attach can connect, even with no debounce.
pub const AFTER_WAKE: Duration = Duration::from_secs(5);

/// Below this share of one core, an agent's CPU counts as near zero.
pub const QUIET_CPU: f64 = 0.02;

/// How long a sleep or a wake that failed waits before it's tried again.
pub const RETRY_AFTER: Duration = Duration::from_secs(60);

/// Disk left free beside a new snapshot.
pub const SNAPSHOT_MARGIN: u64 = 1 << 30;

/// A `/loop` wakeup this long past due that's still pending never fired,
/// and won't: it's left out, rather than keep its agent awake for good.
pub const STALE_LOOP: Duration = Duration::from_secs(10 * 60);

/// The shortest stretch CPU is measured over: the kernel counts it in
/// ticks, so a shorter one is mostly noise.
const CPU_WINDOW: Duration = Duration::from_secs(1);

/// Which idle agents may sleep early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Early {
    /// Any: to make room under `max_awake_agents`.
    Any,
    /// One whose sleep frees memory, or a supervised one, which its OS can
    /// swap out once frozen: under memory pressure.
    UnderPressure,
}

impl Early {
    fn allows(self, caps: vibe_env::Capabilities) -> bool {
        match self {
            Early::Any => true,
            Early::UnderPressure => caps.sleep_frees_memory || !caps.kernel_limits,
        }
    }
}

/// Why an automatic sleep was called off at the last moment: news to no
/// one, and tried again on the next look.
pub(crate) const TYPING: &str = "a send is typing into it";
pub(crate) const ATTACHED: &str = "a client attached";

/// How the watch is timed. Tests shorten it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub every: Duration,
    pub wake_before: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            every: SLEEP_EVERY,
            wake_before: WAKE_BEFORE_DUE,
        }
    }
}

/// Why an agent stays awake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Awake {
    /// Its transcript says it's at work, or hasn't said yet.
    Working,
    /// What may be a permission prompt, but it isn't yet quiet for long
    /// enough, or something runs, or its CPU isn't near zero.
    MaybeWorking,
    /// A cron job, or a wakeup that's inferred or has no due time.
    Wakeup,
    /// Its `/loop` wakeup is due before a wake would come for it.
    LoopDueSoon,
    /// A process in its tree that its transcript can't account for.
    Unseen {
        pid: u32,
        name: String,
    },
    Attached(usize),
    /// Its processes couldn't be listed.
    Unknown(String),
}

impl fmt::Display for Awake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Awake::Working => f.write_str("it's at work"),
            Awake::MaybeWorking => f.write_str("it may be at work"),
            Awake::Wakeup => f.write_str("it has a scheduled job"),
            Awake::LoopDueSoon => f.write_str("its /loop wakeup is due soon"),
            Awake::Unseen { pid, name } => {
                write!(f, "it left {name} (pid {pid}) running")
            }
            Awake::Attached(1) => f.write_str("a client is attached"),
            Awake::Attached(n) => write!(f, "{n} clients are attached"),
            Awake::Unknown(why) => write!(f, "its processes can't be listed: {why}"),
        }
    }
}

/// Whether an agent is idle but for the debounce.
#[derive(Debug, Clone, PartialEq)]
pub enum Rest {
    /// Idle, at rest since `since` (seconds since the epoch).
    Idle {
        since: f64,
    },
    Awake(Awake),
}

/// What the idle rule looks at.
#[derive(Debug, Clone, Copy)]
pub struct Facts<'a> {
    pub status: &'a Status,
    /// Every process of the agent but its shim.
    pub procs: &'a [Proc],
    /// Clients attached to its terminal.
    pub attached: usize,
    /// The share of one core its tree used lately, once measured.
    pub cpu_share: Option<f64>,
    /// Seconds since the epoch.
    pub now: f64,
    pub wake_before: Duration,
}

/// The idle rule, but for the debounce (PRD: Going to sleep).
pub fn rest(f: &Facts<'_>) -> Rest {
    let s = f.status;
    let Some(since) = s.quiet_since.map(Timestamp::as_secs_f64) else {
        return Rest::Awake(Awake::Working);
    };
    let prompt = s.activity == Activity::AwaitingApproval;
    // A firm permission prompt is a question like any other; Claude records
    // its own only as inferred ones.
    if !s.at_rest() && !(prompt && s.certainty == Certainty::Firm) {
        if !prompt {
            return Rest::Awake(Awake::Working);
        }
        let quiet = s.settled(Timestamp::from_secs_f64(f.now));
        let began = s.since.map_or(since, Timestamp::as_secs_f64);
        let calm = f.cpu_share.is_some_and(|c| c < QUIET_CPU);
        if !quiet || started_since(f.procs, began) || !calm {
            return Rest::Awake(Awake::MaybeWorking);
        }
    }
    for w in &s.pending_wakeups {
        match (w.kind, w.certainty, w.due_at) {
            (WakeupKind::Loop, Certainty::Firm, Some(due))
                if due.as_secs_f64() < f.now - STALE_LOOP.as_secs_f64() => {}
            (WakeupKind::Loop, Certainty::Firm, Some(due)) => {
                if due.as_secs_f64() - f.now <= f.wake_before.as_secs_f64() {
                    return Rest::Awake(Awake::LoopDueSoon);
                }
            }
            _ => return Rest::Awake(Awake::Wakeup),
        }
    }
    if let Some(p) = unseen(f.procs) {
        return Rest::Awake(Awake::Unseen {
            pid: p.pid,
            name: p.name.clone(),
        });
    }
    if f.attached > 0 {
        return Rest::Awake(Awake::Attached(f.attached));
    }
    Rest::Idle { since }
}

/// When the first firm `/loop` wakeup an agent has armed is due, in
/// seconds since the epoch, as of `now`; one [`STALE_LOOP`] past due is
/// left out.
pub fn next_loop(status: &Status, now: f64) -> Option<f64> {
    status
        .pending_wakeups
        .iter()
        .filter(|w| w.kind == WakeupKind::Loop && w.certainty == Certainty::Firm)
        .filter_map(|w| w.due_at.map(Timestamp::as_secs_f64))
        .filter(|due| *due >= now - STALE_LOOP.as_secs_f64())
        .reduce(f64::min)
}

/// Claude, in its tree: the earliest of the processes whose parent isn't
/// in it. Anything reparented away started later.
fn claude(procs: &[Proc]) -> Option<&Proc> {
    let pids: std::collections::HashSet<u32> = procs.iter().map(|p| p.pid).collect();
    procs
        .iter()
        .filter(|p| !pids.contains(&p.ppid))
        .min_by_key(|p| (p.started_ms, p.pid))
}

/// A process in the tree that agent-tail can't see: one started more than
/// [`UNSEEN_AFTER`] after Claude, none of whose ancestors short of Claude
/// started before that (the MCP servers Claude starts, and their children).
pub fn unseen(procs: &[Proc]) -> Option<&Proc> {
    let claude = claude(procs)?;
    let by_pid: HashMap<u32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let grace = claude
        .started_ms
        .saturating_add(UNSEEN_AFTER.as_millis() as u64);
    procs.iter().filter(|p| p.pid != claude.pid).find(|p| {
        let mut q = *p;
        // Bounded, in case pids loop on a racy listing.
        for _ in 0..=procs.len() {
            if q.started_ms <= grace {
                return false;
            }
            match by_pid.get(&q.ppid) {
                Some(parent) if parent.pid != claude.pid => q = parent,
                _ => break,
            }
        }
        true
    })
}

/// Whether anything but Claude started at or after `since`, seconds since
/// the epoch: a tool run, for one.
fn started_since(procs: &[Proc], since: f64) -> bool {
    let Some(claude) = claude(procs) else {
        return false;
    };
    let from = ((since - 1.0).max(0.0) * 1000.0) as u64;
    procs
        .iter()
        .any(|p| p.pid != claude.pid && p.started_ms >= from)
}

/// The free space on the filesystem `path` is on, or would be on: the
/// nearest directory above it that exists says.
pub fn free_space(path: &Path) -> Option<u64> {
    let dir = path.ancestors().find(|d| d.is_dir())?;
    let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: a valid path and a struct for statvfs to fill.
    unsafe {
        let mut st = std::mem::zeroed::<libc::statvfs>();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        #[allow(clippy::useless_conversion)]
        Some(u64::from(st.f_bavail).saturating_mul(u64::from(st.f_frsize)))
    }
}

/// What the watch keeps of each agent between looks.
#[derive(Debug, Default)]
pub(crate) struct Watch {
    agents: HashMap<String, Watched>,
}

#[derive(Debug, Default)]
struct Watched {
    /// When this `vibed` first saw it idle, this time.
    idle_since: Option<i64>,
    /// Its tree's CPU time when last measured, and when.
    cpu: Option<(u64, Instant)>,
    cpu_share: Option<f64>,
    /// When it last woke.
    woke: Option<Instant>,
    /// A sleep or wake that failed, when and why.
    failed: Option<(Instant, String)>,
}

impl Watch {
    /// That `agent_ref` just woke: it's left awake a moment, and its
    /// debounce starts again.
    pub(crate) fn woke(&mut self, agent_ref: &str) {
        let w = self.agents.entry(agent_ref.to_owned()).or_default();
        w.woke = Some(Instant::now());
        w.idle_since = None;
        w.failed = None;
    }

    fn just_woke(&self, agent_ref: &str) -> bool {
        self.agents
            .get(agent_ref)
            .and_then(|w| w.woke)
            .is_some_and(|t| t.elapsed() < AFTER_WAKE)
    }

    fn failed_lately(&self, agent_ref: &str) -> bool {
        self.agents
            .get(agent_ref)
            .and_then(|w| w.failed.as_ref())
            .is_some_and(|(t, _)| t.elapsed() < RETRY_AFTER)
    }

    /// Why its last sleep or wake failed, if it did lately.
    fn failed_why(&self, agent_ref: &str) -> Option<String> {
        self.agents
            .get(agent_ref)
            .and_then(|w| w.failed.as_ref())
            .filter(|(t, _)| t.elapsed() < RETRY_AFTER)
            .map(|(_, why)| why.clone())
    }

    /// Notes a failure; true if it's news, to be logged.
    fn failure(&mut self, agent_ref: &str, why: String) -> bool {
        let w = self.agents.entry(agent_ref.to_owned()).or_default();
        let news = w.failed.as_ref().is_none_or(|(_, was)| *was != why);
        w.failed = Some((Instant::now(), why));
        news
    }
}

/// What one look at the agents did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Looked {
    /// Agents it put to sleep.
    pub slept: Vec<String>,
    /// Agents it woke: for their `/loop`, or a client that attached.
    pub woke: Vec<String>,
    /// How many agents are awake now.
    pub awake: usize,
}

/// What an asleep agent's look found.
enum Asleep {
    Woke,
    /// A `/loop` is pending for later: the host must stay up for it.
    LoopPending,
    Resting,
}

impl Core {
    /// Why the watch couldn't put `agent_ref` to sleep, or wake it, lately.
    pub fn sleep_refusal(&self, agent_ref: &str) -> Option<String> {
        lock(&self.sleep_watch).failed_why(agent_ref)
    }

    /// Whether a send is typing into `agent_ref` now.
    fn typing_into(&self, agent_ref: &str) -> bool {
        lock(&self.typing)
            .get(agent_ref)
            .is_some_and(|m| m.try_lock().is_err())
    }

    /// The idle rule on `a` now, but for the debounce. Its CPU is measured
    /// on the way, for the next look.
    async fn rest_of(&self, a: &mut AgentRecord, backend: &Arc<dyn Backend>, h: &Handle) -> Rest {
        let status = self.agent_status(a, self.idle_debounce()).await;
        let procs = {
            let (b, h) = (Arc::clone(backend), h.clone());
            match blocking(move || b.processes(&h)).await {
                Ok(p) => p,
                Err(e) => return Rest::Awake(Awake::Unknown(e.message)),
            }
        };
        let attached = match sessions::probe(&self.layout.session_socket(&a.agent_ref)).await {
            Probe::Running(s) => s.clients.len(),
            _ => 0,
        };
        let cpu: u64 = procs.iter().map(|p| p.cpu_ms).sum();
        let cpu_share = {
            let mut watch = lock(&self.sleep_watch);
            let w = watch.agents.entry(a.agent_ref.clone()).or_default();
            match w.cpu {
                Some((was, at)) if at.elapsed() >= CPU_WINDOW => {
                    let wall = at.elapsed().as_millis() as f64;
                    // A process that ended takes its CPU time with it.
                    w.cpu_share = (cpu >= was).then(|| (cpu - was) as f64 / wall);
                    w.cpu = Some((cpu, Instant::now()));
                }
                Some(_) => {}
                None => w.cpu = Some((cpu, Instant::now())),
            }
            w.cpu_share
        };
        rest(&Facts {
            status: &status,
            procs: &procs,
            attached,
            cpu_share,
            now: self.now() as f64,
            wake_before: self.sleep_timing.wake_before,
        })
    }

    /// Looks at every agent once: puts the idle ones whose debounce has
    /// passed to sleep, wakes the asleep ones whose `/loop` is due or that a
    /// client attached to, and holds the Mac awake while any agent is, or
    /// has a `/loop` to wake for.
    pub async fn check_sleep(&self) -> Looked {
        let mut looked = Looked::default();
        let Ok(records) = self.store().agents(LOCAL_HOST) else {
            return looked;
        };
        lock(&self.sleep_watch)
            .agents
            .retain(|r, _| records.iter().any(|a| &a.agent_ref == r));
        let debounce = self.idle_debounce().as_secs() as i64;
        let toolbox = *lock(&self.toolbox_state);
        let toolbox_awake = !matches!(
            toolbox,
            vibe_core::model::ToolboxState::Closed | vibe_core::model::ToolboxState::Unavailable
        );
        let mut loops = false;
        for mut a in records {
            let r = a.agent_ref.clone();
            if a.starting || self.is_busy(&r) {
                looked.awake += 1;
                continue;
            }
            match a.state {
                AgentState::Asleep => {
                    match self.look_asleep(&mut a).await {
                        Asleep::Woke => {
                            looked.woke.push(r);
                            looked.awake += 1;
                        }
                        Asleep::LoopPending => loops = true,
                        Asleep::Resting => {}
                    }
                    continue;
                }
                AgentState::Exited => continue,
                // Held by a limit or by memory pressure; a wake is the
                // owner's, or the pressure's to lift.
                AgentState::Paused => {
                    looked.awake += 1;
                    continue;
                }
                _ => {}
            }
            let (Ok(backend), Ok(handle)) = (self.backend(a.environment), Core::handle_of(&a))
            else {
                continue;
            };
            if !self.session_runs(&r).await {
                continue;
            }
            looked.awake += 1;
            let now = self.now();
            let since = match self.rest_of(&mut a, &backend, &handle).await {
                Rest::Idle { since } => since.floor() as i64,
                Rest::Awake(_) => {
                    if let Some(w) = lock(&self.sleep_watch).agents.get_mut(&r) {
                        w.idle_since = None;
                    }
                    continue;
                }
            };
            {
                let mut watch = lock(&self.sleep_watch);
                if watch.just_woke(&r) || watch.failed_lately(&r) {
                    continue;
                }
                let w = watch.agents.entry(r.clone()).or_default();
                let seen = *w.idle_since.get_or_insert(now);
                if now - since.max(seen) < debounce {
                    continue;
                }
            }
            if self.typing_into(&r) {
                continue;
            }
            let why = format!("idle for {debounce}s, nothing attached");
            match self.sleep_for(Some(LOCAL_HOST), &r, &why, true).await {
                Ok(_) => {
                    looked.slept.push(r);
                    looked.awake -= 1;
                }
                // Called off at the last moment: it's simply not idle now.
                Err(e) if e.message.ends_with(TYPING) || e.message.ends_with(ATTACHED) => {}
                Err(e) => {
                    if lock(&self.sleep_watch).failure(&r, e.message.clone()) {
                        crate::log!("vibed: {r} can't sleep: {}", e.message);
                    }
                }
            }
        }
        self.power
            .keep_awake(looked.awake > 0 || loops || toolbox_awake);
        looked
    }

    /// Wakes `a`, asleep, if a client has attached to its terminal (an
    /// attach that raced its sleep), or if its firm `/loop` wakeup is due
    /// within the lead a wake gets.
    async fn look_asleep(&self, a: &mut AgentRecord) -> Asleep {
        let r = a.agent_ref.clone();
        if let Probe::Running(s) = sessions::probe(&self.layout.session_socket(&r)).await
            && !s.clients.is_empty()
            && !lock(&self.sleep_watch).failed_lately(&r)
        {
            match self.resume_agent(Some(LOCAL_HOST), &r).await {
                Ok(_) => {
                    crate::log!("vibed: {r} was asleep with a client attached, so it woke");
                    return Asleep::Woke;
                }
                Err(e) => {
                    if lock(&self.sleep_watch).failure(&r, e.message.clone()) {
                        crate::log!(
                            "vibed: {r} has a client attached, but can't wake: {}",
                            e.message
                        );
                    }
                }
            }
        }
        if self.wake_if_due(a).await {
            return Asleep::Woke;
        }
        let status = self.agent_status(a, self.idle_debounce()).await;
        if next_loop(&status, self.now() as f64).is_some() {
            Asleep::LoopPending
        } else {
            Asleep::Resting
        }
    }

    /// Wakes `a`, asleep, if its firm `/loop` wakeup is due within the
    /// lead a wake gets. Says whether it did.
    async fn wake_if_due(&self, a: &mut AgentRecord) -> bool {
        let r = a.agent_ref.clone();
        let status = self.agent_status(a, self.idle_debounce()).await;
        let Some(due) = next_loop(&status, self.now() as f64) else {
            return false;
        };
        let lead = self.sleep_timing.wake_before.as_secs_f64();
        if due - self.now() as f64 > lead || lock(&self.sleep_watch).failed_lately(&r) {
            return false;
        }
        match self.resume_agent(Some(LOCAL_HOST), &r).await {
            Ok(_) => {
                crate::log!(
                    "vibed: woke {r}: its /loop wakeup is due at {}",
                    vibe_core::time::rfc3339(due.floor() as i64)
                );
                true
            }
            Err(e) => {
                if lock(&self.sleep_watch).failure(&r, e.message.clone()) {
                    crate::log!(
                        "vibed: {r}'s /loop is due, but it can't wake: {}",
                        e.message
                    );
                }
                false
            }
        }
    }

    /// The agents that are idle now but for the debounce, idle longest
    /// first, each with what its backend can do.
    pub(crate) async fn idle_now(&self) -> Vec<(String, vibe_env::Capabilities)> {
        let Ok(records) = self.store().agents(LOCAL_HOST) else {
            return Vec::new();
        };
        let mut idle = Vec::new();
        for mut a in records {
            let r = a.agent_ref.clone();
            if a.starting
                || self.is_busy(&r)
                || matches!(
                    a.state,
                    AgentState::Asleep | AgentState::Paused | AgentState::Exited
                )
                || lock(&self.sleep_watch).just_woke(&r)
                || self.typing_into(&r)
            {
                continue;
            }
            let (Ok(backend), Ok(handle)) = (self.backend(a.environment), Core::handle_of(&a))
            else {
                continue;
            };
            if !self.session_runs(&r).await {
                continue;
            }
            if let Rest::Idle { since } = self.rest_of(&mut a, &backend, &handle).await {
                idle.push((since, r, backend.capabilities()));
            }
        }
        idle.sort_by(|x, y| x.0.total_cmp(&y.0).then_with(|| x.1.cmp(&y.1)));
        idle.into_iter().map(|(_, r, caps)| (r, caps)).collect()
    }

    /// Puts the agent idle longest that `early` allows to sleep before its
    /// debounce is up. Returns the one it put to sleep.
    pub(crate) async fn sleep_one_early(&self, early: Early, why: &str) -> Option<String> {
        for (r, caps) in self.idle_now().await {
            if !early.allows(caps) {
                continue;
            }
            match self.sleep_for(Some(LOCAL_HOST), &r, why, true).await {
                Ok(_) => return Some(r),
                Err(e) => crate::log!("vibed: {r} can't sleep early: {}", e.message),
            }
        }
        None
    }

    /// Makes room for `r`'s snapshot, if its backend sleeps to disk, before
    /// it's saved: it must fit on the disk, with [`SNAPSHOT_MARGIN`] to
    /// spare, and in the `snapshot_disk` budget all snapshots share, where
    /// the oldest are dropped to make room (PRD: Sleep and wake; Failures,
    /// Low disk). An agent whose snapshot can't fit stays awake.
    pub(crate) async fn make_room_for_sleep(
        &self,
        r: &str,
        backend: &Arc<dyn Backend>,
        h: &Handle,
    ) -> Result<(), String> {
        let need = {
            let (b, h) = (Arc::clone(backend), h.clone());
            match blocking(move || b.sleep_size(&h)).await {
                Ok(Some(n)) => n,
                Ok(None) => return Ok(()),
                Err(e) => return Err(e.message),
            }
        };
        if let Some(free) = (self.free_disk)(&self.layout.snapshots_dir())
            && need.saturating_add(SNAPSHOT_MARGIN) > free
        {
            return Err(format!(
                "it stays awake: its snapshot would take about {}, and the disk has only {} free",
                size(need),
                size(free)
            ));
        }
        let budget = vibe_core::limits::load(&self.layout.host_config())
            .unwrap_or_default()
            .snapshot_disk;
        if need > budget {
            return Err(format!(
                "it stays awake: its snapshot would take about {}, more than all snapshots may \
                 (snapshot_disk is {})",
                size(need),
                size(budget)
            ));
        }
        // Every other snapshot, oldest first.
        let mut saved = Vec::new();
        let agents = self.store().agents(LOCAL_HOST).unwrap_or_default();
        for a in agents {
            if a.agent_ref == r || a.state != AgentState::Asleep {
                continue;
            }
            let (Ok(b), Ok(h)) = (self.backend(a.environment), Core::handle_of(&a)) else {
                continue;
            };
            if let Some(s) = b.saved(&h) {
                saved.push((s, a.agent_ref, b, h));
            }
        }
        saved.sort_by(|x, y| (x.0.at, &x.1).cmp(&(y.0.at, &y.1)));
        let mut total: u64 = saved.iter().map(|s| s.0.bytes).sum();
        for (s, other, b, h) in saved {
            if total.saturating_add(need) <= budget {
                break;
            }
            // One being woken now is left alone.
            let Ok(_held) = self.hold(&other) else {
                continue;
            };
            match blocking(move || b.drop_saved(&h)).await {
                Ok(()) => {
                    total -= s.bytes;
                    crate::log!(
                        "vibed: dropped {other}'s snapshot ({}) to make room for {r}'s within \
                         snapshot_disk ({}); {other} wakes by resuming its session",
                        size(s.bytes),
                        size(budget)
                    );
                }
                Err(e) => crate::log!("vibed: can't drop {other}'s snapshot: {}", e.message),
            }
        }
        if total.saturating_add(need) > budget {
            return Err(format!(
                "it stays awake: its snapshot would take about {}, and the snapshots of agents \
                 asleep already fill snapshot_disk ({})",
                size(need),
                size(budget)
            ));
        }
        Ok(())
    }

    /// Runs [`Core::check_sleep`] every `every`.
    pub fn spawn_sleep_watch(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let core = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(core.sleep_timing.every);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticks.tick().await;
                core.check_sleep().await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vibe_status::{INFERRED_QUIET, Wakeup};

    const NOW: f64 = 1_793_361_600.0;

    fn proc_(pid: u32, ppid: u32, started_s: f64) -> Proc {
        Proc {
            pid,
            ppid,
            name: format!("p{pid}"),
            args: Vec::new(),
            stopped: false,
            started_ms: (started_s * 1000.0) as u64,
            cpu_ms: 0,
            memory: 0,
        }
    }

    fn status(activity: Activity, certainty: Certainty, quiet_for: f64) -> Status {
        Status {
            activity,
            certainty,
            since: Some(Timestamp::from_secs_f64(NOW - quiet_for)),
            last_write: Some(Timestamp::from_secs_f64(NOW - quiet_for)),
            quiet_since: Some(Timestamp::from_secs_f64(NOW - quiet_for)),
            ..Status::unknown()
        }
    }

    fn done() -> Status {
        status(Activity::Done, Certainty::Firm, 10.0)
    }

    fn wakeup(kind: WakeupKind, certainty: Certainty, due_in: Option<f64>) -> Wakeup {
        Wakeup {
            kind,
            due_at: due_in.map(|d| Timestamp::from_secs_f64(NOW + d)),
            certainty,
            recurring: kind == WakeupKind::Cron,
            reason: None,
        }
    }

    fn facts<'a>(s: &'a Status, procs: &'a [Proc]) -> Facts<'a> {
        Facts {
            status: s,
            procs,
            attached: 0,
            cpu_share: Some(0.0),
            now: NOW,
            wake_before: WAKE_BEFORE_DUE,
        }
    }

    /// Claude, started an hour ago, with an MCP server it started then.
    fn claude_and_mcp() -> Vec<Proc> {
        vec![proc_(10, 1, NOW - 3600.0), proc_(11, 10, NOW - 3595.0)]
    }

    #[test]
    fn firm_rest_is_idle_from_its_last_write() {
        let procs = claude_and_mcp();
        for activity in [
            Activity::Done,
            Activity::Interrupted,
            Activity::Errored,
            Activity::AwaitingAnswer,
        ] {
            let s = status(activity, Certainty::Firm, 10.0);
            assert_eq!(
                rest(&facts(&s, &procs)),
                Rest::Idle { since: NOW - 10.0 },
                "{activity:?}"
            );
        }
        // A usage limit is a firm errored: it sleeps like any idle agent.
        let mut limited = status(Activity::Errored, Certainty::Firm, 10.0);
        limited.stop_reason = Some(vibe_core::model::StopReason::UsageLimit);
        assert!(matches!(rest(&facts(&limited, &procs)), Rest::Idle { .. }));
    }

    #[test]
    fn work_never_sleeps() {
        let procs = claude_and_mcp();
        for activity in [
            Activity::Thinking,
            Activity::RunningTool,
            Activity::BackgroundWork,
            Activity::Unknown,
        ] {
            for certainty in [Certainty::Firm, Certainty::Inferred] {
                let s = status(activity, certainty, 3600.0);
                assert_eq!(
                    rest(&facts(&s, &procs)),
                    Rest::Awake(Awake::Working),
                    "{activity:?} {certainty:?}"
                );
            }
        }
        // Inferred rest is no rest; nor is nothing read at all.
        let s = status(Activity::Done, Certainty::Inferred, 3600.0);
        assert_eq!(rest(&facts(&s, &procs)), Rest::Awake(Awake::Working));
        assert_eq!(
            rest(&facts(&Status::unknown(), &procs)),
            Rest::Awake(Awake::Working)
        );
    }

    #[test]
    fn a_permission_prompt_sleeps_only_once_long_quiet_and_calm() {
        let procs = claude_and_mcp();
        let quiet = INFERRED_QUIET.as_secs_f64();
        let prompt = |for_s: f64| status(Activity::AwaitingApproval, Certainty::Inferred, for_s);
        // Too soon: a silent tool run looks the same.
        let s = prompt(quiet - 1.0);
        assert_eq!(rest(&facts(&s, &procs)), Rest::Awake(Awake::MaybeWorking));
        let s = prompt(quiet + 1.0);
        assert_eq!(
            rest(&facts(&s, &procs)),
            Rest::Idle {
                since: NOW - quiet - 1.0
            }
        );
        // Its CPU busy, or not measured yet.
        let busy = Facts {
            cpu_share: Some(0.5),
            ..facts(&s, &procs)
        };
        assert_eq!(rest(&busy), Rest::Awake(Awake::MaybeWorking));
        let unmeasured = Facts {
            cpu_share: None,
            ..facts(&s, &procs)
        };
        assert_eq!(rest(&unmeasured), Rest::Awake(Awake::MaybeWorking));
        // A process started since it began: a tool, running silently.
        let mut running = claude_and_mcp();
        running.push(proc_(12, 10, NOW - quiet));
        assert_eq!(rest(&facts(&s, &running)), Rest::Awake(Awake::MaybeWorking));
        // A firm prompt is a question like any other.
        let s = status(Activity::AwaitingApproval, Certainty::Firm, 10.0);
        assert!(matches!(rest(&facts(&s, &procs)), Rest::Idle { .. }));
    }

    #[test]
    fn only_firm_loops_due_after_their_wake_let_it_sleep() {
        let procs = claude_and_mcp();
        let with = |w: Vec<Wakeup>| Status {
            pending_wakeups: w,
            ..done()
        };
        let lead = WAKE_BEFORE_DUE.as_secs_f64();
        let s = with(vec![wakeup(WakeupKind::Loop, Certainty::Firm, Some(600.0))]);
        assert!(matches!(rest(&facts(&s, &procs)), Rest::Idle { .. }));
        assert_eq!(next_loop(&s, NOW), Some(NOW + 600.0));
        // Due within the lead: the wake would come at once.
        let s = with(vec![
            wakeup(WakeupKind::Loop, Certainty::Firm, Some(600.0)),
            wakeup(WakeupKind::Loop, Certainty::Firm, Some(lead)),
        ]);
        assert_eq!(rest(&facts(&s, &procs)), Rest::Awake(Awake::LoopDueSoon));
        assert_eq!(next_loop(&s, NOW), Some(NOW + lead));
        for w in [
            wakeup(WakeupKind::Loop, Certainty::Inferred, Some(600.0)),
            wakeup(WakeupKind::Loop, Certainty::Firm, None),
            wakeup(WakeupKind::Cron, Certainty::Firm, Some(600.0)),
            wakeup(WakeupKind::Cron, Certainty::Firm, None),
        ] {
            let s = with(vec![
                wakeup(WakeupKind::Loop, Certainty::Firm, Some(600.0)),
                w.clone(),
            ]);
            assert_eq!(
                rest(&facts(&s, &procs)),
                Rest::Awake(Awake::Wakeup),
                "{w:?}"
            );
        }
        // One long past due never fired: it's left out, so it neither keeps
        // the agent awake nor wakes it.
        let stale = -(STALE_LOOP.as_secs_f64() + 1.0);
        let s = with(vec![wakeup(WakeupKind::Loop, Certainty::Firm, Some(stale))]);
        assert!(matches!(rest(&facts(&s, &procs)), Rest::Idle { .. }));
        assert_eq!(next_loop(&s, NOW), None);
        // One just past due is due: woken at once, never slept on.
        let s = with(vec![wakeup(WakeupKind::Loop, Certainty::Firm, Some(-5.0))]);
        assert_eq!(rest(&facts(&s, &procs)), Rest::Awake(Awake::LoopDueSoon));
        assert_eq!(next_loop(&s, NOW), Some(NOW - 5.0));
        // An inferred loop isn't one to wake for.
        let s = with(vec![wakeup(
            WakeupKind::Loop,
            Certainty::Inferred,
            Some(5.0),
        )]);
        assert_eq!(next_loop(&s, NOW), None);
    }

    #[test]
    fn a_process_started_late_keeps_it_awake_unless_an_mcp_server_started_it() {
        let s = done();
        let mut procs = claude_and_mcp();
        // The MCP server's own child, started much later: still its.
        procs.push(proc_(12, 11, NOW - 60.0));
        // Something started within 30 s of Claude, and its child.
        procs.push(proc_(13, 10, NOW - 3575.0));
        procs.push(proc_(14, 13, NOW - 30.0));
        assert!(matches!(rest(&facts(&s, &procs)), Rest::Idle { .. }));
        assert_eq!(unseen(&procs), None);
        // A nohup job: reparented away from the tree, started late.
        let mut nohup = procs.clone();
        nohup.push(proc_(20, 1, NOW - 100.0));
        assert_eq!(
            rest(&facts(&s, &nohup)),
            Rest::Awake(Awake::Unseen {
                pid: 20,
                name: "p20".into()
            })
        );
        // Started late by Claude itself, as a tool's shell is, and what it
        // started.
        let mut late = procs.clone();
        late.push(proc_(30, 10, NOW - 100.0));
        late.push(proc_(31, 30, NOW - 99.0));
        assert_eq!(unseen(&late).map(|p| p.pid), Some(30));
        // No processes at all, or Claude alone: nothing unseen.
        assert_eq!(unseen(&[]), None);
        assert_eq!(unseen(&procs[..1]), None);
        // A listing with a loop in it doesn't hang.
        let looped = vec![
            proc_(10, 1, NOW - 3600.0),
            proc_(40, 41, NOW - 10.0),
            proc_(41, 40, NOW - 10.0),
        ];
        assert!(unseen(&looped).is_some());
    }

    #[test]
    fn something_attached_keeps_it_awake() {
        let procs = claude_and_mcp();
        let s = done();
        let f = Facts {
            attached: 2,
            ..facts(&s, &procs)
        };
        assert_eq!(rest(&f), Rest::Awake(Awake::Attached(2)));
        assert_eq!(Awake::Attached(2).to_string(), "2 clients are attached");
        assert_eq!(
            Awake::Unseen {
                pid: 7,
                name: "node".into()
            }
            .to_string(),
            "it left node (pid 7) running"
        );
    }

    #[test]
    fn free_space_is_asked_of_the_nearest_directory_there_is() {
        let tmp = tempfile::tempdir().unwrap();
        let free = free_space(&tmp.path().join("no/such/dir")).unwrap();
        assert!(free > 0);
        assert!(free_space(Path::new("")).is_none());
    }

    #[test]
    fn a_wake_is_left_a_moment_and_a_failure_said_once() {
        let mut w = Watch::default();
        assert!(!w.just_woke("a"));
        w.woke("a");
        assert!(w.just_woke("a"));
        assert!(w.failure("a", "full".into()));
        assert!(!w.failure("a", "full".into()));
        assert!(w.failure("a", "fuller".into()));
        assert!(w.failed_lately("a"));
        w.woke("a");
        assert!(!w.failed_lately("a"));
    }
}
