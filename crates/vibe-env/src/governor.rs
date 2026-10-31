//! What the `macos` governor decides, from what it sees of an agent (PRD:
//! How macos is governed). It's pure: [`crate::macos`] samples the agent's
//! tree, hands the sample here, and acts on the [`Decision`]. The levers,
//! mildest first:
//!
//! - **CPU**, in cores, from CPU-time deltas between samples. Over the cap,
//!   the agent is first put in the background (`PRIO_DARWIN_BG`). If it's
//!   still over after [`ESCALATE_AFTER`] samples, `SIGSTOP`/`SIGCONT` duty
//!   cycling holds it to the cap: it runs only [`Decision::duty`] of each
//!   period. Each lever is let go once the agent has stayed under the cap
//!   for [`RELAX_AFTER`].
//! - **Memory**, the sum of `phys_footprint`: at [`WARN_PERCENT`] of the
//!   profile's memory, background and a warning; at 100%, a pause.
//! - **Processes**: over `max_processes`, a pause (the fork-bomb stop).
//! - **Disk**, the worktree and its temp dir as last measured: past
//!   `disk_soft`, a warning; past `disk_hard`, a pause.

use std::collections::HashMap;
use std::time::Duration;

use crate::{Limit, Limits, Proc};

/// At this share of its memory, an agent is put in the background and
/// warned about.
pub const WARN_PERCENT: u64 = 80;

/// Below this share, the memory warning is over.
pub const CLEAR_PERCENT: u64 = 75;

/// Samples in a row over the CPU cap, in the background, before duty
/// cycling starts.
pub const ESCALATE_AFTER: u32 = 3;

/// How long an agent stays under its CPU cap before a lever is let go.
pub const RELAX_AFTER: Duration = Duration::from_secs(10);

/// The least share of each period a duty-cycled agent runs.
pub const MIN_DUTY: f64 = 0.05;

/// Two samples closer together than this don't measure CPU.
const MIN_INTERVAL: Duration = Duration::from_millis(200);

/// What to do with an agent now.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// Pause it, for this limit.
    pub pause: Option<Limit>,
    /// Keep it in the background.
    pub background: bool,
    /// The share of each period it may run: 1 is unthrottled.
    pub duty: f64,
    /// The soft limits it's past.
    pub warnings: Vec<Limit>,
    /// The cores it used since the last sample, while it ran; `None` until
    /// there are two samples.
    pub demand: Option<f64>,
}

/// One agent's governor: its limits and what it has done about them.
#[derive(Debug, Clone)]
pub struct Governor {
    limits: Limits,
    /// When the last sample was taken, and each process's CPU time then.
    last: Option<Sample>,
    cpu_background: bool,
    cycling: bool,
    duty: f64,
    /// Samples in a row over the cap.
    over: u32,
    /// Since when it has been under the cap.
    under_since: Option<Duration>,
    memory_warned: bool,
    demand: Option<f64>,
}

#[derive(Debug, Clone)]
struct Sample {
    at: Duration,
    wall_ms: u64,
    cpu: HashMap<u32, u64>,
}

