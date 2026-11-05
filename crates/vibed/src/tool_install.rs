//! What a host's agents need and don't have, installed for nobody to have to
//! (PRD: Key user flows, Install and setup): `gh` and Claude Code from their
//! publishers' releases, with no root, and `git` from the server's package manager,
//! with `sudo -n` or, failing that, one button on the Mac.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use vibe_core::model::{CheckId, CheckState, EventKind, GhState};
use vibe_core::rpc::DoctorCheck;
use vibe_env::tools;

use crate::core::{Core, lock};

/// Installs git with whichever package manager a server has. `{sudo}` is how `sudo` is
/// called: with `-n` so it never asks, or plainly on the owner's own terminal.
pub const GIT_WITH_SUDO: &str = r#"if command -v apt-get >/dev/null 2>&1; then
  {sudo} env DEBIAN_FRONTEND=noninteractive apt-get install -y -q git </dev/null
elif command -v dnf >/dev/null 2>&1; then
  {sudo} dnf install -y -q git </dev/null
elif command -v yum >/dev/null 2>&1; then
  {sudo} yum install -y -q git </dev/null
elif command -v zypper >/dev/null 2>&1; then
  {sudo} zypper --non-interactive install git </dev/null
elif command -v pacman >/dev/null 2>&1; then
  {sudo} pacman -S --noconfirm git </dev/null
elif command -v apk >/dev/null 2>&1; then
  {sudo} apk add git </dev/null
else
  echo "This server has no package manager VibeOS knows, to install git with." >&2
  false
fi
"#;

/// Where they come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolReleases {
    pub gh: String,
    pub claude: String,
}

impl ToolReleases {
    /// The publishers', unless `VIBE_GH_RELEASES` or `VIBE_CLAUDE_RELEASES` names others
    /// (a `file://` directory, say).
    pub fn from_env() -> ToolReleases {
        let var = |name: &str, default: &str| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default.to_owned())
        };
        ToolReleases {
            gh: var("VIBE_GH_RELEASES", tools::GH_RELEASES),
            claude: var("VIBE_CLAUDE_RELEASES", vibe_env::images::CLAUDE_RELEASES),
        }
    }
}

/// How one tool's install is going.
#[derive(Debug, Default)]
pub struct ToolInstall {
    running: AtomicBool,
    error: std::sync::Mutex<Option<String>>,
}

impl ToolInstall {
    fn now(&self) -> ToolNow {
        ToolNow {
            installing: self.running.load(Ordering::SeqCst),
            error: lock(&self.error).clone(),
        }
    }
}

/// Both.
#[derive(Debug, Default)]
pub struct ToolInstalls {
    gh: ToolInstall,
    claude: ToolInstall,
    /// `sudo -n` was tried for git. A refused try is in the server's logs, and can
    /// mail its admin, so once a run is enough.
    git_sudo_tried: AtomicBool,
}

/// How a tool stands, for the doctor, when it isn't there.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ToolNow {
    pub installing: bool,
    pub error: Option<String>,
}

impl ToolNow {
    /// The doctor's row for a tool that isn't there.
    pub fn check(&self, id: CheckId, name: &str) -> DoctorCheck {
        self.check_or(id, name, &format!("{name} isn't installed"))
    }

    /// The same, with what was found.
    pub fn check_or(&self, id: CheckId, name: &str, found: &str) -> DoctorCheck {
        let row = |state, detail: String, hint: Option<&str>, fixable| DoctorCheck {
            id,
            state,
            detail,
            hint: hint.map(str::to_owned),
            fixable,
            progress: None,
        };
        match &self.error {
            _ if self.installing => row(
                CheckState::Working,
                format!("Installing {name}."),
                None,
                false,
            ),
            Some(why) => row(
                CheckState::Fail,
                format!("{name} can't be installed yet: {why}."),
                Some("VibeOS tries again every hour."),
                true,
            ),
            None => row(
                CheckState::Fail,
                format!("{}.", found.trim_end_matches('.')),
                Some("VibeOS installs it by itself."),
                true,
            ),
        }
    }
}

/// Both, for the doctor.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ToolsNow {
    pub gh: ToolNow,
    pub claude: ToolNow,
}

