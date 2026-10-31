//! Limit hits, as clients see them (PRD: How macos is governed; Key user
//! flows, Monitor). Each agent's backend holds it to its limits; this asks
//! every backend now and then what its agents use, through the `Limiter`
//! trait alone. A pause makes the agent `paused` and fires an
//! `agent.limit` event; so does each new warning, such as memory at 80% of
//! the profile's or a worktree past `disk_soft`. Once a wake has let it
//! go, the agent is back to what its transcript says.
//!
//! It also holds running agents to host limits edited since they started,
//! as `host.toml` takes effect at once (PRD: Host limits). A paused agent
//! keeps the limits it was paused under until it's woken.
//!
//! After a restart of `vibed`, an agent already recorded `paused` isn't
//! announced again; its warnings are, once.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use vibe_core::limits;
use vibe_core::model::{AgentState, EventKind, LOCAL_HOST};
use vibe_core::store::AgentRecord;
use vibe_env::{Limit, Limits, Usage};

use crate::agents::{blocking, size};
use crate::core::Core;

/// How often agents' limits are looked at.
pub const LIMITS_EVERY: Duration = Duration::from_secs(2);

/// What was last seen of an agent's limits.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Seen {
    paused: Option<Limit>,
    warnings: Vec<Limit>,
    /// The limits it was last held to.
    applied: Option<Limits>,
}

impl Core {
    /// Asks each awake agent's backend what it uses, and says what changed
    /// since `seen`.
    pub async fn check_limits(&self, seen: &mut HashMap<String, Seen>) {
        let Ok(agents) = self.store().agents(LOCAL_HOST) else {
            return;
        };
        seen.retain(|r, _| agents.iter().any(|a| &a.agent_ref == r));
        let host = limits::load(&self.layout().host_config()).unwrap_or_default();
        for mut a in agents {
            // Asleep, its backend may have nothing to ask (a VM saved to
            // disk); exited, nothing runs.
            // Paused for memory pressure, it's that watch's to let go.
            if a.starting
                || self.is_busy(&a.agent_ref)
                || matches!(a.state, AgentState::Asleep | AgentState::Exited)
                || crate::core::lock(&self.pressure_paused).contains_key(&a.agent_ref)
            {
                continue;
            }
            let (Ok(backend), Ok(handle)) = (self.backend(a.environment), Core::handle_of(&a))
            else {
                continue;
            };
            if handle.session.is_none() {
                continue;
            }
            let usage = {
                let (backend, handle) = (Arc::clone(&backend), handle.clone());
                match blocking(move || backend.usage(&handle)).await {
                    Ok(u) => u,
                    Err(_) => continue,
                }
            };
            let limits = Limits::new(a.memory, &host);
            // A supervisor puts an agent near its memory in the background;
            // the kernel just holds it to the limit.
            let backgrounds = !backend.capabilities().kernel_limits;
            // Seen for the first time, it's held to the limits there are
            // now, whatever it started with; one recorded paused was
            // announced before a restart of vibed.
            let mut before = seen.remove(&a.agent_ref).unwrap_or_else(|| {
                if a.state == AgentState::Paused {
                    Seen {
                        paused: usage.paused,
                        warnings: usage.warnings.clone(),
                        applied: None,
                    }
                } else {
                    Seen::default()
                }
            });
            // A stop, delete or resume that began meanwhile goes first.
            if self.is_busy(&a.agent_ref) {
                continue;
            }
            if usage.paused.is_none() && before.applied != Some(limits) {
                // Tried once per change, so a failure is said once.
                before.applied = Some(limits);
                match blocking(move || backend.apply(&handle, &limits)).await {
                    Ok(()) => crate::log!("vibed: {} is held to new limits", a.agent_ref),
                    Err(e) => crate::log!(
                        "vibed: can't hold {} to new limits: {}",
                        a.agent_ref,
                        e.message
                    ),
                }
            }
            if self.is_busy(&a.agent_ref) {
                continue;
            }
            if let Some(limit) = usage.paused
                && before.paused != Some(limit)
            {
                self.announce(&a, limit, pause_message(limit, &usage, &limits));
            }
            for &w in usage
                .warnings
                .iter()
                .filter(|w| !before.warnings.contains(w))
            {
                if usage.paused.is_none() {
                    self.announce(&a, w, warning_message(w, &usage, &limits, backgrounds));
                }
            }
            match usage.paused {
                Some(_) if a.state != AgentState::Paused => {
                    self.set_state(&mut a, AgentState::Paused)
                }
                None if a.state == AgentState::Paused => {
                    let status = self.agent_status(&mut a, self.idle_debounce()).await;
                    self.set_state(&mut a, status.state());
                }
                _ => {}
            }
            seen.insert(
                a.agent_ref.clone(),
                Seen {
                    paused: usage.paused,
                    warnings: usage.warnings,
                    applied: before.applied,
                },
            );
        }
    }

