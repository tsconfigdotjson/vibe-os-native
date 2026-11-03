//! Critical memory pressure (PRD: Failures): put idle agents to sleep
//! early, and then, while it lasts:
//!
//! - on a backend whose limits a supervisor holds (`kernel_limits` false,
//!   as `macos`'s are), pause the agent furthest over 80% of its memory,
//!   and if the pressure lasts [`KILL_AFTER`] more, end it, resumable. One
//!   paused for pressure is let go once it eases;
//! - on one whose limits the kernel holds and whose sleep keeps its memory
//!   (a Linux host's cgroups: PRD, Memory pressure on a Linux host), end
//!   frozen agents, oldest first. Each cgroup's own limit already stops a
//!   single agent from taking the host; this is for many agents that fit
//!   their limits together outgrowing it.
//!
//! Pressure is the kernel's own measure. On Linux, its pressure stall
//! information for memory (`/proc/pressure/memory`): the share of the last
//! 10 s in which some task waited on memory (`some`), or every task did
//! (`full`), past [`HIGH_SOME`] or [`HIGH_FULL`]. On macOS, the level
//! `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE` reports, at critical. Once it has
//! lasted [`END_AFTER`], an agent is paused or ended, then one more every
//! [`END_EVERY`] while it lasts. Frozen is asleep, or paused by a limit, on
//! a backend whose sleep keeps its memory (`sleep_frees_memory` false, so
//! ending it gives memory back). An ended agent is `asleep`; a send or an
//! attach resumes it with `claude --resume`, which loses a pending question
//! (PRD: Going to sleep).
//!
//! Where the kernel doesn't say (a Linux without PSI), there's no pressure
//! here.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use vibe_core::model::{AgentState, LOCAL_HOST};
use vibe_env::{Limit, Limits};

use crate::agents::{blocking, size};
use crate::core::{Core, lock};

/// How often pressure is read.
pub const PRESSURE_EVERY: Duration = Duration::from_secs(2);

/// Past this share of the last 10 s with some task waiting on memory, the
/// host is short of it.
pub const HIGH_SOME: f64 = 30.0;

/// Past this share with every task waiting on memory, likewise.
pub const HIGH_FULL: f64 = 10.0;

/// How long it may stay short before an agent is paused or ended.
pub const END_AFTER: Duration = Duration::from_secs(10);

/// How long after ending one the next may be, while it's still short: the
/// kernel's 10 s averages take that long to show the memory came back.
pub const END_EVERY: Duration = Duration::from_secs(15);

/// How long an agent paused for pressure is held before it's ended, if
/// the pressure lasts.
pub const KILL_AFTER: Duration = Duration::from_secs(5 * 60);

/// The share of its memory past which a supervised agent may be paused.
pub const PAUSE_PAST: f64 = 0.8;

/// What the kernel says of memory pressure.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Psi {
    /// The share of the last 10 s, in percent, that some task waited.
    pub some10: f64,
    /// The share in which every task did.
    pub full10: f64,
}

impl Psi {
    /// From `/proc/pressure/memory`.
    pub fn parse(text: &str) -> Option<Psi> {
        let avg10 = |kind: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(kind)?.strip_prefix(' '))?
                .split_whitespace()
                .find_map(|f| f.strip_prefix("avg10=")?.parse::<f64>().ok())
        };
        Some(Psi {
            some10: avg10("some")?,
            // Kernels before 5.13 have no `full` line for the whole system.
            full10: avg10("full").unwrap_or(0.0),
        })
    }

    /// Whether the host is short of memory.
    pub fn high(&self) -> bool {
        self.some10 >= HIGH_SOME || self.full10 >= HIGH_FULL
    }
}

impl fmt::Display for Psi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "its programs waited on memory {:.0}% of the last 10 s",
            self.some10.max(self.full10)
        )
    }
}

/// macOS's memory pressure level, as its dispatch source reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Normal,
    Warn,
    Critical,
}

