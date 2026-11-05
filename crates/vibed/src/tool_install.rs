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
/// called: with `-n` so it never asks, or plainly on the owner's own terminal. As root
/// there's nothing to ask, and a server may have no `sudo` at all.
pub const GIT_WITH_SUDO: &str = r#"if [ "$(id -u)" = 0 ]; then S=; else S="{sudo}"; fi
if command -v apt-get >/dev/null 2>&1; then
  # One dead repository fails an update, and needn't stop git from the rest.
  $S env DEBIAN_FRONTEND=noninteractive apt-get update -q </dev/null
  $S env DEBIAN_FRONTEND=noninteractive apt-get install -y -q git </dev/null
elif command -v dnf >/dev/null 2>&1; then
  $S dnf install -y -q git </dev/null
elif command -v yum >/dev/null 2>&1; then
  $S yum install -y -q git </dev/null
elif command -v zypper >/dev/null 2>&1; then
  $S zypper --non-interactive install git </dev/null
elif command -v pacman >/dev/null 2>&1; then
  $S pacman -S --noconfirm git </dev/null
elif command -v apk >/dev/null 2>&1; then
  $S apk add git </dev/null
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
            broken: false,
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

/// How a tool stands, for the doctor, when it doesn't answer.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ToolNow {
    pub installing: bool,
    pub error: Option<String>,
    /// One's there and doesn't run: it isn't VibeOS's to put another beside it.
    pub broken: bool,
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
        let found = found.trim_end_matches('.');
        if self.broken {
            return row(
                CheckState::Fail,
                format!("{found}."),
                Some(
                    "One's there and doesn't run, so VibeOS leaves it as it is: remove it or put it right, and VibeOS takes it from there.",
                ),
                false,
            );
        }
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
                format!("{found}."),
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
        let checks = Arc::clone(&self.checks);
        let found = tokio::task::spawn_blocking(move || checks.claude_found())
            .await
            .unwrap_or(false);
        let mut claude = self.tool_installs.claude.now();
        // Whether this is the owner's to see to, if it doesn't answer.
        claude.broken = found;
        ToolsNow {
            gh: self.tool_installs.gh.now(),
            claude,
        }
    }

    /// Installs what's missing, as far as it can be, with nobody asked. Quick when it's
    /// all there. Never over something of the owner's: if they have one, it's found,
    /// and this has nothing to do. VibeOS's own `gh` follows a new pin.
    pub(crate) async fn repair_tools(&self) {
        if !self.checks.installs_tools() {
            return;
        }
        let checks = Arc::clone(&self.checks);
        let (gh, claude_found, git_missing) = tokio::task::spawn_blocking(move || {
            (
                checks.find("gh"),
                checks.claude_found(),
                checks.git().is_err(),
            )
        })
        .await
        .unwrap_or((Some(PathBuf::new()), true, false));
        let (root, home) = (self.layout.data_dir(), self.layout.home().to_owned());
        let gh_due = gh
            .as_ref()
            .is_none_or(|found| tools::gh_needs_update(found, &root, &home));
        // One named for it is the owner's to look after.
        if gh_due && self.gh == *"gh" && self.begin(&self.tool_installs.gh) {
            self.install_gh().await;
        }
        if !claude_found && self.begin(&self.tool_installs.claude) {
            self.install_claude().await;
        }
        if git_missing && self.info().os == "linux" {
            self.install_git().await;
        } else {
            self.git_needs_admin.store(false, Ordering::SeqCst);
        }
    }

    /// Takes `install` on, unless it's under way.
    fn begin(&self, install: &ToolInstall) -> bool {
        !install.running.swap(true, Ordering::SeqCst)
    }

    async fn install_gh(&self) {
        crate::log!("vibed: gh isn't here as it should be, so VibeOS is installing it");
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
        self.tool_installed("gh", &self.tool_installs.gh, done);
        self.check_gh().await;
    }

    async fn install_claude(&self) {
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
        self.tool_installed("Claude Code", &self.tool_installs.claude, done);
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
        self.publish(self.event(EventKind::HostLink));
    }

    /// Whether `git` is missing and waiting for someone with `sudo`. It may have just
    /// been put right, by the button or by hand, so that's looked at.
    pub(crate) fn git_waits_for_admin(&self) -> bool {
        if !self.git_needs_admin.load(Ordering::SeqCst) {
            return false;
        }
        let path = std::env::var_os("PATH");
        if crate::core::find_tool("git", path.as_deref(), Some(self.layout.home())).is_some() {
            self.git_needs_admin.store(false, Ordering::SeqCst);
            return false;
        }
        true
    }

    /// The doctor's Try Again, for a tool it can install: in the background, as a
    /// download can outlast whoever asked. Says if that's what it was.
    pub(crate) async fn tool_try_again(&self, check: CheckId) -> bool {
        if !self.checks.installs_tools() {
            return false;
        }
        let in_background = self.me.get().is_some() && self.runtime.get().is_some();
        match check {
            CheckId::Gh if lock(&self.gh_status).state == GhState::NotInstalled => {
                if self.begin(&self.tool_installs.gh) {
                    if in_background {
                        self.in_background(|core| async move { core.install_gh().await });
                    } else {
                        self.install_gh().await;
                    }
                }
                true
            }
            CheckId::Claude => {
                let checks = Arc::clone(&self.checks);
                let found = tokio::task::spawn_blocking(move || checks.claude_found())
                    .await
                    .unwrap_or(true);
                if found {
                    return false;
                }
                if self.begin(&self.tool_installs.claude) {
                    if in_background {
                        self.in_background(|core| async move { core.install_claude().await });
                    } else {
                        self.install_claude().await;
                    }
                }
                true
            }
            _ => false,
        }
    }
}
