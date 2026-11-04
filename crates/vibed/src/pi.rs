//! Pi on this host (PRD: Who gets which credentials; What's inside `linux`):
//! what's installed and which providers it's signed in to, for Settings, and
//! installing it, once a profile here runs it, with nobody asked. VibeOS doesn't
//! sign anyone in: `/login` is Pi's own, and only the providers' names are ever
//! read here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use vibe_core::model::{
    AgentKind, AgentState, Environment, EventKind, LOCAL_HOST, PiTarget, Profile,
};
use vibe_core::rpc::{
    ErrorCode, ErrorObject, HostParams, PiBinary, PiForgetParams, PiInstallParams, PiOffered,
    PiStatus, PiVms,
};
use vibe_core::store::AgentRecord;
use vibe_env::images::Images;
use vibe_env::pi_logins::{self, Broker, Copy, Synced};
use vibe_env::pi_release;
use vibe_env::{AgentHome, Backend, pi};

/// How often the logins are looked at: a `/login` or a refreshed token passed on
/// to every other copy, soon enough that nobody waits for it.
pub const PI_WATCH_EVERY: std::time::Duration = std::time::Duration::from_secs(3);

/// How long a Pi agent's start waits for an install under way. A download has an
/// end of its own, and this keeps a queue of starts from outlasting it.
const PI_WAIT: Duration = Duration::from_secs(20 * 60);

/// Why an install of Pi last failed and when, for each kind: for the doctor and
/// Settings, and for starts that were waiting on it.
#[derive(Debug, Default)]
pub struct PiFailures {
    pub(crate) native: Option<(String, Instant)>,
    pub(crate) vm: Option<(String, Instant)>,
}

/// What was last said of Pi, to say so when it changes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PiSeen {
    own: Vec<String>,
    vms: Vec<String>,
    error: Option<String>,
    alerts: Vec<String>,
}

use crate::core::{Core, error, lock};
use crate::doctor::PiWanted;

/// What one look at the copies found: each copy's, and what's wrong with the store.
type Look = (Vec<(Copy, std::io::Result<Synced>)>, Option<std::io::Error>);