/// What the kernel says of memory pressure.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reading {
    /// Linux's pressure stall information.
    Psi(Psi),
    /// macOS's level.
    Level(Level),
}

impl Reading {
    /// Whether the host is critically short of memory.
    pub fn high(&self) -> bool {
        match self {
            Reading::Psi(p) => p.high(),
            Reading::Level(l) => *l == Level::Critical,
        }
    }
}

impl From<Psi> for Reading {
    fn from(p: Psi) -> Reading {
        Reading::Psi(p)
    }
}

impl From<Level> for Reading {
    fn from(l: Level) -> Reading {
        Reading::Level(l)
    }
}

impl fmt::Display for Reading {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reading::Psi(p) => p.fmt(f),
            Reading::Level(Level::Critical) => {
                f.write_str("macOS says memory pressure is critical")
            }
            Reading::Level(Level::Warn) => f.write_str("macOS warns of memory pressure"),
            Reading::Level(Level::Normal) => f.write_str("its memory pressure is normal"),
        }
    }
}

/// Where memory pressure comes from.
pub trait MemoryPressure: Send + Sync + fmt::Debug {
    /// The pressure now, or `None` where the kernel doesn't say.
    fn read(&self) -> Option<Reading>;
}

/// This host's: `/proc/pressure/memory` on Linux, the memory pressure
/// dispatch source on macOS.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostPressure;

impl MemoryPressure for HostPressure {
    #[cfg(target_os = "macos")]
    fn read(&self) -> Option<Reading> {
        Some(Reading::Level(macos::level()))
    }

    #[cfg(not(target_os = "macos"))]
    fn read(&self) -> Option<Reading> {
        Psi::parse(&std::fs::read_to_string("/proc/pressure/memory").ok()?).map(Reading::Psi)
    }
}

/// No pressure, ever: for tests, and hosts that don't measure it.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPressure;

impl MemoryPressure for NoPressure {
    fn read(&self) -> Option<Reading> {
        None
    }
}

/// `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE`, watched from the first read for as
/// long as `vibed` runs. It reports each change of level; the level at the
/// start comes from the sysctl it mirrors.
#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::c_void;
    use std::sync::Once;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::Level;

    const NORMAL: usize = 0x1;
    const WARN: usize = 0x2;
    const CRITICAL: usize = 0x4;

    #[repr(C)]
    struct SourceType {
        _opaque: [u8; 0],
    }

    unsafe extern "C" {
        static _dispatch_source_type_memorypressure: SourceType;
        fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *mut c_void;
        fn dispatch_source_create(
            kind: *const SourceType,
            handle: usize,
            mask: usize,
            queue: *mut c_void,
        ) -> *mut c_void;
        fn dispatch_set_context(object: *mut c_void, context: *mut c_void);
        fn dispatch_source_set_event_handler_f(
            source: *mut c_void,
            handler: extern "C" fn(*mut c_void),
        );
        fn dispatch_source_get_data(source: *mut c_void) -> usize;
        fn dispatch_resume(object: *mut c_void);
    }

    static LEVEL: AtomicUsize = AtomicUsize::new(NORMAL);
    static WATCH: Once = Once::new();

    /// Called on a dispatch queue with the source as its context.
    extern "C" fn changed(source: *mut c_void) {
        // SAFETY: the context is the source itself, which is never freed.
        let level = unsafe { dispatch_source_get_data(source) };
        if level != 0 {
            LEVEL.store(level, Ordering::SeqCst);
        }
    }

    /// `kern.memorystatus_vm_pressure_level`: the same levels.
    fn sysctl_level() -> Option<usize> {
        let mut level: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>();
        // SAFETY: an int for sysctl to fill, and its size.
        let rc = unsafe {
            libc::sysctlbyname(
                c"kern.memorystatus_vm_pressure_level".as_ptr(),
                (&mut level as *mut libc::c_int).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0 && level > 0).then_some(level as usize)
    }

    pub(super) fn level() -> Level {
        WATCH.call_once(|| {
            if let Some(level) = sysctl_level() {
                LEVEL.store(level, Ordering::SeqCst);
            }
            // SAFETY: the dispatch calls as libdispatch documents them; the
            // source lives as long as the process.
            unsafe {
                let queue = dispatch_get_global_queue(0, 0);
                let source = dispatch_source_create(
                    &raw const _dispatch_source_type_memorypressure,
                    0,
                    NORMAL | WARN | CRITICAL,
                    queue,
                );
                if source.is_null() {
                    crate::log!("vibed: can't watch memory pressure: no dispatch source");
                    return;
                }
                dispatch_set_context(source, source);
                dispatch_source_set_event_handler_f(source, changed);
                dispatch_resume(source);
            }
        });
        let level = LEVEL.load(Ordering::SeqCst);
        if level & CRITICAL != 0 {
            Level::Critical
        } else if level & WARN != 0 {
            Level::Warn
        } else {
            Level::Normal
        }
    }
}