impl Governor {
    pub fn new(limits: Limits) -> Governor {
        Governor {
            limits,
            last: None,
            cpu_background: false,
            cycling: false,
            duty: 1.0,
            over: 0,
            under_since: None,
            memory_warned: false,
            demand: None,
        }
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    pub fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    /// Forgets what it measured and let go of, as after the agent slept or
    /// was paused: its CPU while stopped says nothing about its demand.
    pub fn reset(&mut self) {
        *self = Governor::new(self.limits);
    }

    /// Takes a sample of the agent's processes (its shim left out) at `at`
    /// on a monotonic clock, `wall_ms` since the epoch, with its disk as
    /// last measured, and says what to do.
    pub fn observe(
        &mut self,
        at: Duration,
        wall_ms: u64,
        procs: &[Proc],
        disk: Option<u64>,
    ) -> Decision {
        self.observe_cpu(at, wall_ms, procs);
        let l = &self.limits;
        let memory: u64 = procs.iter().map(|p| p.memory).sum();
        let percent = |share: u64| u128::from(l.memory) * u128::from(share);
        let used = u128::from(memory) * 100;
        if l.memory > 0 && used >= percent(WARN_PERCENT) {
            self.memory_warned = true;
        } else if used < percent(CLEAR_PERCENT) {
            self.memory_warned = false;
        }
        let pause = if l.max_processes > 0 && procs.len() > l.max_processes as usize {
            Some(Limit::Processes)
        } else if l.memory > 0 && memory >= l.memory {
            Some(Limit::Memory)
        } else if l.disk_hard > 0 && disk.is_some_and(|d| d >= l.disk_hard) {
            Some(Limit::Disk)
        } else {
            None
        };
        let mut warnings = Vec::new();
        if self.memory_warned {
            warnings.push(Limit::Memory);
        }
        if l.disk_soft > 0 && disk.is_some_and(|d| d >= l.disk_soft) {
            warnings.push(Limit::Disk);
        }
        Decision {
            pause,
            background: self.cpu_background || self.memory_warned,
            duty: if self.cycling { self.duty } else { 1.0 },
            warnings,
            demand: self.demand,
        }
    }

    fn observe_cpu(&mut self, at: Duration, wall_ms: u64, procs: &[Proc]) {
        let cpu: HashMap<u32, u64> = procs.iter().map(|p| (p.pid, p.cpu_ms)).collect();
        let Some(last) = &self.last else {
            self.last = Some(Sample { at, wall_ms, cpu });
            return;
        };
        let elapsed = at.saturating_sub(last.at);
        if elapsed < MIN_INTERVAL {
            return;
        }
        // Each process's CPU since the last sample. One not seen then
        // counts whole only if it started since; one found later (by the
        // session scan) would otherwise bring its whole past with it.
        let used_ms: u64 = procs
            .iter()
            .map(|p| match last.cpu.get(&p.pid) {
                Some(&before) if p.cpu_ms >= before => p.cpu_ms - before,
                Some(_) => p.cpu_ms,
                None if p.started_ms + 1000 >= last.wall_ms => p.cpu_ms,
                None => 0,
            })
            .sum();
        self.last = Some(Sample { at, wall_ms, cpu });
        let cap = f64::from(self.limits.cpu_cores);
        // While cycled, it ran only `duty` of the time: what it would use
        // unthrottled is more.
        let ran = if self.cycling { self.duty } else { 1.0 };
        let demand = used_ms as f64 / elapsed.as_millis() as f64 / ran;
        self.demand = Some(demand);
        if cap <= 0.0 {
            return;
        }
        if demand > cap {
            self.under_since = None;
            if !self.cpu_background {
                self.cpu_background = true;
                self.over = 0;
            } else {
                self.over += 1;
                if self.over >= ESCALATE_AFTER {
                    self.cycling = true;
                }
            }
        } else {
            self.over = 0;
            let since = *self.under_since.get_or_insert(at);
            let relaxed = at.saturating_sub(since) >= RELAX_AFTER;
            if self.cycling {
                if relaxed && self.duty >= 0.99 {
                    self.cycling = false;
                    self.duty = 1.0;
                    self.under_since = Some(at);
                }
            } else if relaxed {
                self.cpu_background = false;
            }
        }
        if self.cycling {
            // Down at once to what holds it to the cap; back up halfway
            // each sample, so it doesn't swing between the two.
            let target = (cap / demand.max(f64::MIN_POSITIVE)).clamp(MIN_DUTY, 1.0);
            self.duty = if target < self.duty {
                target
            } else {
                self.duty + (target - self.duty) / 2.0
            };
            if self.duty > 0.99 {
                self.duty = 1.0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    fn limits() -> Limits {
        Limits {
            memory: 1000 * MIB,
            cpu_cores: 2,
            max_processes: 10,
            disk_soft: 100 * MIB,
            disk_hard: 200 * MIB,
        }
    }

    fn proc(pid: u32, cpu_ms: u64, memory: u64) -> Proc {
        Proc {
            pid,
            ppid: 1,
            name: "p".into(),
            args: Vec::new(),
            stopped: false,
            started_ms: 0,
            cpu_ms,
            memory,
        }
    }

    /// A clock for samples a second apart, and a tree using `cores`.
    struct Feed {
        g: Governor,
        t: u64,
        cpu: u64,
    }

    impl Feed {
        fn new() -> Feed {
            Feed {
                g: Governor::new(limits()),
                t: 0,
                cpu: 0,
            }
        }

        /// One second in which the tree, unthrottled, would use `cores`;
        /// throttled, it runs only its duty of it.
        fn second(&mut self, cores: f64) -> Decision {
            let duty = self.last_duty();
            self.t += 1000;
            self.cpu += (cores * duty * 1000.0) as u64;
            let procs = [proc(10, self.cpu, MIB)];
            self.g.observe(
                Duration::from_millis(self.t),
                1_000_000 + self.t,
                &procs,
                None,
            )
        }

        fn last_duty(&self) -> f64 {
            if self.g.cycling { self.g.duty } else { 1.0 }
        }
    }

    #[test]
    fn cpu_over_the_cap_goes_to_the_background_then_duty_cycles_then_lets_go() {
        let mut f = Feed::new();
        // The first sample only sets the baseline.
        let d = f.second(1.0);
        assert_eq!((d.background, d.duty, d.demand), (false, 1.0, None));
        let d = f.second(1.0);
        assert!(!d.background && d.duty == 1.0);
        assert!((d.demand.unwrap() - 1.0).abs() < 0.01, "{d:?}");

        // Over: the background first.
        let d = f.second(6.0);
        assert!(d.background && d.duty == 1.0, "{d:?}");
        // Still over: the background alone, for a while.
        for _ in 0..ESCALATE_AFTER - 1 {
            let d = f.second(6.0);
            assert!(d.background && d.duty == 1.0, "{d:?}");
        }
        // Then duty cycling, to what holds 6 cores to 2.
        let d = f.second(6.0);
        assert!((d.duty - 1.0 / 3.0).abs() < 0.01, "{d:?}");
        // Throttled, it's measured at what it would use, not what it got.
        for _ in 0..5 {
            let d = f.second(6.0);
            assert!((d.demand.unwrap() - 6.0).abs() < 0.1, "{d:?}");
            assert!((d.duty - 1.0 / 3.0).abs() < 0.02, "{d:?}");
        }
        // Wanting less, it's given more back, halfway each time, and never
        // more than holds it to the cap.
        let d = f.second(3.0);
        assert!(d.duty > 1.0 / 3.0 && d.duty < 2.0 / 3.0, "{d:?}");
        // Under the cap, the duty climbs back to 1, then cycling stops and
        // later the background goes.
        let mut stopped_cycling = None;
        let mut left_background = None;
        for s in 0..40 {
            let d = f.second(1.0);
            if d.duty == 1.0 && stopped_cycling.is_none() && !f.g.cycling {
                stopped_cycling = Some(s);
            }
            if !d.background && left_background.is_none() {
                left_background = Some(s);
            }
        }
        let (c, b) = (stopped_cycling.unwrap(), left_background.unwrap());
        assert!(c >= 9, "cycling stopped after {c} seconds under the cap");
        assert!(b > c, "the background went at {b}, cycling at {c}");
        assert!(!f.g.cycling && !f.g.cpu_background);
    }

    #[test]
    fn a_short_burst_only_goes_to_the_background() {
        let mut f = Feed::new();
        f.second(0.5);
        f.second(5.0);
        let d = f.second(0.5);
        assert!(d.background && d.duty == 1.0, "{d:?}");
        let d = f.second(5.0);
        assert!(d.background && d.duty == 1.0, "{d:?}");
        assert!(!f.g.cycling);
    }

    #[test]
    fn no_cap_means_no_cpu_lever() {
        let mut g = Governor::new(Limits {
            cpu_cores: 0,
            ..limits()
        });
        for s in 0..10u64 {
            let d = g.observe(
                Duration::from_secs(s),
                s * 1000,
                &[proc(1, s * 64_000, MIB)],
                None,
            );
            assert!(!d.background && d.duty == 1.0, "{d:?}");
        }
    }

    #[test]
    fn cpu_of_processes_that_come_and_go() {
        let mut g = Governor::new(limits());
        let at = |s: u64| Duration::from_secs(s);
        g.observe(at(0), 10_000, &[proc(1, 5000, 0)], None);
        // A new process counts whole if it started since; one that was
        // there all along but only now found counts from now; one whose
        // pid was reused (its CPU went down) counts whole; one gone counts
        // nothing.
        let mut new = proc(2, 400, 0);
        new.started_ms = 10_500;
        let mut old = proc(3, 90_000, 0);
        old.started_ms = 1_000;
        let d = g.observe(at(1), 11_000, &[proc(1, 5300, 0), new, old], None);
        assert!((d.demand.unwrap() - 0.7).abs() < 0.01, "{d:?}");
        let d = g.observe(at(2), 12_000, &[proc(1, 100, 0)], None);
        assert!((d.demand.unwrap() - 0.1).abs() < 0.01, "{d:?}");
        // Samples too close together don't measure.
        let d = g.observe(at(2) + Duration::from_millis(50), 12_050, &[], None);
        assert!((d.demand.unwrap() - 0.1).abs() < 0.01, "{d:?}");
    }

    #[test]
    fn memory_warns_at_80_percent_and_pauses_at_100() {
        let mut g = Governor::new(limits());
        let at = |s: u64| Duration::from_secs(s);
        let two = |a: u64, b: u64| [proc(1, 0, a * MIB), proc(2, 0, b * MIB)];
        let d = g.observe(at(0), 0, &two(400, 390), None);
        assert_eq!((d.pause, d.background), (None, false));
        assert!(d.warnings.is_empty());
        // 800 of 1000: the background, and a warning.
        let d = g.observe(at(1), 1000, &two(400, 400), None);
        assert_eq!(d.pause, None);
        assert!(d.background);
        assert_eq!(d.warnings, vec![Limit::Memory]);
        // Still warned until it's below 75%.
        let d = g.observe(at(2), 2000, &two(400, 360), None);
        assert!(d.background && d.warnings == vec![Limit::Memory]);
        let d = g.observe(at(3), 3000, &two(400, 340), None);
        assert!(!d.background && d.warnings.is_empty(), "{d:?}");
        // All of it: a pause.
        let d = g.observe(at(4), 4000, &two(500, 500), None);
        assert_eq!(d.pause, Some(Limit::Memory));
    }

    #[test]
    fn too_many_processes_pause_first() {
        let mut g = Governor::new(limits());
        let procs: Vec<Proc> = (1..=10).map(|p| proc(p, 0, 0)).collect();
        assert_eq!(g.observe(Duration::ZERO, 0, &procs, None).pause, None);
        let mut more = procs.clone();
        more.push(proc(11, 0, 2000 * MIB));
        let d = g.observe(Duration::from_secs(1), 1000, &more, Some(1 << 40));
        assert_eq!(d.pause, Some(Limit::Processes), "the fork-bomb stop wins");
    }

    #[test]
    fn disk_warns_past_soft_and_pauses_past_hard() {
        let mut g = Governor::new(limits());
        let at = Duration::ZERO;
        let d = g.observe(at, 0, &[], None);
        assert!(d.pause.is_none() && d.warnings.is_empty());
        let d = g.observe(at, 0, &[], Some(99 * MIB));
        assert!(d.pause.is_none() && d.warnings.is_empty());
        let d = g.observe(at, 0, &[], Some(100 * MIB));
        assert_eq!((d.pause, d.warnings.as_slice()), (None, &[Limit::Disk][..]));
        let d = g.observe(at, 0, &[], Some(200 * MIB));
        assert_eq!(d.pause, Some(Limit::Disk));
        // A disk past soft doesn't put it in the background.
        assert!(!d.background);
    }

    #[test]
    fn a_reset_lets_every_lever_go_but_keeps_the_limits() {
        let mut f = Feed::new();
        for _ in 0..10 {
            f.second(8.0);
        }
        assert!(f.g.cycling && f.g.cpu_background);
        f.g.set_limits(Limits {
            cpu_cores: 3,
            ..limits()
        });
        f.g.reset();
        assert!(!f.g.cycling && !f.g.cpu_background && f.g.last.is_none());
        assert_eq!(f.g.limits().cpu_cores, 3);
    }
}
