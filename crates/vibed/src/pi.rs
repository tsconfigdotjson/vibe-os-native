//! Pi on this host (PRD: Who gets which credentials; What's inside `linux`):
//! what's installed and which providers it's signed in to, for Settings, and
//! installing it, on the owner's word. VibeOS doesn't sign anyone in: `/login`
//! is Pi's own, and only the providers' names are ever read here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use vibe_core::model::{
    AgentKind, AgentState, Environment, EventKind, LOCAL_HOST, PiTarget, Profile,
};
use vibe_core::rpc::{
    ErrorCode, ErrorObject, HostParams, PiBinary, PiInstallParams, PiStatus, PiVms,
};
use vibe_core::store::AgentRecord;
use vibe_env::images::Images;
use vibe_env::pi_logins::{self, Broker, Copy, Synced};
use vibe_env::pi_release;
use vibe_env::{AgentHome, pi};

/// How often the logins are looked at: a `/login` or a refreshed token passed on
/// to every other copy, soon enough that nobody waits for it.
pub const PI_WATCH_EVERY: std::time::Duration = std::time::Duration::from_secs(3);

/// What was last said of Pi, to say so when it changes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PiSeen {
    own: Vec<String>,
    vms: Vec<String>,
    error: Option<String>,
    alerts: Vec<String>,
}

use crate::core::{Core, error, lock};

/// Where an alert about the store itself is kept, among those about each copy.
const STORE_ALERT: &str = "the store";

/// Where Pi's releases come from: the published ones, unless
/// `VIBE_PI_RELEASES` names others (a `file://` directory, say).
pub fn releases_from_env() -> String {
    std::env::var("VIBE_PI_RELEASES")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| pi_release::RELEASES.to_owned())
}

impl Core {
    /// The owner's own agent dir, which native agents use.
    fn own_pi_dir(&self) -> PathBuf {
        pi::own_dir(self.layout.home())
    }

    /// Whether any Pi agent still runs, from whichever install it started on.
    fn pi_agents_run(&self) -> bool {
        self.store()
            .agents(vibe_core::model::LOCAL_HOST)
            .map_or(true, |agents| {
                agents.iter().any(|a| {
                    a.agent == AgentKind::Pi && a.state != vibe_core::model::AgentState::Exited
                })
            })
    }

    /// Every private copy of Pi's login there is, with whether it's in use: the
    /// toolbox's once it has one, and each `linux` agent's. One whose VM can't be
    /// running has nothing new to say, and is given what's new when it next runs.
    fn pi_copies(&self) -> Vec<(Copy, bool)> {
        let store = self.layout.linux_pi_logins_dir();
        let mut copies = Vec::new();
        let toolbox = self.layout.linux_pi_signin_dir();
        if toolbox.is_dir() {
            copies.push((pi_logins::toolbox_copy(&toolbox, &store), true));
        }
        let agents = self.store().agents(LOCAL_HOST).unwrap_or_default();
        for a in &agents {
            if a.agent == AgentKind::Pi
                && let Ok(handle) = Core::handle_of(a)
                && let AgentHome::Pi(home) = &handle.home
                && home.config.is_some()
            {
                let running = !matches!(a.state, AgentState::Exited | AgentState::Asleep);
                copies.push((pi_logins::vm_copy(&a.agent_ref, &handle.agent_dir), running));
            }
        }
        copies
    }

    /// What's new for the owner from one look at a copy, kept in the alerts for as
    /// long as it stands.
    fn pi_note_synced(
        alerts: &mut BTreeMap<String, String>,
        copy: &Copy,
        synced: std::io::Result<Synced>,
    ) {
        let mut trouble = None;
        match synced {
            Err(e) => {
                let alert = format!("Pi's logins can't be kept in step: {e}");
                if alerts.get(STORE_ALERT) != Some(&alert) {
                    crate::log!("vibed: {alert}");
                    alerts.insert(STORE_ALERT.to_owned(), alert);
                }
                return;
            }
            Ok(Synced::Busy) => return,
            Ok(Synced::Gone) => {}
            Ok(Synced::Quarantined(why)) => trouble = Some(why),
            Ok(Synced::Done(report)) => {
                if !report.bad.is_empty() {
                    let bad: Vec<String> = report.bad.values().cloned().collect();
                    trouble = Some(bad.join("; "));
                }
                if !report.taken.is_empty() {
                    crate::log!(
                        "vibed: Pi's login for {} came from {}",
                        report.taken.join(", "),
                        copy.name
                    );
                }
                for (what, names) in [
                    ("changed there, and stays there", &report.kept),
                    (
                        "written back from a stale read, and replaced",
                        &report.stale,
                    ),
                ] {
                    if !names.is_empty() {
                        crate::log!(
                            "vibed: Pi's login for {} in {}: {what}",
                            names.join(", "),
                            copy.name
                        );
                    }
                }
                if let Some(aside) = &report.set_aside {
                    crate::log!("vibed: Pi's login store couldn't be read, so it's now {aside}");
                }
            }
        }
        alerts.remove(STORE_ALERT);
        match trouble {
            None => {
                alerts.remove(&copy.name);
            }
            Some(why) => {
                let alert = format!(
                    "{}: {why}, so that isn't shared with any other agent. Something in it may \
                     be hostile.",
                    copy.name
                );
                if alerts.get(&copy.name) != Some(&alert) {
                    crate::log!("vibed: Pi's login: {alert}");
                    alerts.insert(copy.name.clone(), alert);
                }
            }
        }
    }

