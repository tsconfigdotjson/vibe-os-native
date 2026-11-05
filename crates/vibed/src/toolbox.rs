//! The toolbox (PRD: The toolbox shell): `toolbox.status`, `toolbox.open`,
//! `toolbox.close`, `toolbox.update` and `toolbox.rebuild`, and the
//! supervisor that saves when the shell exits and runs `toolbox.toml`'s
//! `[update]` weekly. The work is `vibe_env::toolbox`; this is when it
//! runs, and what clients see of it.
//!
//! The shell is a session like an agent's, ref `toolbox`, held by a shim
//! on the guest's terminal, so `vibe toolbox` (and later the menu) attaches
//! to it as to any session, and detaching leaves it open; `toolbox.status`
//! counts the terminals attached. `toolbox.close` (Save & Close) hangs up
//! on the shell, as closing its Terminal window does, and saves once it
//! has exited. The supervisor notices when the shim has gone: if the
//! shell still runs (the shim died), a new shim holds it; if it exited,
//! the toolbox is closed and its new versions kept. One toolbox operation
//! runs at a time; another is refused as busy rather than queued. What the
//! last one did is kept in `~/VibeOS/agents/toolbox/state.json`, with when
//! the weekly update last ran.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
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

/// How long a login just approved in a browser waits for the toolbox to be free.
const TOOLBOX_WAIT: Duration = Duration::from_secs(10 * 60);

/// The shell's window title.
const TITLE: &str = "Linux toolbox";

/// How long a shell `toolbox.close` hung up on may take to exit, its shim
/// with it.
const HANGUP_PATIENCE: Duration = Duration::from_secs(15);

/// How long a shell whose hang-up failed may take to be seen gone: the
/// `kill` fails too if the shell had just exited on its own.
const JUST_EXITED: Duration = Duration::from_secs(2);

/// What `state.json` keeps.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Kept {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last: Option<ToolboxRun>,
    /// The last weekly or manual update: when, and whether it worked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    update: Option<Attempt>,
    /// The last time the template didn't move to a new base by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    moved: Option<NotMoved>,
}

/// A move of the template to a new base by itself that didn't happen.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotMoved {
    /// Unix seconds.
    pub at: i64,
    /// The base it was for.
    pub base: u32,
    /// The template it was from.
    pub template: u32,
    /// A sum of `toolbox.toml` as it was, or nothing if there's none.
    pub recipe: String,
    /// What moving would have lost, if that's why. Otherwise it failed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub held: Vec<String>,
}