/// How many alerts there can be at once, whatever agents try.
const MAX_ALERTS: usize = 32;

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
                let running = pi_copy_in_use(a.state, self.is_busy(&a.agent_ref));
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
        let mut odd_name = false;
        match synced {
            Err(e) => {
                Core::pi_note_store(alerts, Some(&e));
                return;
            }
            Ok(Synced::Busy) => return,
            Ok(Synced::Gone) => {}
            Ok(Synced::Quarantined(why)) => trouble = Some(why),
            Ok(Synced::Done(report)) => {
                odd_name = report.odd_name;
                if !report.bad.is_empty() {
                    let bad: std::collections::BTreeSet<&String> = report.bad.values().collect();
                    let bad: Vec<&str> = bad.into_iter().map(String::as_str).collect();
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
        // What may be an attack says so; a custom provider's odd name needn't.
        let mut said = Vec::new();
        if let Some(why) = trouble {
            said.push(format!(
                "{why}, so that isn't shared with any other agent. Something in it may be hostile."
            ));
        }
        if odd_name {
            said.push(format!(
                "{}. It works there, and reaches no other agent.",
                pi_logins::ODD_NAME
            ));
        }
        if said.is_empty() {
            alerts.remove(&copy.name);
        } else {
            let alert = format!("{}: {}", copy.name, said.join(" "));
            if alerts.get(&copy.name) != Some(&alert) {
                crate::log!("vibed: Pi's login: {alert}");
                alerts.insert(copy.name.clone(), alert);
            }
        }
    }

    /// Keeps the alert about the store as it stands: `trouble` if there is any.
    fn pi_note_store(alerts: &mut BTreeMap<String, String>, trouble: Option<&std::io::Error>) {
        match trouble {
            Some(e) => {
                let alert = format!("Pi's logins can't be kept in step: {e}");
                if alerts.get(STORE_ALERT) != Some(&alert) {
                    crate::log!("vibed: {alert}");
                    alerts.insert(STORE_ALERT.to_owned(), alert);
                }
            }
            None => {
                alerts.remove(STORE_ALERT);
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
            let done = copies
                .into_iter()
                .filter(|(_, in_use)| *in_use)
                .map(|(copy, _)| {
                    let synced = broker.sync(&copy, pi_logins::now_ms());
                    (copy, synced)
                })
                .collect::<Vec<_>>();
            // As it stands after all of them, whichever copy looked last, with
            // or without any copy to look.
            (done, broker.read_store().err())
        })
        .await;
        Core::pi_note_look(&mut lock(&self.pi_alerts), &known, done);
    }

    /// Keeps the alerts as one look at the copies left them. If the look itself
    /// fell over, that's said, or logins would quietly stop being kept in step.
    fn pi_note_look(
        alerts: &mut BTreeMap<String, String>,
        known: &[String],
        look: Result<Look, tokio::task::JoinError>,
    ) {
        let (done, store) = match look {
            Ok(look) => look,
            Err(e) => {
                let why = std::io::Error::other(format!("the look at them failed: {e}"));
                Core::pi_note_store(alerts, Some(&why));
                return;
            }
        };
        alerts.retain(|name, _| name == STORE_ALERT || known.contains(name));
        for (copy, synced) in done {
            Core::pi_note_synced(alerts, &copy, synced);
        }
        Core::pi_note_store(alerts, store.as_ref());
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

    /// All that needs the owner's eye about Pi's logins: what's wrong now, what's
    /// wrong with what the broker keeps, and what agents have done with one. Part
    /// of it can have come from a guest, so it's made safe to print here, for
    /// every client, and there's a limit to how much of it there is.
    async fn pi_alerts_now(&self) -> Vec<String> {
        let broker = Broker::of(&self.layout);
        let (problems, notices) = tokio::task::spawn_blocking(move || {
            (broker.problems(), broker.notices(pi_logins::now_ms()))
        })
        .await
        .unwrap_or_default();
        let wrong: Vec<String> = lock(&self.pi_alerts).values().cloned().collect();
        wrong
            .into_iter()
            .chain(problems)
            .chain(notices.into_iter().rev())
            .take(MAX_ALERTS)
            .map(|alert| vibe_core::model::clean_alert(&alert))
            .collect()
    }

    /// The logins agents offered, from what the broker kept.
    async fn pi_offered_now(&self) -> Vec<PiOffered> {
        let broker = Broker::of(&self.layout);
        tokio::task::spawn_blocking(move || broker.offered())
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(provider, agent)| PiOffered { provider, agent })
            .collect()
    }

    /// `pi.forget`.
    pub async fn pi_forget(&self, p: PiForgetParams) -> Result<PiStatus, ErrorObject> {
        self.host(p.host.as_deref())?;
        if self.info().os != "macos" {
            return Err(error(
                ErrorCode::Conflict,
                "only a Mac keeps a login for linux agents; run `pi` here and use /logout",
            ));
        }
        if p.providers.is_empty() || !p.providers.iter().all(|n| pi::provider_name_ok(n)) {
            return Err(error(
                ErrorCode::InvalidParams,
                "give the providers to forget",
            ));
        }
        let broker = Broker::of(&self.layout);
        let names = p.providers.clone();
        let gone = tokio::task::spawn_blocking(move || broker.forget(&names, pi_logins::now_ms()))
            .await
            .map_err(|e| error(ErrorCode::InternalError, e.to_string()))?
            .map_err(|e| error(ErrorCode::CommandFailed, e.to_string()))?;
        if !gone.is_empty() {
            crate::log!("vibed: forgot Pi's login for {}", gone.join(", "));
        }
        self.publish(self.event(EventKind::PiChanged));
        self.pi_status(HostParams { host: None }).await
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
        let offered = self.pi_offered_now().await;
        let failures = {
            let failures = lock(&self.pi_install_failures);
            (
                failures.native.as_ref().map(|(why, _)| why.clone()),
                failures.vm.as_ref().map(|(why, _)| why.clone()),
            )
        };
        let vms = (self.info().os == "macos").then(|| PiVms {
            version: Images::new(self.layout.linux_dir()).pi().map(|(v, _)| v),
            logins: vm_logins,
            error: self
                .linux_images
                .as_ref()
                .and_then(|w| w.pi_error())
                .or(failures.1),
            alerts,
            offered,
        });
        Ok(PiStatus {
            host,
            native,
            error: failures.0,
            logins,
            vms,
        })
    }

    /// `pi.install`: installs it now, and says how things stand then. It takes as
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
        // One under way is waited for, and this one then finds little left to do.
        let _one_at_a_time = self.pi_installing.lock().await;
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
        self.pi_install_done(p.target, done)?;
        self.pi_status(HostParams { host: None }).await
    }

    /// Says how an install went, to the log, to the doctor and to everyone
    /// watching.
    fn pi_install_done(
        &self,
        target: PiTarget,
        done: Result<String, String>,
    ) -> Result<(), ErrorObject> {
        *lock(&self.doctor_found) = None;
        let mut event = self.event(EventKind::PiChanged);
        let mut failures = lock(&self.pi_install_failures);
        let slot = match target {
            PiTarget::Native => &mut failures.native,
            PiTarget::Vm => &mut failures.vm,
        };
        let result = match done {
            Ok(what) => {
                crate::log!("vibed: {what}");
                *slot = None;
                Ok(())
            }
            Err(why) => {
                crate::log!("vibed: can't install Pi: {why}");
                *slot = Some((why.clone(), Instant::now()));
                event.message = Some(why.clone());
                Err(error(ErrorCode::CommandFailed, why))
            }
        };
        drop(failures);
        self.publish(event);
        result
    }

    /// The environments a profile here runs Pi in, with their backends.
    fn pi_profile_backends(&self) -> Vec<(Environment, Arc<dyn Backend>)> {
        let mut environments = Vec::new();
        {
            let store = self.store();
            for project in store.projects(LOCAL_HOST).unwrap_or_default() {
                for profile in store
                    .profiles(LOCAL_HOST, &project.name)
                    .unwrap_or_default()
                {
                    if profile.agent == AgentKind::Pi
                        && !environments.contains(&profile.environment)
                    {
                        environments.push(profile.environment);
                    }
                }
            }
        }
        environments
            .into_iter()
            .filter_map(|e| Some((e, self.backends.get(e)?)))
            .collect()
    }

    /// Whether Pi needs installing on this machine for those: it isn't there, or
    /// it's VibeOS's own and this build pins another. What the owner put there
    /// themselves is used as it is.
    async fn native_pi_needs_install(&self) -> bool {
        let backends = self.pi_profile_backends();
        let data = self.layout.data_dir();
        let link = self.layout.home().join(".local/bin/pi");
        tokio::task::spawn_blocking(move || {
            backends.iter().any(|(_, b)| match b.native_pi() {
                None => false,
                Some(Err(_)) => true,
                Some(Ok(path)) => pi_release::needs_update(&path, &link, &data, pi_release::PINNED),
            })
        })
        .await
        .unwrap_or(false)
    }

    /// Where Pi stands for the doctor. With nothing under way and nothing having
    /// failed, asking starts it.
    pub(crate) async fn pi_wanted(&self) -> PiWanted {
        let failed = {
            let failures = lock(&self.pi_install_failures);
            (
                failures.native.as_ref().map(|(why, _)| why.clone()),
                failures.vm.as_ref().map(|(why, _)| why.clone()),
            )
        };
        let backends = self.pi_profile_backends();
        if self.native_pi_needs_install().await {
            if self.pi_installing.try_lock().is_err() {
                return PiWanted::Installing;
            }
            if let Some(why) = failed.0 {
                return PiWanted::Failed(why);
            }
            self.in_background(|core| async move { core.repair_pi().await });
            return PiWanted::Installing;
        }
        // A VM's layer the same way: what's in its download or a start's.
        if backends.iter().any(|(e, _)| *e == Environment::Linux)
            && let Some(watch) = &self.linux_images
            && Images::new(self.layout.linux_dir()).pi().is_none()
            && let Some(why) = watch.pi_error().or(failed.1)
        {
            return PiWanted::Failed(why);
        }
        // One's there, and VibeOS won't touch it: only the doctor knows if it runs.
        let found = tokio::task::spawn_blocking(move || {
            backends
                .iter()
                .any(|(_, b)| matches!(b.native_pi(), Some(Ok(_))))
        })
        .await
        .unwrap_or(false);
        if found {
            PiWanted::Present
        } else {
            PiWanted::No
        }
    }

    /// One install of `target`, under `pi_installing`.
    async fn install_pi_now(&self, target: PiTarget) -> Result<(), String> {
        let (releases, data, home, linux) = (
            self.pi_releases.clone(),
            self.layout.data_dir(),
            self.layout.home().to_owned(),
            self.layout.linux_dir(),
        );
        let prune = !self.pi_agents_run();
        let done = tokio::task::spawn_blocking(move || {
            install(target, &releases, &data, &home, &linux, prune)
        })
        .await
        .map_err(|e| e.to_string())?;
        self.pi_install_done(target, done).map_err(|e| e.message)
    }

    /// What a profile that runs Pi asks of this host, whenever that's looked at: at
    /// each launch of `vibed`, every hour, and as one is saved. The layer for VMs,
    /// from the watch that keeps those; for native agents, the pinned build. Nobody
    /// is asked, and Macs and servers that never run Pi never download it.
    pub(crate) async fn repair_pi(&self) {
        let backends = self.pi_profile_backends();
        if let Some(watch) = &self.linux_images
            && backends.iter().any(|(e, _)| *e == Environment::Linux)
        {
            watch.want_pi();
            if Images::new(self.layout.linux_dir()).pi().is_none() {
                watch.kick();
            }
        }
        if !self.native_pi_needs_install().await {
            lock(&self.pi_install_failures).native = None;
            return;
        }
        // One under way is doing this already, and nothing queues up behind it.
        let Ok(_one_at_a_time) = self.pi_installing.try_lock() else {
            return;
        };
        crate::log!("vibed: a profile here runs Pi, so VibeOS is installing it");
        if let Err(why) = self.install_pi_now(PiTarget::Native).await {
            crate::log!("vibed: VibeOS tries again later: {why}");
        }
    }

    /// Before a Pi agent starts or comes back: Pi's there for it, waiting for it to
    /// be installed if it isn't, so a first agent just starts, a little late. Only
    /// if that can't be done does the agent fail, saying so. Starts waiting on one
    /// install share how it went, rather than each download again.
    pub(crate) async fn pi_ready(&self, backend: &Arc<dyn Backend>) -> Result<(), ErrorObject> {
        let needed = |b: Arc<dyn Backend>| async move {
            tokio::task::spawn_blocking(move || {
                if matches!(b.native_pi(), Some(Err(_))) {
                    Some(PiTarget::Native)
                } else if b.pi_layer_missing() {
                    Some(PiTarget::Vm)
                } else {
                    None
                }
            })
            .await
            .unwrap_or(None)
        };
        if needed(Arc::clone(backend)).await.is_none() {
            return Ok(());
        }
        let cant = |why: String| {
            error(
                ErrorCode::CommandFailed,
                format!(
                    "Pi isn't installed here yet, and VibeOS couldn't install it: {why}. It tries \
                     again every hour, and when a Pi agent next starts"
                ),
            )
        };
        let began = Instant::now();
        let Ok(_one_at_a_time) = tokio::time::timeout(PI_WAIT, self.pi_installing.lock()).await
        else {
            return Err(cant(
                "it's still being installed, so try again in a while".to_owned(),
            ));
        };
        let Some(target) = needed(Arc::clone(backend)).await else {
            return Ok(());
        };
        {
            let failures = lock(&self.pi_install_failures);
            let failed = match target {
                PiTarget::Native => &failures.native,
                PiTarget::Vm => &failures.vm,
            };
            if let Some((why, at)) = failed
                && *at >= began
            {
                return Err(cant(why.clone()));
            }
        }
        crate::log!("vibed: a Pi agent is starting, so VibeOS is installing Pi for it");
        self.install_pi_now(target).await.map_err(cant)
    }

    /// A profile that runs Pi was saved, so its first agent should find Pi there
    /// rather than wait for it: the layer for VMs is asked for, and the rest happens
    /// in the background.
    pub(crate) fn pi_profile_saved(&self, profile: &Profile) {
        if profile.agent != AgentKind::Pi {
            return;
        }
        if profile.environment == Environment::Linux
            && let Some(watch) = &self.linux_images
        {
            watch.want_pi();
            if Images::new(self.layout.linux_dir()).pi().is_none() {
                watch.kick();
            }
        }
        self.in_background(|core| async move { core.repair_pi().await });
    }
}

