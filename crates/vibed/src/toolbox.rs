//! The toolbox (PRD: The toolbox shell): `toolbox.status`, `toolbox.open`,
//! `toolbox.update` and `toolbox.rebuild`, and the supervisor that saves
//! when the shell exits and runs `toolbox.toml`'s `[update]` weekly. The
//! work is `vibe_env::toolbox`; this is when it runs, and what clients see
//! of it.
//!
//! The shell is a session like an agent's, ref `toolbox`, held by a shim
//! on the guest's terminal, so `vibe toolbox` (and later the menu) attaches
//! to it as to any session, and detaching leaves it open. The supervisor
//! notices when the shim has gone: if the shell still runs (the shim
//! died), a new shim holds it; if it exited, the toolbox is closed and its
//! new versions kept. One toolbox operation runs at a time; another is
//! refused as busy rather than queued. What the last one did is kept in
//! `~/VibeOS/agents/toolbox/state.json`, with when the weekly update last
//! ran.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use vibe_core::model::{EventKind, LOCAL_HOST, ToolboxAction, ToolboxState};
use vibe_core::paths::private_dir;
use vibe_core::rpc::{
    ErrorCode, ErrorObject, NoParams, SessionAttach, ToolboxLogins, ToolboxRun, ToolboxStatus,
};
use vibe_core::shim;
use vibe_core::time::rfc3339;
use vibe_core::toolbox as recipe;
use vibe_env::TerminalEnd;
use vibe_env::images::{Images, Kind};
use vibe_env::toolbox::{Saved, TOOLBOX_REF, ToolboxHost, Versions};

use crate::core::{Core, error, lock};
use crate::sessions::{self, Probe, SessionSpec};

/// The weekly update runs this long after the template was last made or
/// updated.
pub const UPDATE_EVERY: i64 = 7 * 24 * 60 * 60;

/// After a failed update, the next try comes this much later.
pub const RETRY_AFTER: i64 = 24 * 60 * 60;

/// How often the supervisor asks whether the weekly update is due.
pub const UPDATE_CHECK_EVERY: Duration = Duration::from_secs(60 * 60);

/// The shell's window title.
const TITLE: &str = "Linux toolbox";

/// What `state.json` keeps.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Kept {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last: Option<ToolboxRun>,
    /// The last weekly or manual update: when, and whether it worked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    update: Option<Attempt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    /// Unix seconds.
    pub at: i64,
    pub ok: bool,
}

/// Whether the weekly update is due at `now`: a week after the template
/// was last made (by the shell, an update or a rebuild) or updated, and a
/// day after an update that failed. With no template there's nothing to
/// update; **Rebuild template** makes the first from the recipe.
pub fn update_due(now: i64, template_made: Option<i64>, last: Option<Attempt>) -> bool {
    let Some(made) = template_made else {
        return false;
    };
    match last {
        Some(Attempt { at, ok: false }) if at >= made => now - at >= RETRY_AFTER,
        Some(Attempt { at, .. }) => now - at.max(made) >= UPDATE_EVERY,
        None => now - made >= UPDATE_EVERY,
    }
}

fn conflict(message: impl Into<String>) -> ErrorObject {
    error(ErrorCode::Conflict, message)
}

fn failed(message: impl Into<String>) -> ErrorObject {
    error(ErrorCode::CommandFailed, message)
}

/// Runs a toolbox call on a thread that may block.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> vibe_env::Result<T> + Send + 'static,
) -> Result<vibe_env::Result<T>, ErrorObject> {
    tokio::task::spawn_blocking(f).await.map_err(|e| {
        error(
            ErrorCode::InternalError,
            format!("Internal error: the toolbox call panicked: {e}"),
        )
    })
}

/// What the owner is told a run did.
fn describe(action: ToolboxAction, saved: &Saved) -> String {
    let versions = match (saved.template, saved.home) {
        (Some(t), Some(h)) => format!("template {t} and Linux home {h}"),
        (Some(t), None) => format!("template {t}"),
        (None, Some(h)) => format!("Linux home {h}"),
        (None, None) => "nothing".into(),
    };
    let mut text = match action {
        ToolboxAction::Shell => format!("The toolbox saved {versions}"),
        ToolboxAction::Update => format!("The update saved {versions}"),
        ToolboxAction::Rebuild => format!("The rebuild saved {versions}"),
    };
    if !saved.uncovered.is_empty() {
        text.push_str(&format!(
            "; toolbox.toml doesn't cover {}",
            saved.uncovered.join(", ")
        ));
    }
    text
}