/// When each agent was frozen (asleep or paused), since this `vibed`
/// started, and in which order, as clocks may say the same second.
#[derive(Debug, Default)]
pub(crate) struct Sleeps {
    order: u64,
    since: HashMap<String, (i64, u64)>,
}

impl Sleeps {
    /// `agent_ref` is frozen from `now`, unless it was already.
    pub(crate) fn frozen(&mut self, agent_ref: &str, now: i64) {
        if !self.since.contains_key(agent_ref) {
            self.order += 1;
            self.since.insert(agent_ref.to_owned(), (now, self.order));
        }
    }

    pub(crate) fn running(&mut self, agent_ref: &str) {
        self.since.remove(agent_ref);
    }

    /// When it was frozen, and in which order; an agent frozen since before
    /// this `vibed` started isn't here.
    fn since(&self, agent_ref: &str) -> Option<(i64, u64)> {
        self.since.get(agent_ref).copied()
    }
}

/// Where the agents memory pressure paused are kept, in the state
/// directory, so a restarted `vibed` still knows they're its to let go.
pub const PAUSED_FILE: &str = "pressure-paused";

/// How long pressure has lasted, and when an agent was last paused or
/// ended for it.
#[derive(Debug, Default, Clone)]
pub struct PressureWatch {
    since: Option<Instant>,
    last_end: Option<Instant>,
}

impl Core {
    /// Notes that pressure paused `r` at `at`, and keeps the note.
    pub(crate) fn pressure_pause(&self, r: &str, at: Instant) {
        let mut paused = lock(&self.pressure_paused);
        paused.insert(r.to_owned(), at);
        self.keep_pressure_pauses(&paused);
    }

    /// Forgets that pressure paused `r`, if it did.
    pub(crate) fn pressure_unpause(&self, r: &str) {
        let mut paused = lock(&self.pressure_paused);
        if paused.remove(r).is_some() {
            self.keep_pressure_pauses(&paused);
        }
    }

    fn keep_pressure_pauses(&self, paused: &HashMap<String, Instant>) {
        let path = self.layout.data_dir().join(PAUSED_FILE);
        let mut refs: Vec<&str> = paused.keys().map(String::as_str).collect();
        refs.sort();
        let kept = if refs.is_empty() {
            match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        } else {
            std::fs::write(&path, refs.join("\n") + "\n")
        };
        if let Err(e) = kept {
            crate::log!("vibed: can't keep {}: {e}", path.display());
        }
    }

    /// The agents pressure paused before this `vibed` started, still
    /// paused: their grace before they're ended starts again now.
    pub(crate) fn load_pressure_pauses(&self) {
        let Ok(text) = std::fs::read_to_string(self.layout.data_dir().join(PAUSED_FILE)) else {
            return;
        };
        let agents = self.store().agents(LOCAL_HOST).unwrap_or_default();
        let mut paused = lock(&self.pressure_paused);
        for r in text.lines().map(str::trim).filter(|r| !r.is_empty()) {
            if agents
                .iter()
                .any(|a| a.agent_ref == r && a.state == AgentState::Paused)
            {
                paused.insert(r.to_owned(), Instant::now());
            }
        }
        self.keep_pressure_pauses(&paused);
    }

