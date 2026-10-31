//! Memory pressure on a Linux host (PRD: Failures, Memory pressure on a
//! Linux host): refuse new agents there, then end frozen agents, oldest
//! first. Each cgroup's own memory limit already stops a single agent from
//! taking the server; this is for many agents that fit their limits
//! together outgrowing it.
//!
//! Pressure is the kernel's pressure stall information for memory
//! (`/proc/pressure/memory`): the share of the last 10 s in which some task
//! waited on memory (`some`), or every task did (`full`). Past
//! [`HIGH_SOME`] or [`HIGH_FULL`], starts are refused. If it stays that high
//! for [`END_AFTER`], the agent frozen longest whose memory is still held is
//! ended, and then one more every [`END_EVERY`] while it lasts. Frozen is
//! asleep, or paused by a limit, on a backend whose sleep keeps its memory
//! (`sleep_frees_memory` false, so ending it gives memory back). An ended
//! agent is `asleep`; a send or an attach resumes it with `claude --resume`,
//! which loses a pending question (PRD: Going to sleep).
//!
//! Where the kernel doesn't say (macOS, which has its own signal, or a
//! Linux without PSI), there's no pressure here.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use vibe_core::model::{AgentState, LOCAL_HOST};

use crate::agents::blocking;
use crate::core::{Core, lock};

/// How often pressure is read.
pub const PRESSURE_EVERY: Duration = Duration::from_secs(2);

/// Past this share of the last 10 s with some task waiting on memory, the
/// host is short of it.
pub const HIGH_SOME: f64 = 30.0;

/// Past this share with every task waiting on memory, likewise.
pub const HIGH_FULL: f64 = 10.0;

/// How long it may stay short before a frozen agent is ended.
pub const END_AFTER: Duration = Duration::from_secs(10);

/// How long after ending one the next may be, while it's still short: the
/// kernel's 10 s averages take that long to show the memory came back.
pub const END_EVERY: Duration = Duration::from_secs(15);

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

/// Where memory pressure comes from.
pub trait MemoryPressure: Send + Sync + fmt::Debug {
    /// The pressure now, or `None` where the kernel doesn't say.
    fn read(&self) -> Option<Psi>;
}

/// This host's: `/proc/pressure/memory`.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostPressure;

impl MemoryPressure for HostPressure {
    fn read(&self) -> Option<Psi> {
        Psi::parse(&std::fs::read_to_string("/proc/pressure/memory").ok()?)
    }
}

/// No pressure, ever: for tests, and hosts that don't measure it.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPressure;

impl MemoryPressure for NoPressure {
    fn read(&self) -> Option<Psi> {
        None
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

/// How long pressure has lasted, and when an agent was last ended for it.
#[derive(Debug, Default, Clone)]
pub struct PressureWatch {
    since: Option<Instant>,
    last_end: Option<Instant>,
}

impl Core {
    /// Why a start must be refused now, if memory is short.
    pub(crate) fn short_of_memory(&self) -> Option<String> {
        let psi = self.pressure.read().filter(Psi::high)?;
        Some(format!(
            "this host is short of memory right now ({psi}). Try again once it eases, or \
             delete agents you're done with"
        ))
    }

    /// Reads the pressure at `now`, and if it has lasted long enough, ends
    /// the agent asleep longest whose memory is still held. Returns the ref
    /// it ended.
    pub async fn check_pressure(&self, w: &mut PressureWatch, now: Instant) -> Option<String> {
        let psi = match self.pressure.read() {
            Some(p) if p.high() => p,
            _ => {
                w.since = None;
                return None;
            }
        };
        let since = *w.since.get_or_insert(now);
        if now.duration_since(since) < END_AFTER
            || w.last_end
                .is_some_and(|t| now.duration_since(t) < END_EVERY)
        {
            return None;
        }
        let ended = self.end_oldest_frozen(&psi).await?;
        w.last_end = Some(now);
        Some(ended)
    }

    /// Ends the agent frozen longest whose memory is still held.
    async fn end_oldest_frozen(&self, psi: &Psi) -> Option<String> {
        let frozen_state = |s: AgentState| matches!(s, AgentState::Asleep | AgentState::Paused);
        let mut frozen = Vec::new();
        for a in self.store().agents(LOCAL_HOST).ok()? {
            if a.starting || !frozen_state(a.state) || self.is_busy(&a.agent_ref) {
                continue;
            }
            let Ok(backend) = self.backend(a.environment) else {
                continue;
            };
            // Saved to disk, it holds no memory to give back.
            if backend.capabilities().sleep_frees_memory {
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
                        "vibed: memory is short ({psi}): ended {}, which was {was}; a send \
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
        assert_eq!(Psi::parse("some avg10=x\n"), None);
    }
}