    pub(crate) fn announce(&self, a: &AgentRecord, limit: Limit, message: String) {
        crate::log!("vibed: {}: {message}", a.agent_ref);
        let mut e = self.event(EventKind::AgentLimit);
        e.project = Some(a.project.clone());
        e.agent = Some(a.agent_ref.clone());
        e.limit = Some(limit.as_str().to_owned());
        e.message = Some(message);
        self.publish(e);
    }

    /// Runs [`Core::check_limits`] every `every`.
    pub fn spawn_limit_watch(self: &Arc<Self>, every: Duration) -> tokio::task::JoinHandle<()> {
        let core = Arc::clone(self);
        tokio::spawn(async move {
            let mut seen = HashMap::new();
            let mut ticks = tokio::time::interval(every);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticks.tick().await;
                core.check_limits(&mut seen).await;
            }
        })
    }
}

fn pause_message(limit: Limit, u: &Usage, l: &Limits) -> String {
    match limit {
        Limit::Memory => format!("paused at 100% of its {}", size(l.memory)),
        Limit::Processes => format!(
            "paused with {} processes, over its limit of {}",
            u.processes, l.max_processes
        ),
        Limit::Disk => format!(
            "paused: its worktree and temp files take {}, past disk_hard ({})",
            size(u.disk.unwrap_or(0)),
            size(l.disk_hard)
        ),
        Limit::Cpu => "paused for CPU".into(),
    }
}

fn warning_message(limit: Limit, u: &Usage, l: &Limits, backgrounds: bool) -> String {
    match limit {
        Limit::Memory => format!(
            "at {}% of its {}{}",
            (u128::from(u.memory) * 100 / u128::from(l.memory.max(1))),
            size(l.memory),
            if backgrounds {
                ", so it runs in the background"
            } else {
                ""
            }
        ),
        Limit::Disk => format!(
            "its worktree and temp files take {}, past disk_soft ({})",
            size(u.disk.unwrap_or(0)),
            size(l.disk_soft)
        ),
        Limit::Processes => format!(
            "{} processes, near its limit of {}",
            u.processes, l.max_processes
        ),
        Limit::Cpu => "over its CPU cap".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vibe_core::limits::HostLimits;
    use vibe_core::units::GIB;

    #[test]
    fn messages_say_what_it_hit() {
        let l = Limits::new(4 * GIB, &HostLimits::default());
        let u = Usage {
            memory: 3_436_000_000,
            processes: 513,
            disk: Some(31 * GIB),
            ..Usage::default()
        };
        assert_eq!(
            pause_message(Limit::Memory, &u, &l),
            "paused at 100% of its 4G"
        );
        assert_eq!(
            pause_message(Limit::Processes, &u, &l),
            "paused with 513 processes, over its limit of 512"
        );
        assert_eq!(
            pause_message(Limit::Disk, &u, &l),
            "paused: its worktree and temp files take 31G, past disk_hard (30G)"
        );
        assert_eq!(
            warning_message(Limit::Memory, &u, &l, true),
            "at 80% of its 4G, so it runs in the background"
        );
        assert_eq!(
            warning_message(Limit::Memory, &u, &l, false),
            "at 80% of its 4G"
        );
        let u = Usage {
            disk: Some(21 * GIB),
            ..u
        };
        assert_eq!(
            warning_message(Limit::Disk, &u, &l, true),
            "its worktree and temp files take 21G, past disk_soft (20G)"
        );
    }
}