    /// Looks at every copy of Pi's login in use, keeping each in step with the
    /// store. An alert goes only when the agent does: one asleep keeps its own.
    async fn pi_sync_logins(&self) {
        let copies = self.pi_copies();
        let broker = Broker::of(&self.layout);
        let known: Vec<String> = copies.iter().map(|(c, _)| c.name.clone()).collect();
        let done = tokio::task::spawn_blocking(move || {
            copies
                .into_iter()
                .filter(|(_, in_use)| *in_use)
                .map(|(copy, _)| {
                    let synced = broker.sync(&copy, pi_logins::now_ms());
                    (copy, synced)
                })
                .collect::<Vec<_>>()
        })
        .await;
        let mut alerts = lock(&self.pi_alerts);
        alerts.retain(|name, _| name == STORE_ALERT || known.contains(name));
        for (copy, synced) in done.unwrap_or_default() {
            Core::pi_note_synced(&mut alerts, &copy, synced);
        }
    }

    /// Looks at one agent's copy as it starts, wakes or resumes, so what a backend
    /// found there is told at once and not left for the next tick.
    pub(crate) async fn pi_sync_copy(&self, record: &AgentRecord, handle: &vibe_env::Handle) {
        if record.agent != AgentKind::Pi
            || !matches!(&handle.home, AgentHome::Pi(home) if home.config.is_some())
        {
            return;
        }
        let copy = pi_logins::vm_copy(&record.agent_ref, &handle.agent_dir);
        let broker = Broker::of(&self.layout);
        let (done, copy) =
            tokio::task::spawn_blocking(move || (broker.sync(&copy, pi_logins::now_ms()), copy))
                .await
                .unwrap_or_else(|e| {
                    let copy = pi_logins::vm_copy(&record.agent_ref, &handle.agent_dir);
                    (Err(std::io::Error::other(e.to_string())), copy)
                });
        Core::pi_note_synced(&mut lock(&self.pi_alerts), &copy, done);
    }

    /// All that needs the owner's eye about Pi's logins: what's wrong now, and
    /// what agents have done with one.
    async fn pi_alerts_now(&self) -> Vec<String> {
        let broker = Broker::of(&self.layout);
        let notices = tokio::task::spawn_blocking(move || broker.notices(pi_logins::now_ms()))
            .await
            .unwrap_or_default();
        lock(&self.pi_alerts)
            .values()
            .cloned()
            .chain(notices)
            .collect()
    }

    /// The looks at Pi's logins this host makes every [`PI_WATCH_EVERY`], and
    /// says if they, a `/login` on this host, or the layer's download changed
    /// what Settings shows (PRD: Who gets which credentials).
    pub async fn pi_watch(&self, seen: &mut Option<PiSeen>) {
        if self.info().os == "macos" {
            self.pi_sync_logins().await;
        }
        let (own, vms) = self.pi_logins_now().await;
        let now = PiSeen {
            own,
            vms,
            error: self.linux_images.as_ref().and_then(|w| w.pi_error()),
            alerts: self.pi_alerts_now().await,
        };
        let before = seen.as_ref();
        let new: Vec<&String> = now
            .alerts
            .iter()
            .filter(|a| !before.is_some_and(|s| s.alerts.contains(a)))
            .collect();
        // Everything new is said, whatever else came with it.
        for alert in &new {
            let mut event = self.event(EventKind::PiChanged);
            event.message = Some((*alert).clone());
            self.publish(event);
        }
        if new.is_empty() && before.is_some_and(|s| *s != now) {
            let mut event = self.event(EventKind::PiChanged);
            event.message = now.error.clone();
            self.publish(event);
        }
        *seen = Some(now);
    }

    /// The providers signed in to on this host, and for its `linux` agents. Off
    /// the runtime, as each is a file read.
    async fn pi_logins_now(&self) -> (Vec<String>, Vec<String>) {
        let (own, vms) = (self.own_pi_dir(), self.layout.linux_pi_logins_dir());
        tokio::task::spawn_blocking(move || (pi::logins_in(&own), pi::logins_in(&vms)))
            .await
            .unwrap_or_default()
    }