/// Whether the template should move by itself to base `newest` now, from `template`
/// on `base`, with this `recipe`. Whatever came of the last try, once what it had to
/// go on has changed. Otherwise not if it would lose something: nobody wants minutes
/// of VM time for the same answer. And if it failed, a day on, as its weekly update is.
pub fn move_due(
    now: i64,
    base: u32,
    newest: u32,
    template: u32,
    recipe: &str,
    last: Option<&NotMoved>,
) -> bool {
    if base >= newest {
        return false;
    }
    let Some(last) = last.filter(|l| l.base == newest) else {
        return true;
    };
    if last.template != template || last.recipe != recipe {
        return true;
    }
    last.held.is_empty() && now - last.at >= RETRY_AFTER
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
/// update; `toolbox.rebuild` makes the first from the recipe.
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

/// Waits up to `patience` for the shim at `socket` to go.
async fn shim_gone(socket: &std::path::Path, patience: Duration) -> bool {
    let deadline = Instant::now() + patience;
    loop {
        if matches!(sessions::probe(socket).await, Probe::Gone) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
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

    /// Sets what the toolbox is doing. Says whether that changed.
    fn set_toolbox(&self, state: ToolboxState) -> bool {
        let was = std::mem::replace(&mut *lock(&self.toolbox_state), state);
        if was != state {
            crate::log!("vibed: the toolbox is {state} now, was {was}");
        }
        was != state
    }

    /// The toolbox is closed again without a run to report, as when an
    /// open fails or its shell is gone with nothing to save: subscribers
    /// are told, so none goes on showing a shell open that isn't (#46).
    fn toolbox_closed(&self) {
        if self.set_toolbox(ToolboxState::Closed) {
            self.publish(self.event(EventKind::ToolboxChanged));
        }
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
        // It's where it should be, by whichever way: nothing's waiting.
        if action == ToolboxAction::Rebuild && result.is_ok() {
            kept.moved = None;
        }
        // A weekly update adds a version, and changes nothing about what a move would
        // lose: it's no reason to try the whole of it again.
        if action == ToolboxAction::Update
            && let (Ok(saved), Some(moved)) = (&result, kept.moved.as_mut())
            && let Some(template) = saved.template
        {
            moved.template = template;
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
            return Ok(self.toolbox_status_of(None, None));
        };
        let versions = blocking(move || Ok(host.versions())).await?.ok();
        // Only a shim that answers knows: during a boot there's none yet.
        let open = *lock(&self.toolbox_state) == ToolboxState::Open;
        let clients = match open {
            true => match sessions::probe(&self.layout.session_socket(TOOLBOX_REF)).await {
                Probe::Running(status) => Some(status.clients.len() as u32),
                _ => None,
            },
            false => None,
        };
        Ok(self.toolbox_status_of(versions, clients))
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

    fn toolbox_status_of(&self, versions: Option<Versions>, clients: Option<u32>) -> ToolboxStatus {
        let Some(versions) = versions.filter(|_| self.toolbox.is_some()) else {
            return ToolboxStatus {
                state: ToolboxState::Unavailable,
                template: None,
                home: None,
                base: None,
                newest_base: None,
                rebuild_held: Vec::new(),
                last: None,
                clients: None,
            };
        };
        ToolboxStatus {
            state: *lock(&self.toolbox_state),
            template: versions.template,
            home: versions.home,
            base: versions.base,
            newest_base: versions.newest_base,
            rebuild_held: self.toolbox_held(&versions),
            last: self.kept().last,
            clients,
        }
    }

    /// Starts a shim on the toolbox shell's terminal.
    async fn hold_toolbox(&self, end: &TerminalEnd) -> Result<u32, String> {
        let socket = self.layout.session_socket(TOOLBOX_REF);
        // The record of the last shell, which a new one replaces.
        let _ = crate::agents::remove_session_files(&socket);
        let _ = fs::remove_file(self.toolbox_dir().join(shim::RECORD_FILE));
        let spec = match end {
            TerminalEnd::Pty {
                command,
                cwd,
                env,
                clean_env,
            } => SessionSpec {
                agent: TOOLBOX_REF.into(),
                command: command.iter().map(Into::into).collect(),
                cwd: cwd.clone(),
                env: env.iter().map(|(k, v)| (k.into(), v.into())).collect(),
                clean_env: *clean_env,
                size: None,
                vm: None,
            },
            TerminalEnd::Vm { socket } => SessionSpec {
                agent: TOOLBOX_REF.into(),
                command: Vec::new(),
                cwd: self.toolbox_dir(),
                env: Vec::new(),
                clean_env: false,
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
        // Open from the boot on, so it counts as awake against
        // max_awake_agents.
        self.set_toolbox(ToolboxState::Open);
        let end = {
            let host = Arc::clone(&host);
            blocking(move || host.open()).await
        };
        let end = match end {
            Ok(Ok(end)) => end,
            Ok(Err(e)) => {
                if !host.is_open() {
                    self.toolbox_closed();
                }
                return Err(failed(format!("can't open the toolbox: {e}")));
            }
            Err(e) => {
                self.toolbox_closed();
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
                    self.toolbox_closed();
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
                self.toolbox_closed();
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
        let result = blocking(move || host.rebuild(&recipe, false)).await;
        let result = match result {
            Ok(r) => r,
            Err(e) => {
                self.toolbox_closed();
                return Err(e);
            }
        };
        self.finish_run(ToolboxAction::Rebuild, result)
    }

    /// `toolbox.close`: **Save & Close**. Hangs up on the open shell, as
    /// closing its Terminal window does, waits for it to exit, and saves
    /// as an exit does; then says how the toolbox is. Closed already, it's
    /// left as it is. Whoever is attached sees the shell exit.
    pub async fn close_toolbox(&self, _: NoParams) -> Result<ToolboxStatus, ErrorObject> {
        let host = self.toolbox_host()?;
        // As for an open: a long update or rebuild is refused, and a brief
        // hold (an open, or a save already under way) is waited for.
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
        {
            let _op = self.toolbox_op.lock().await;
            if host.is_open() {
                self.hang_up_and_save(host).await?;
            } else {
                self.toolbox_closed();
            }
        }
        self.toolbox_status(NoParams {}).await
    }

    /// Ends the open shell and saves, under the toolbox's lock.
    async fn hang_up_and_save(&self, host: Arc<dyn ToolboxHost>) -> Result<(), ErrorObject> {
        let socket = self.layout.session_socket(TOOLBOX_REF);
        match sessions::probe(&socket).await {
            Probe::Running(status) => {
                let pid = status.child_pid;
                let h = Arc::clone(&host);
                if let Err(e) = blocking(move || h.hang_up(pid)).await? {
                    // A shell that had just exited can't be hung up on, and
                    // is saved like one that was.
                    if !shim_gone(&socket, JUST_EXITED).await {
                        return Err(failed(format!("can't close the toolbox shell: {e}")));
                    }
                } else if !shim_gone(&socket, HANGUP_PATIENCE).await {
                    return Err(conflict(
                        "the toolbox shell didn't exit when asked; exit it in Terminal to save",
                    ));
                }
                crate::log!("vibed: the toolbox shell has gone, to be saved");
            }
            Probe::Broken(why) => {
                return Err(failed(format!(
                    "the toolbox shell's session isn't answering: {why}"
                )));
            }
            // Nothing holds it (its shim died): nobody is attached, and
            // the save ends what still runs, as it does after an exit.
            Probe::Gone => {}
        }
        self.save_toolbox(host).await.map(drop)
    }

    /// Once the shell has exited: keeps its new versions.
    async fn save_exited_toolbox(&self, host: Arc<dyn ToolboxHost>) {
        let _op = self.toolbox_op.lock().await;
        if !host.is_open() {
            self.toolbox_closed();
            return;
        }
        // Opened again while this waited: its shell runs on.
        let socket = self.layout.session_socket(TOOLBOX_REF);
        if !matches!(sessions::probe(&socket).await, Probe::Gone) {
            return;
        }
        let _ = self.save_toolbox(host).await;
    }

    /// Keeps an open toolbox's new versions, under the toolbox's lock, its
    /// shell gone; and clears what its session left.
    async fn save_toolbox(&self, host: Arc<dyn ToolboxHost>) -> Result<ToolboxRun, ErrorObject> {
        self.set_toolbox(ToolboxState::Saving);
        let result = match blocking(move || host.close()).await {
            Ok(r) => r,
            Err(e) => Err(vibe_env::EnvError::new(e.message)),
        };
        let run = self.finish_run(ToolboxAction::Shell, result);
        let socket = self.layout.session_socket(TOOLBOX_REF);
        let _ = crate::agents::remove_session_files(&socket);
        let _ = fs::remove_file(self.toolbox_dir().join(shim::RECORD_FILE));
        run
    }

    /// One look at an open toolbox: a shell whose shim has gone is held
    /// again if it still runs, and closed if it exited.
    pub async fn check_toolbox(&self) {
        let Some(host) = self.toolbox.clone() else {
            return;
        };
        if !host.is_open() {
            // Said to be open, but nothing is: what the shell had is gone
            // (an unfinished toolbox thrown away, say), so it's closed. Not
            // while something holds the toolbox, as an open about to boot.
            if *lock(&self.toolbox_state) == ToolboxState::Open
                && let Ok(_op) = self.toolbox_op.try_lock()
                && !host.is_open()
                && *lock(&self.toolbox_state) == ToolboxState::Open
            {
                crate::log!("vibed: the toolbox was said to be open, but no shell is");
                self.toolbox_closed();
            }
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
            // Saved and closed by `toolbox.close` since the look above, or
            // held again by an open: a new shim here would boot it again, or
            // take the session from under the one that's there.
            if !host.is_open() || !matches!(sessions::probe(&socket).await, Probe::Gone) {
                return;
            }
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
        self.save_exited_toolbox(host).await;
    }

    /// A sum of `toolbox.toml` as it stands, or nothing if there's none.
    fn recipe_sum(&self) -> String {
        fs::read(self.layout.toolbox_config())
            .map(|bytes| crate::remote::sha256_hex(&bytes))
            .unwrap_or_default()
    }

    /// What moving the template would lose, if that's what it's waiting on.
    pub(crate) fn toolbox_held(&self, versions: &Versions) -> Vec<String> {
        match self.kept().moved {
            Some(moved)
                if Some(moved.base) == versions.newest_base
                    && Some(moved.template) == versions.template
                    && versions.base < versions.newest_base =>
            {
                moved.held
            }
            _ => Vec::new(),
        }
    }

    /// Moves the template to a new base image by itself, once one's here (PRD: Base
    /// image updates). New agents take it as soon as it's kept; agents that exist
    /// keep the images they have until their next fresh boot, as an image is never
    /// changed in place, so one asleep still wakes. What the owner installed by hand
    /// and the recipe doesn't cover isn't lost behind their back: then nothing moves,
    /// and Settings says what with one click.
    pub async fn rebuild_toolbox_if_due(&self) {
        let Some(host) = self.toolbox.clone() else {
            return;
        };
        let versions = host.versions();
        let (Some(template), Some(base), Some(newest)) =
            (versions.template, versions.base, versions.newest_base)
        else {
            return;
        };
        let recipe_sum = self.recipe_sum();
        if !move_due(
            self.now(),
            base,
            newest,
            template,
            &recipe_sum,
            self.kept().moved.as_ref(),
        ) || host.is_open()
            || host.has_unsaved()
        {
            return;
        }
        // Its download isn't done, or there's no room: it's no failure, and it's
        // looked at again soon.
        if self
            .linux_images
            .as_ref()
            .is_some_and(|w| w.check().state == vibe_core::model::CheckState::Working)
        {
            return;
        }
        if (self.free_disk)(&self.layout.linux_dir())
            .is_some_and(|free| free < crate::agents::MIN_FREE_DISK)
        {
            crate::log!("vibed: the template waits for disk to move to base image {newest}");
            return;
        }
        let Ok(_op) = self.take_toolbox() else {
            return;
        };
        let not_moved = |held: Vec<String>| NotMoved {
            at: self.now(),
            base: newest,
            template,
            recipe: recipe_sum.clone(),
            held,
        };
        let recipe = match recipe::load(&self.layout.toolbox_config()) {
            Ok(recipe) => recipe,
            Err(e) => {
                let why = vibe_env::EnvError::new(e.to_string());
                let _ = self.finish_run(ToolboxAction::Rebuild, Err(why));
                let mut kept = self.kept();
                kept.moved = Some(not_moved(Vec::new()));
                self.keep(&kept);
                return;
            }
        };
        crate::log!("vibed: moving the template to base image {newest}");
        self.set_toolbox(ToolboxState::Rebuilding);
        // Nobody asked for this, so it doesn't count among the agents awake: it isn't
        // to put an agent to sleep early, or keep one from waking.
        self.toolbox_moving.store(true, Ordering::SeqCst);
        let done = blocking(move || host.rebuild(&recipe, true)).await;
        self.toolbox_moving.store(false, Ordering::SeqCst);
        let done = match done {
            Ok(done) => done,
            Err(e) => {
                crate::log!("vibed: moving the template failed: {}", e.message);
                self.toolbox_closed();
                return;
            }
        };
        match done {
            Ok(saved) if saved.template.is_none() => {
                crate::log!(
                    "vibed: the template stays on base image {base}: moving would lose {}",
                    saved.uncovered.join(", ")
                );
                let mut kept = self.kept();
                kept.moved = Some(not_moved(saved.uncovered));
                // What failed before isn't what's in the way now.
                if kept
                    .last
                    .as_ref()
                    .is_some_and(|l| l.action == ToolboxAction::Rebuild && l.error.is_some())
                {
                    kept.last = None;
                }
                self.keep(&kept);
                self.toolbox_closed();
                self.toolbox_event(format!(
                    "The template stays on its base image for now: moving it would lose {}.",
                    kept.moved
                        .as_ref()
                        .map(|m| m.held.join(", "))
                        .unwrap_or_default()
                ));
            }
            done => {
                let failed = done.is_err();
                let _ = self.finish_run(ToolboxAction::Rebuild, done);
                if failed {
                    let mut kept = self.kept();
                    kept.moved = Some(not_moved(Vec::new()));
                    self.keep(&kept);
                }
            }
        }
    }

    /// The GitHub login a browser sign-in left at `scratch`, a `gh` config directory of
    /// its own.
    pub(crate) async fn toolbox_github_token(
        &self,
        gh: &std::ffi::OsStr,
        scratch: &std::path::Path,
    ) -> Result<String, String> {
        let out = tokio::process::Command::new(gh)
            .args(["auth", "token", "--hostname", "github.com"])
            .env("GH_CONFIG_DIR", scratch)
            .env("HOME", self.layout.home())
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|e| format!("can't run gh: {e}"))?;
        if !out.status.success() {
            return Err("gh signed in, and won't say with what".to_owned());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// Takes that login on into the toolbox's Linux home, for new linux agents. A shell
    /// opened meanwhile is waited for, for a while: a login just approved in a browser
    /// isn't to be thrown away for that, nor the shell's state disturbed.
    pub(crate) async fn toolbox_give_github(&self, token: String) -> Result<(), String> {
        let host = self
            .toolbox
            .clone()
            .ok_or("this host has no Linux toolbox")?;
        let deadline = Instant::now() + TOOLBOX_WAIT;
        let op = loop {
            // Looked at with the lock held: nothing opens or changes it now.
            if let Ok(op) = self.take_toolbox()
                && *lock(&self.toolbox_state) == ToolboxState::Closed
                && !host.is_open()
                && !host.has_unsaved()
            {
                break op;
            }
            if Instant::now() >= deadline {
                return Err(
                    "the toolbox stayed in use, so that login wasn't kept: sign in again once \
                     it's closed"
                        .to_owned(),
                );
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        self.set_toolbox(ToolboxState::Saving);
        let done = blocking(move || host.log_in_to_github(&token)).await;
        let result = match done {
            Ok(result) => {
                let message = result.as_ref().err().map(ToString::to_string);
                let _ = self.finish_run(ToolboxAction::Shell, result);
                message.map_or(Ok(()), Err)
            }
            Err(e) => {
                self.toolbox_closed();
                Err(e.message)
            }
        };
        drop(op);
        result
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
    /// toolbox is checked; and every `update_check`, whether a new base image or
    /// the weekly update is due.
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
                    core.rebuild_toolbox_if_due().await;
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
    fn a_failed_move_goes_again_at_once_if_what_it_had_to_go_on_has_changed() {
        let failed = NotMoved {
            at: 10 * DAY,
            base: 3,
            template: 5,
            recipe: "r".into(),
            held: Vec::new(),
        };
        assert!(!move_due(10 * DAY + 60, 2, 3, 5, "r", Some(&failed)));
        assert!(move_due(10 * DAY + 60, 2, 3, 5, "fixed", Some(&failed)));
        assert!(move_due(10 * DAY + 60, 2, 3, 6, "r", Some(&failed)));
    }

    #[test]
    fn the_template_moves_once_for_each_base_or_when_what_held_it_changes() {
        let due = |last: Option<&NotMoved>| move_due(10 * DAY, 2, 3, 5, "r", last);
        // Nothing's new: there's nothing to do.
        assert!(!move_due(10 * DAY, 3, 3, 5, "r", None));
        assert!(!move_due(10 * DAY, 4, 3, 5, "r", None));
        assert!(due(None));
        let held = NotMoved {
            at: 9 * DAY,
            base: 3,
            template: 5,
            recipe: "r".into(),
            held: vec!["apt: htop".into()],
        };
        // Held: not again, however long, till what could change the answer has.
        assert!(!move_due(100 * DAY, 2, 3, 5, "r", Some(&held)));
        assert!(
            move_due(10 * DAY, 2, 3, 6, "r", Some(&held)),
            "the template"
        );
        assert!(
            move_due(10 * DAY, 2, 3, 5, "new", Some(&held)),
            "the recipe"
        );
        assert!(
            move_due(10 * DAY, 2, 4, 5, "r", Some(&held)),
            "another base"
        );
        // Failed: a day on.
        let failed = NotMoved {
            held: Vec::new(),
            ..held
        };
        assert!(!move_due(9 * DAY + 60, 2, 3, 5, "r", Some(&failed)));
        assert!(move_due(10 * DAY, 2, 3, 5, "r", Some(&failed)));
    }

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