    /// Reads the pressure at `now`, and acts on it: idle agents sleep early
    /// at once; once it has lasted, an agent paused for it long enough is
    /// ended, or the supervised agent furthest over 80% of its memory is
    /// paused, or the agent frozen longest whose memory is still held is
    /// ended. Returns the ref it ended. Once it eases, agents it paused run
    /// again.
    pub async fn check_pressure(&self, w: &mut PressureWatch, now: Instant) -> Option<String> {
        let reading = match self.pressure.read() {
            Some(r) if r.high() => r,
            _ => {
                w.since = None;
                self.release_pressure_pauses().await;
                return None;
            }
        };
        let since = *w.since.get_or_insert(now);
        // Idle agents sleep early, debounce or not: one whose sleep frees
        // memory gives it back, and a supervised one frozen can be swapped
        // out. On a Linux host's cgroups that frees nothing, and ending
        // frozen agents is what makes room there.
        let why = format!("early: memory is short ({reading})");
        while self
            .sleep_one_early(crate::sleep::Early::UnderPressure, &why)
            .await
            .is_some()
        {}
        if now.duration_since(since) < END_AFTER
            || w.last_end
                .is_some_and(|t| now.duration_since(t) < END_EVERY)
        {
            return None;
        }
        if let Some(ended) = self.end_paused_too_long(&reading, now).await {
            w.last_end = Some(now);
            return Some(ended);
        }
        if self.pause_the_hog(&reading, now).await {
            w.last_end = Some(now);
            return None;
        }
        let ended = self.end_oldest_frozen(&reading).await?;
        w.last_end = Some(now);
        Some(ended)
    }

    /// Ends the agent paused for pressure longest, if that's been
    /// [`KILL_AFTER`]: resumable, as asleep.
    async fn end_paused_too_long(&self, reading: &Reading, now: Instant) -> Option<String> {
        let mut due: Vec<(Instant, String)> = lock(&self.pressure_paused)
            .iter()
            .filter(|(_, at)| now.duration_since(**at) >= KILL_AFTER)
            .map(|(r, at)| (*at, r.clone()))
            .collect();
        due.sort();
        for (_, r) in due {
            let Ok(_held) = self.hold(&r) else {
                continue;
            };
            self.pressure_unpause(&r);
            let Ok(mut a) = self.store().agent(LOCAL_HOST, &r) else {
                continue;
            };
            // Woken meanwhile, it's not this pause's to end.
            if a.state != AgentState::Paused {
                continue;
            }
            let (Ok(backend), Ok(handle)) = (self.backend(a.environment), Core::handle_of(&a))
            else {
                continue;
            };
            match blocking(move || backend.stop(&handle)).await {
                Ok(()) => {
                    self.set_state(&mut a, AgentState::Asleep);
                    crate::log!(
                        "vibed: memory is still short ({reading}) {} min after {r} was paused \
                         for it, so it was ended; a send resumes it with claude --resume",
                        KILL_AFTER.as_secs() / 60
                    );
                    return Some(r);
                }
                Err(e) => crate::log!(
                    "vibed: memory is short, but {r} can't be ended: {}",
                    e.message
                ),
            }
        }
        None
    }