    /// Runs [`Core::pi_watch`] every `every`, for as long as the runtime runs.
    pub fn spawn_pi_watch(
        self: &Arc<Self>,
        every: std::time::Duration,
    ) -> tokio::task::JoinHandle<()> {
        let core = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(every);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut seen = None;
            loop {
                ticks.tick().await;
                core.pi_watch(&mut seen).await;
            }
        })
    }

    /// `pi.status`.
    pub async fn pi_status(&self, p: HostParams) -> Result<PiStatus, ErrorObject> {
        let host = self.host(p.host.as_deref())?.to_owned();
        let checks = Arc::clone(&self.checks);
        let native = tokio::task::spawn_blocking(move || checks.pi())
            .await
            .map_err(|e| error(ErrorCode::InternalError, e.to_string()))?
            .ok()
            .map(|(path, version)| PiBinary {
                managed: pi_release::is_ours(&path, &self.layout.data_dir()),
                path: path.to_string_lossy().into_owned(),
                version,
            });
        let (logins, vm_logins) = self.pi_logins_now().await;
        let alerts = self.pi_alerts_now().await;
        let vms = (self.info().os == "macos").then(|| PiVms {
            version: Images::new(self.layout.linux_dir()).pi().map(|(v, _)| v),
            logins: vm_logins,
            error: self.linux_images.as_ref().and_then(|w| w.pi_error()),
            alerts: alerts.clone(),
        });
        Ok(PiStatus {
            host,
            native,
            logins,
            vms,
        })
    }

    /// `pi.install`: installs it, and says how things stand then. It takes as
    /// long as the download does; one at a time. Either way a `pi.changed`
    /// event tells everyone else watching.
    pub async fn pi_install(&self, p: PiInstallParams) -> Result<PiStatus, ErrorObject> {
        self.host(p.host.as_deref())?;
        if p.target == PiTarget::Vm && self.info().os != "macos" {
            return Err(error(
                ErrorCode::Conflict,
                "only a Mac runs linux agents in VMs; install Pi for this host's own agents",
            ));
        }
        let Ok(_one_at_a_time) = self.pi_installing.try_lock() else {
            return Err(error(
                ErrorCode::Conflict,
                "Pi is being installed already; try again once that's done",
            ));
        };
        let (releases, data, home, linux) = (
            self.pi_releases.clone(),
            self.layout.data_dir(),
            self.layout.home().to_owned(),
            self.layout.linux_dir(),
        );
        let prune = !self.pi_agents_run();
        let done = tokio::task::spawn_blocking(move || {
            install(p.target, &releases, &data, &home, &linux, prune)
        })
        .await
        .map_err(|e| error(ErrorCode::InternalError, e.to_string()))?;
        *lock(&self.doctor_found) = None;
        let mut event = self.event(EventKind::PiChanged);
        match done {
            Ok(what) => {
                crate::log!("vibed: {what}");
                self.publish(event);
                self.pi_status(HostParams { host: None }).await
            }
            Err(why) => {
                crate::log!("vibed: can't install Pi: {why}");
                event.message = Some(why.clone());
                self.publish(event);
                Err(error(ErrorCode::CommandFailed, why))
            }
        }
    }

    /// A saved profile that runs Pi in a VM asks for the Pi layer, if there
    /// isn't one, in the background: its first agent shouldn't wait for the
    /// next daily check, and Macs that never run Pi never download it.
    pub(crate) fn pi_profile_saved(&self, profile: &Profile) {
        if profile.agent == AgentKind::Pi
            && profile.environment == Environment::Linux
            && let Some(watch) = &self.linux_images
        {
            watch.want_pi();
            if Images::new(self.layout.linux_dir()).pi().is_none() {
                watch.kick();
            }
        }
    }
}

/// Installs `target`, and says what it did.
fn install(
    target: PiTarget,
    releases: &str,
    data: &Path,
    home: &Path,
    linux: &Path,
    prune: bool,
) -> Result<String, String> {
    match target {
        PiTarget::Native => {
            let platform =
                pi_release::platform_here().ok_or("Pi publishes no build for this machine")?;
            let done =
                pi_release::install(releases, pi_release::PINNED, platform, data, home, prune)
                    .map_err(|e| e.to_string())?;
            Ok(format!(
                "Pi {} is at {}, linked from {}",
                done.version,
                done.binary.display(),
                done.link.display()
            ))
        }
        PiTarget::Vm => {
            let images = Images::new(linux);
            match images
                .update_pi(releases, pi_release::PINNED)
                .map_err(|e| e.to_string())?
            {
                Some(version) => Ok(format!(
                    "Pi {version} is the layer for linux Pi agents' next boot"
                )),
                None => Ok("the Pi layer for linux agents is up to date".to_owned()),
            }
        }
    }
}