impl Core {
    pub(crate) async fn tools_now(&self) -> ToolsNow {
        ToolsNow {
            gh: self.tool_installs.gh.now(),
            claude: self.tool_installs.claude.now(),
        }
    }

    /// Installs what's missing, as far as it can be, with nobody asked. Quick when it's
    /// all there. Never over something of the owner's: if they have one, it's found,
    /// and this has nothing to do.
    pub(crate) async fn repair_tools(&self) {
        if !self.checks.installs_tools() {
            return;
        }
        let checks = Arc::clone(&self.checks);
        let (gh_missing, claude_missing, git_missing) = tokio::task::spawn_blocking(move || {
            (
                checks.find("gh").is_none(),
                checks.claude().is_err(),
                checks.git().is_err(),
            )
        })
        .await
        .unwrap_or((false, false, false));
        // One named for it is the owner's to look after.
        if gh_missing && self.gh == *"gh" {
            self.install_gh().await;
        }
        if claude_missing {
            self.install_claude().await;
        }
        if git_missing && self.info().os == "linux" {
            self.install_git().await;
        } else {
            self.git_needs_admin.store(false, Ordering::SeqCst);
        }
    }

    async fn install_gh(&self) {
        let install = &self.tool_installs.gh;
        if install.running.swap(true, Ordering::SeqCst) {
            return;
        }
        crate::log!("vibed: gh isn't here, so VibeOS is installing it");
        let (releases, root, home) = (
            self.tool_releases.gh.clone(),
            self.layout.data_dir(),
            self.layout.home().to_owned(),
        );
        let done = tokio::task::spawn_blocking(move || {
            tools::install_gh(&releases, &root, &home).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|done| done);
        self.tool_installed("gh", install, done);
        self.check_gh().await;
    }

    async fn install_claude(&self) {
        let install = &self.tool_installs.claude;
        if install.running.swap(true, Ordering::SeqCst) {
            return;
        }
        crate::log!("vibed: Claude Code isn't here, so VibeOS is installing it");
        let (releases, root, home) = (
            self.tool_releases.claude.clone(),
            self.layout.data_dir(),
            self.layout.home().to_owned(),
        );
        let done = tokio::task::spawn_blocking(move || {
            tools::install_claude(&releases, &root, &home)
                .map(|(_, binary)| binary)
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|done| done);
        self.tool_installed("Claude Code", install, done);
    }

    fn tool_installed(&self, name: &str, install: &ToolInstall, done: Result<PathBuf, String>) {
        match done {
            Ok(binary) => {
                crate::log!("vibed: {name} is at {}", binary.display());
                *lock(&install.error) = None;
            }
            Err(why) => {
                crate::log!("vibed: can't install {name} yet: {why}");
                *lock(&install.error) = Some(why);
            }
        }
        install.running.store(false, Ordering::SeqCst);
        *lock(&self.doctor_found) = None;
        self.publish(self.event(EventKind::GhStatus));
    }

    /// `git`: `sudo -n`, once a run, or else the button.
    async fn install_git(&self) {
        if self
            .tool_installs
            .git_sudo_tried
            .swap(true, Ordering::SeqCst)
        {
            return;
        }
        crate::log!("vibed: git isn't here, so VibeOS is trying to install it");
        let script = GIT_WITH_SUDO.replace("{sudo}", "sudo -n");
        let installed = tokio::time::timeout(
            std::time::Duration::from_secs(5 * 60),
            tokio::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .status(),
        )
        .await;
        let ok = matches!(installed, Ok(Ok(status)) if status.success());
        self.git_needs_admin.store(!ok, Ordering::SeqCst);
        *lock(&self.doctor_found) = None;
    }

    /// The doctor's Try Again, for a tool it can install. Says if that's what it was.
    pub(crate) async fn tool_try_again(&self, check: CheckId) -> bool {
        if !self.checks.installs_tools() {
            return false;
        }
        match check {
            CheckId::Gh if lock(&self.gh_status).state == GhState::NotInstalled => {
                self.install_gh().await;
                true
            }
            CheckId::Claude => {
                self.install_claude().await;
                true
            }
            _ => false,
        }
    }
}