    /// Pauses the agent, on a backend whose limits a supervisor holds,
    /// furthest over [`PAUSE_PAST`] of its memory, unless one is paused for
    /// pressure already. Says whether it paused one.
    async fn pause_the_hog(&self, reading: &Reading, now: Instant) -> bool {
        if !lock(&self.pressure_paused).is_empty() {
            return false;
        }
        let mut hogs = Vec::new();
        let agents = self.store().agents(LOCAL_HOST).unwrap_or_default();
        for a in agents {
            if a.starting
                || self.is_busy(&a.agent_ref)
                || matches!(
                    a.state,
                    AgentState::Asleep | AgentState::Paused | AgentState::Exited
                )
            {
                continue;
            }
            let (Ok(backend), Ok(handle)) = (self.backend(a.environment), Core::handle_of(&a))
            else {
                continue;
            };
            if backend.capabilities().kernel_limits || handle.session.is_none() {
                continue;
            }
            let usage = {
                let (b, h) = (Arc::clone(&backend), handle.clone());
                match blocking(move || b.usage(&h)).await {
                    Ok(u) => u,
                    Err(_) => continue,
                }
            };
            let share = usage.memory as f64 / a.memory.max(1) as f64;
            if share >= PAUSE_PAST {
                hogs.push((share, a, backend, handle, usage.memory));
            }
        }
        hogs.sort_by(|x, y| {
            y.0.total_cmp(&x.0)
                .then_with(|| x.1.agent_ref.cmp(&y.1.agent_ref))
        });
        for (share, a, backend, handle, used) in hogs {
            let Ok(_held) = self.hold(&a.agent_ref) else {
                continue;
            };
            let Ok(mut a) = self.store().agent(LOCAL_HOST, &a.agent_ref) else {
                continue;
            };
            if matches!(
                a.state,
                AgentState::Asleep | AgentState::Paused | AgentState::Exited
            ) {
                continue;
            }
            if let Err(e) = blocking(move || backend.sleep(&handle)).await {
                crate::log!(
                    "vibed: memory is short, but {} can't be paused: {}",
                    a.agent_ref,
                    e.message
                );
                continue;
            }
            self.pressure_pause(&a.agent_ref, now);
            self.set_state(&mut a, AgentState::Paused);
            let message = format!(
                "paused: memory is short ({reading}), and it's at {:.0}% of its {} ({}). It's \
                 ended in {} min if that lasts; a send resumes it",
                share * 100.0,
                size(a.memory),
                size(used),
                KILL_AFTER.as_secs() / 60
            );
            self.announce(&a, Limit::Memory, message);
            return true;
        }
        false
    }

    /// Lets every agent paused for pressure run again, now it's eased.
    async fn release_pressure_pauses(&self) {
        let paused: Vec<String> = lock(&self.pressure_paused).keys().cloned().collect();
        for r in paused {
            let Ok(_held) = self.hold(&r) else {
                continue;
            };
            self.pressure_unpause(&r);
            let Ok(mut a) = self.store().agent(LOCAL_HOST, &r) else {
                continue;
            };
            if a.state != AgentState::Paused {
                continue;
            }
            let (Ok(backend), Ok(handle)) = (self.backend(a.environment), Core::handle_of(&a))
            else {
                continue;
            };
            let host = vibe_core::limits::load(&self.layout.host_config()).unwrap_or_default();
            let limits = Limits::new(a.memory, &host);
            let (b, h) = (Arc::clone(&backend), handle.clone());
            let woke = async {
                blocking(move || b.apply(&h, &limits)).await?;
                blocking(move || backend.wake(&handle)).await
            };
            match woke.await {
                Ok(_) => {
                    let status = self.agent_status(&mut a, self.idle_debounce()).await;
                    self.set_state(&mut a, status.state());
                    lock(&self.sleep_watch).woke(&r);
                    crate::log!("vibed: memory pressure eased, so {r} runs again");
                }
                Err(e) => crate::log!(
                    "vibed: memory pressure eased, but {r} can't run again: {}",
                    e.message
                ),
            }
        }
    }