impl Core {
    fn toolbox_host(&self) -> Result<Arc<dyn ToolboxHost>, ErrorObject> {
        self.toolbox.clone().ok_or_else(|| {
            conflict(
                "this host has no Linux toolbox: on a Linux host, install tools on the server \
                 directly",
            )
        })
    }

    fn toolbox_dir(&self) -> PathBuf {
        self.layout.agent_dir(TOOLBOX_REF)
    }

    fn kept(&self) -> Kept {
        fs::read_to_string(self.toolbox_dir().join("state.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn keep(&self, kept: &Kept) {
        let dir = self.toolbox_dir();
        let path = dir.join("state.json");
        let tmp = dir.join("state.json.tmp");
        let written = private_dir(&dir)
            .and_then(|()| fs::write(&tmp, serde_json::to_string(kept).expect("it encodes")))
            .and_then(|()| fs::rename(&tmp, &path));
        if let Err(e) = written {
            crate::log!("vibed: can't keep {}: {e}", path.display());
        }
    }

    fn set_toolbox(&self, state: ToolboxState) {
        *lock(&self.toolbox_state) = state;
    }

    fn toolbox_event(&self, message: String) {
        crate::log!("vibed: {message}");
        let mut e = self.event(EventKind::ToolboxChanged);
        e.message = Some(message);
        self.publish(e);
    }

    /// Takes the toolbox for one operation, or says what it's busy with.
    fn take_toolbox(&self) -> Result<tokio::sync::MutexGuard<'_, ()>, ErrorObject> {
        self.toolbox_op.try_lock().map_err(|_| {
            let doing = match *lock(&self.toolbox_state) {
                ToolboxState::Saving => "saving the toolbox",
                ToolboxState::Updating => "updating the template",
                ToolboxState::Rebuilding => "rebuilding the template",
                _ => "opening the toolbox",
            };
            conflict(format!(
                "the toolbox is busy {doing}; try again when it's done"
            ))
        })
    }

    /// Records how a run went, and tells subscribers.
    fn finish_run(
        &self,
        action: ToolboxAction,
        result: vibe_env::Result<Saved>,
    ) -> Result<ToolboxRun, ErrorObject> {
        let at = self.now();
        let (run, message) = match &result {
            Ok(saved) => (
                ToolboxRun {
                    action,
                    at: rfc3339(at),
                    template: saved.template,
                    uncovered: saved.uncovered.clone(),
                    error: None,
                },
                describe(action, saved),
            ),
            Err(e) => (
                ToolboxRun {
                    action,
                    at: rfc3339(at),
                    template: None,
                    uncovered: Vec::new(),
                    error: Some(e.to_string()),
                },
                match action {
                    ToolboxAction::Shell => format!("The toolbox couldn't save: {e}"),
                    ToolboxAction::Update => format!("The template's update failed: {e}"),
                    ToolboxAction::Rebuild => format!("Rebuilding the template failed: {e}"),
                },
            ),
        };
        let mut kept = self.kept();
        kept.last = Some(run.clone());
        if action == ToolboxAction::Update {
            kept.update = Some(Attempt {
                at,
                ok: result.is_ok(),
            });
        }
        self.keep(&kept);
        self.set_toolbox(ToolboxState::Closed);
        self.toolbox_event(message);
        match result {
            Ok(_) => Ok(run),
            Err(e) => Err(failed(e.to_string())),
        }
    }

    /// `toolbox.status`. The versions are read off the runtime's threads,
    /// as every other handler's file work is.
    pub async fn toolbox_status(&self, _: NoParams) -> Result<ToolboxStatus, ErrorObject> {
        let Some(host) = self.toolbox.clone() else {
            return Ok(self.toolbox_status_of(None));
        };
        let versions = blocking(move || Ok(host.versions())).await?.ok();
        Ok(self.toolbox_status_of(versions))
    }

    /// `toolbox.logins`: what the newest Linux home holds, from the note
    /// kept beside it, and whether linux agents have a Claude login. Boots
    /// nothing, and works while the toolbox is busy.
    pub async fn toolbox_logins(&self, _: NoParams) -> Result<ToolboxLogins, ErrorObject> {
        let host = self.toolbox_host()?;
        blocking(move || Ok(host.logins()))
            .await?
            .map_err(|e| error(ErrorCode::InternalError, e.to_string()))
    }

    /// The status as it is now, without leaving the current thread.
    pub fn toolbox_status_now(&self) -> ToolboxStatus {
        let versions = self.toolbox.as_ref().map(|h| h.versions());
        self.toolbox_status_of(versions)
    }

    fn toolbox_status_of(&self, versions: Option<Versions>) -> ToolboxStatus {
        let Some(versions) = versions.filter(|_| self.toolbox.is_some()) else {
            return ToolboxStatus {
                state: ToolboxState::Unavailable,
                template: None,
                home: None,
                base: None,
                newest_base: None,
                last: None,
            };
        };
        ToolboxStatus {
            state: *lock(&self.toolbox_state),
            template: versions.template,
            home: versions.home,
            base: versions.base,
            newest_base: versions.newest_base,
            last: self.kept().last,
        }
    }

    /// Starts a shim on the toolbox shell's terminal.
    async fn hold_toolbox(&self, end: &TerminalEnd) -> Result<u32, String> {
        let socket = self.layout.session_socket(TOOLBOX_REF);
        // The record of the last shell, which a new one replaces.
        let _ = crate::agents::remove_session_files(&socket);
        let _ = fs::remove_file(self.toolbox_dir().join(shim::RECORD_FILE));
        let spec = match end {
            TerminalEnd::Pty { command, cwd, env } => SessionSpec {
                agent: TOOLBOX_REF.into(),
                command: command.iter().map(Into::into).collect(),
                cwd: cwd.clone(),
                env: env.iter().map(|(k, v)| (k.into(), v.into())).collect(),
                size: None,
                vm: None,
            },
            TerminalEnd::Vm { socket } => SessionSpec {
                agent: TOOLBOX_REF.into(),
                command: Vec::new(),
                cwd: self.toolbox_dir(),
                env: Vec::new(),
                size: None,
                vm: Some(socket.clone()),
            },
        };
        sessions::spawn_shim(&self.shim, &self.layout, &spec).await
    }

    /// `toolbox.open`.
    pub async fn open_toolbox(&self, _: NoParams) -> Result<SessionAttach, ErrorObject> {
        let host = self.toolbox_host()?;
        // An update or a rebuild can take many minutes: refused, not
        // queued. Anything else that holds the toolbox (vibed's tidying at
        // start, a save, another open) is brief, and the open waits for it.
        let doing = match *lock(&self.toolbox_state) {
            ToolboxState::Updating => Some("updating"),
            ToolboxState::Rebuilding => Some("rebuilding"),
            _ => None,
        };
        if let Some(doing) = doing {
            return Err(conflict(format!(
                "the toolbox is busy {doing} the template; try again when it's done"
            )));
        }
        let _op = self.toolbox_op.lock().await;
        let socket = self.layout.session_socket(TOOLBOX_REF);
        let attach = |ttys| SessionAttach {
            host: LOCAL_HOST.into(),
            agent: TOOLBOX_REF.into(),
            title: TITLE.into(),
            socket: socket.to_string_lossy().into_owned(),
            ttys,
        };
        // One toolbox at a time: an open one is attached to again.
        if let Probe::Running(status) = sessions::probe(&socket).await {
            return Ok(attach(sessions::ttys(&status)));
        }
        let was_open = host.is_open();
        // Open from the boot on, so admission counts its VM's memory.
        self.set_toolbox(ToolboxState::Open);
        let end = {
            let host = Arc::clone(&host);
            blocking(move || host.open()).await
        };
        let end = match end {
            Ok(Ok(end)) => end,
            Ok(Err(e)) => {
                if !host.is_open() {
                    self.set_toolbox(ToolboxState::Closed);
                }
                return Err(failed(format!("can't open the toolbox: {e}")));
            }
            Err(e) => {
                self.set_toolbox(ToolboxState::Closed);
                return Err(e);
            }
        };
        match self.hold_toolbox(&end).await {
            Ok(pid) => crate::log!("vibed: the toolbox shell's session started (shim pid {pid})"),
            Err(why) => {
                // A shell that was open before stays for the supervisor;
                // one just booted for this is kept unsaved.
                if !was_open {
                    let host = Arc::clone(&host);
                    let _ = blocking(move || host.discard()).await;
                    self.set_toolbox(ToolboxState::Closed);
                }
                return Err(failed(format!("can't hold the toolbox shell: {why}")));
            }
        }
        let ttys = match sessions::probe(&socket).await {
            Probe::Running(status) => sessions::ttys(&status),
            _ => Vec::new(),
        };
        if !was_open {
            self.toolbox_event(
                "The Linux toolbox is open: exit its shell to save the template and Linux home"
                    .into(),
            );
        }
        Ok(attach(ttys))
    }

    /// `toolbox.update`, and the weekly update.
    pub async fn update_toolbox(&self, _: NoParams) -> Result<ToolboxRun, ErrorObject> {
        let host = self.toolbox_host()?;
        let _op = self.take_toolbox()?;
        if host.is_open() {
            return Err(conflict(
                "the toolbox shell is open: exit it first, and the update can run",
            ));
        }
        let recipe =
            recipe::load(&self.layout.toolbox_config()).map_err(|e| failed(e.to_string()))?;
        if recipe.update.is_empty() {
            return Err(failed(format!(
                "{} has no [update] commands to run",
                self.layout.toolbox_config().display()
            )));
        }
        self.set_toolbox(ToolboxState::Updating);
        let result = blocking(move || host.update(&recipe.update)).await;
        let result = match result {
            Ok(r) => r,
            Err(e) => {
                self.set_toolbox(ToolboxState::Closed);
                return Err(e);
            }
        };
        self.finish_run(ToolboxAction::Update, result)
    }

    /// `toolbox.rebuild`.
    pub async fn rebuild_toolbox(&self, _: NoParams) -> Result<ToolboxRun, ErrorObject> {
        let host = self.toolbox_host()?;
        let _op = self.take_toolbox()?;
        if host.is_open() {
            return Err(conflict(
                "the toolbox shell is open: exit it first, and the rebuild can run",
            ));
        }
        let recipe =
            recipe::load(&self.layout.toolbox_config()).map_err(|e| failed(e.to_string()))?;
        self.set_toolbox(ToolboxState::Rebuilding);
        let result = blocking(move || host.rebuild(&recipe)).await;
        let result = match result {
            Ok(r) => r,
            Err(e) => {
                self.set_toolbox(ToolboxState::Closed);
                return Err(e);
            }
        };
        self.finish_run(ToolboxAction::Rebuild, result)
    }

    /// Once the shell has exited: keeps its new versions.
    async fn close_toolbox(&self, host: Arc<dyn ToolboxHost>) {
        let _op = self.toolbox_op.lock().await;
        if !host.is_open() {
            self.set_toolbox(ToolboxState::Closed);
            return;
        }
        // Opened again while this waited: its shell runs on.
        let socket = self.layout.session_socket(TOOLBOX_REF);
        if !matches!(sessions::probe(&socket).await, Probe::Gone) {
            return;
        }
        self.set_toolbox(ToolboxState::Saving);
        let result = match blocking(move || host.close()).await {
            Ok(r) => r,
            Err(e) => Err(vibe_env::EnvError::new(e.message)),
        };
        let _ = self.finish_run(ToolboxAction::Shell, result);
        let _ = crate::agents::remove_session_files(&socket);
        let _ = fs::remove_file(self.toolbox_dir().join(shim::RECORD_FILE));
    }

    /// One look at an open toolbox: a shell whose shim has gone is held
    /// again if it still runs, and closed if it exited.
    pub async fn check_toolbox(&self) {
        let Some(host) = self.toolbox.clone() else {
            return;
        };
        if !host.is_open() {
            return;
        }
        let socket = self.layout.session_socket(TOOLBOX_REF);
        if !matches!(sessions::probe(&socket).await, Probe::Gone) {
            return;
        }
        let running = {
            let host = Arc::clone(&host);
            blocking(move || Ok(host.shell_running())).await
        };
        if matches!(running, Ok(Ok(true))) {
            let Ok(_op) = self.toolbox_op.try_lock() else {
                return;
            };
            let end = {
                let host = Arc::clone(&host);
                blocking(move || host.open()).await
            };
            match end {
                Ok(Ok(end)) => match self.hold_toolbox(&end).await {
                    Ok(pid) => crate::log!(
                        "vibed: the toolbox shell's shim had gone; a new one holds it (pid {pid})"
                    ),
                    Err(why) => crate::log!("vibed: can't hold the toolbox shell again: {why}"),
                },
                Ok(Err(e)) => crate::log!("vibed: can't reach the toolbox shell again: {e}"),
                Err(e) => crate::log!("vibed: {}", e.message),
            }
            return;
        }
        self.close_toolbox(host).await;
    }

    /// Runs the weekly update if it's due.
    pub async fn update_toolbox_if_due(&self) {
        let Some(host) = self.toolbox.clone() else {
            return;
        };
        let has_commands = recipe::load(&self.layout.toolbox_config())
            .map(|r| !r.update.is_empty())
            .unwrap_or(false);
        if !has_commands || host.is_open() {
            return;
        }
        let made = Images::new(self.layout.linux_dir())
            .newest_of(Kind::Template)
            .and_then(|(_, path)| fs::metadata(path).ok()?.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        if !update_due(self.now(), made, self.kept().update) {
            return;
        }
        crate::log!("vibed: the template's weekly update is due");
        if let Err(e) = self.update_toolbox(NoParams {}).await
            && e.code != ErrorCode::CommandFailed.code()
        {
            crate::log!("vibed: the weekly update didn't run: {}", e.message);
        }
    }

    /// At start: an open toolbox is watched again, and what an unfinished
    /// update or rebuild left is thrown away. Then, every `poll`, the open
    /// toolbox is checked; and every `update_check`, whether the weekly
    /// update is due.
    pub fn spawn_toolbox(
        self: &Arc<Self>,
        poll: Duration,
        update_check: Duration,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let host = self.toolbox.clone()?;
        let core = Arc::clone(self);
        Some(tokio::spawn(async move {
            // Under the toolbox's lock, as every call that changes it is, so
            // an open that comes first is seen here and left alone, and one
            // that comes after waits for this.
            {
                let _op = core.toolbox_op.lock().await;
                if host.is_open() {
                    core.set_toolbox(ToolboxState::Open);
                } else {
                    let h = Arc::clone(&host);
                    if let Ok(Err(e)) = blocking(move || h.discard()).await {
                        crate::log!("vibed: can't clear what the toolbox left: {e}");
                    }
                }
            }
            let mut checked = None::<Instant>;
            let mut ticks = tokio::time::interval(poll);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticks.tick().await;
                core.check_toolbox().await;
                if checked.is_none_or(|t| t.elapsed() >= update_check) {
                    checked = Some(Instant::now());
                    core.update_toolbox_if_due().await;
                }
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 24 * 60 * 60;

    #[test]
    fn the_update_runs_weekly_and_a_day_after_a_failure() {
        let made = 1_000 * DAY;
        assert!(!update_due(made + 30 * DAY, None, None), "no template yet");
        assert!(!update_due(made + 6 * DAY, Some(made), None));
        assert!(update_due(made + 7 * DAY, Some(made), None));
        let ok = |at| Some(Attempt { at, ok: true });
        let bad = |at| Some(Attempt { at, ok: false });
        // A week after the last update...
        assert!(!update_due(made + 13 * DAY, Some(made), ok(made + 7 * DAY)));
        assert!(update_due(made + 14 * DAY, Some(made), ok(made + 7 * DAY)));
        // ...or after a newer template, from the shell or a rebuild.
        assert!(!update_due(
            made + 14 * DAY,
            Some(made + 10 * DAY),
            ok(made + 7 * DAY)
        ));
        // A failed one is tried again a day later.
        assert!(!update_due(
            made + 7 * DAY + 3600,
            Some(made),
            bad(made + 7 * DAY)
        ));
        assert!(update_due(made + 8 * DAY, Some(made), bad(made + 7 * DAY)));
        // A failure from before the newest template doesn't count.
        assert!(!update_due(
            made + 8 * DAY,
            Some(made + 5 * DAY),
            bad(made + 4 * DAY)
        ));
    }

    #[test]
    fn what_a_run_did_is_said_plainly() {
        let saved = Saved {
            template: Some(3),
            home: Some(2),
            uncovered: vec![],
        };
        assert_eq!(
            describe(ToolboxAction::Shell, &saved),
            "The toolbox saved template 3 and Linux home 2"
        );
        let rebuilt = Saved {
            template: Some(4),
            home: None,
            uncovered: vec!["apt: htop".into(), "/opt/sdk".into()],
        };
        assert_eq!(
            describe(ToolboxAction::Rebuild, &rebuilt),
            "The rebuild saved template 4; toolbox.toml doesn't cover apt: htop, /opt/sdk"
        );
    }
}