/// Whether an agent's copy of Pi's login is one to look at now. One whose VM can't
/// be running has nothing new to say. One being deleted, slept or woken (`busy`) is
/// looked at when that's done: a delete mustn't race with something writing beside
/// it.
fn pi_copy_in_use(state: AgentState, busy: bool) -> bool {
    !busy && !matches!(state, AgentState::Exited | AgentState::Asleep)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// F6: a look that falls over is told of, and a good one after it clears that.
    #[tokio::test]
    async fn a_look_that_falls_over_is_told_of() {
        let panicked = tokio::spawn(async { panic!("the look") })
            .await
            .unwrap_err();
        let mut alerts = BTreeMap::new();
        Core::pi_note_look(&mut alerts, &[], Err(panicked));
        let said = alerts.values().cloned().collect::<Vec<_>>().join(" ");
        assert!(said.contains("the look at them failed"), "{said}");
        Core::pi_note_look(&mut alerts, &[], Ok((Vec::new(), None)));
        assert!(alerts.is_empty(), "{alerts:?}");
    }

    /// F4: an odd name is told of without the alarm, and what may be hostile with.
    #[test]
    fn only_what_may_be_hostile_is_called_that() {
        let tmp = tempfile::tempdir().unwrap();
        let copy = pi_logins::vm_copy("quiet-otter", tmp.path());
        let mut alerts = BTreeMap::new();
        let odd = pi_logins::Report {
            odd_name: true,
            ..Default::default()
        };
        Core::pi_note_synced(&mut alerts, &copy, Ok(Synced::Done(odd)));
        let said = &alerts["quiet-otter"];
        assert!(said.contains("can't pass on"), "{said}");
        assert!(!said.contains("hostile"), "{said}");
        let mut bad = pi_logins::Report::default();
        bad.bad.insert(
            "openai".into(),
            "its login for openai would run a command".into(),
        );
        Core::pi_note_synced(&mut alerts, &copy, Ok(Synced::Done(bad)));
        assert!(alerts["quiet-otter"].contains("hostile"));
        // Put right, it's over.
        Core::pi_note_synced(&mut alerts, &copy, Ok(Synced::Done(Default::default())));
        assert!(alerts.is_empty());
    }

    #[test]
    fn a_copy_is_looked_at_only_while_its_agent_is_up_and_settled() {
        for state in [
            AgentState::Busy,
            AgentState::Idle,
            AgentState::Question,
            AgentState::Paused,
        ] {
            assert!(pi_copy_in_use(state, false), "{state}");
            // Being deleted, among other things.
            assert!(!pi_copy_in_use(state, true), "{state}");
        }
        for state in [AgentState::Asleep, AgentState::Exited] {
            assert!(!pi_copy_in_use(state, false), "{state}");
        }
    }
}