    /// Ends the agent frozen longest whose memory is still held, on a
    /// backend whose limits the kernel holds.
    async fn end_oldest_frozen(&self, reading: &Reading) -> Option<String> {
        let frozen_state = |s: AgentState| matches!(s, AgentState::Asleep | AgentState::Paused);
        let mut frozen = Vec::new();
        for a in self.store().agents(LOCAL_HOST).ok()? {
            if a.starting
                || !frozen_state(a.state)
                || self.is_busy(&a.agent_ref)
                || lock(&self.pressure_paused).contains_key(&a.agent_ref)
            {
                continue;
            }
            let Ok(backend) = self.backend(a.environment) else {
                continue;
            };
            let caps = backend.capabilities();
            // Saved to disk, it holds no memory to give back; supervised,
            // the agent using the most is paused instead.
            if caps.sleep_frees_memory || !caps.kernel_limits {
                continue;
            }
            // Frozen since before this vibed started, it's taken to have
            // been since it was made.
            let since = lock(&self.asleep_since)
                .since(&a.agent_ref)
                .unwrap_or((a.created_at, 0));
            frozen.push((since, a, backend));
        }
        frozen.sort_by(|x, y| (x.0, &x.1.agent_ref).cmp(&(y.0, &y.1.agent_ref)));
        for (_, a, backend) in frozen {
            // Its processes must still be there to end.
            if !self.session_runs(&a.agent_ref).await {
                continue;
            }
            let Ok(_held) = self.hold(&a.agent_ref) else {
                continue;
            };
            // A send may have woken it meanwhile: then it's not to be ended.
            let Ok(mut a) = self.store().agent(LOCAL_HOST, &a.agent_ref) else {
                continue;
            };
            if !frozen_state(a.state) || !self.session_runs(&a.agent_ref).await {
                continue;
            }
            let Ok(handle) = Core::handle_of(&a) else {
                continue;
            };
            let was = a.state;
            let b = Arc::clone(&backend);
            match blocking(move || b.stop(&handle)).await {
                Ok(()) => {
                    self.set_state(&mut a, AgentState::Asleep);
                    crate::log!(
                        "vibed: memory is short ({reading}): ended {}, which was {was}; a send \
                         resumes it with claude --resume",
                        a.agent_ref
                    );
                    return Some(a.agent_ref);
                }
                Err(e) => crate::log!(
                    "vibed: memory is short, but {} can't be ended: {}",
                    a.agent_ref,
                    e.message
                ),
            }
        }
        None
    }

    /// Runs [`Core::check_pressure`] every `every`.
    pub fn spawn_pressure_watch(self: &Arc<Self>, every: Duration) -> tokio::task::JoinHandle<()> {
        let core = Arc::clone(self);
        tokio::spawn(async move {
            let mut w = PressureWatch::default();
            let mut ticks = tokio::time::interval(every);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticks.tick().await;
                core.check_pressure(&mut w, Instant::now()).await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kernels_numbers() {
        let psi = Psi::parse(
            "some avg10=31.50 avg60=4.00 avg300=1.00 total=15613848837\n\
             full avg10=2.25 avg60=0.00 avg300=0.00 total=13279565009\n",
        )
        .unwrap();
        assert_eq!(
            psi,
            Psi {
                some10: 31.5,
                full10: 2.25
            }
        );
        assert!(psi.high());
        assert_eq!(
            psi.to_string(),
            "its programs waited on memory 32% of the last 10 s"
        );
        let old = Psi::parse("some avg10=1.00 avg60=0.00 avg300=0.00 total=1\n").unwrap();
        assert_eq!(old.full10, 0.0);
        assert!(!old.high());
        assert!(
            Psi {
                some10: 0.0,
                full10: HIGH_FULL
            }
            .high()
        );
        assert_eq!(Psi::parse(""), None);
        let r = Reading::from(psi);
        assert!(r.high());
        assert_eq!(r.to_string(), psi.to_string());
        assert_eq!(Psi::parse("some avg10=x\n"), None);
    }

    #[test]
    fn only_a_critical_level_is_short() {
        assert!(Reading::from(Level::Critical).high());
        assert!(!Reading::from(Level::Warn).high());
        assert!(!Reading::from(Level::Normal).high());
        assert_eq!(
            Reading::from(Level::Critical).to_string(),
            "macOS says memory pressure is critical"
        );
    }

    /// On a Mac, the level is read from the dispatch source, and an idle
    /// test machine isn't critical.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_mac_says_its_level() {
        assert!(matches!(HostPressure.read(), Some(Reading::Level(_))));
    }
}
