//! `vibe doctor` and the first run's checklist (PRD: Key user flows,
//! Install and setup): what this host needs for agents to work, as `vibed`
//! sees it. git; `gh`'s login, and git using `gh` for GitHub (`gh auth
//! setup-git`, which `vibed` runs itself); the `claude` macos agents run, and
//! its login; system sleep off, on a Mac; on a Linux host, `vibed` staying up
//! while the owner's logged out, which `vibed` also sets up itself; Tailscale,
//! which is optional; on a Mac, the Linux images; and Pi, optional too, and
//! installed by `vibed` once a profile runs it. The checks that look at the
//! machine go through [`HostChecks`], so tests can give fixed answers.
//! VibeOS.app adds its own permission to control Terminal, which only the app
//! can see.

use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use vibe_core::keychain;
use vibe_core::model::{CheckId, CheckState, GhState};
use vibe_core::rpc::{
    DoctorCheck, DoctorFixParams, DoctorReport, ErrorCode, ErrorObject, GhStatus,
};
use vibe_env::Run;

use crate::core::{Core, error, find_tool, lock};

/// How long one of the doctor's commands may take.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

/// The service Claude Code keeps its login under in the macOS Keychain.
pub const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// What the doctor asks of the machine. Each answer is `Err` with why,
/// as a phrase, when the thing isn't there.
pub trait HostChecks: Send + Sync + fmt::Debug {
    /// Where the program `name` is, as this host's agents would find it:
    /// on the login shell's `PATH`, then in `~/.local/bin`,
    /// `/opt/homebrew/bin` and `/usr/local/bin`. How `vibed` finds `gh`.
    fn find(&self, name: &str) -> Option<PathBuf>;
    /// `git --version`.
    fn git(&self) -> Result<String, String>;
    /// Whether git asks `gh` for github.com's credentials, as `gh auth
    /// setup-git` sets it up, or has a helper of the owner's own for it.
    fn git_uses_gh(&self) -> Result<bool, String>;
    /// Runs `<gh> auth setup-git`.
    fn setup_git(&self, gh: &OsStr) -> Result<(), String>;
    /// The `claude` this host's native agents run, and its version.
    fn claude(&self) -> Result<(PathBuf, String), String>;
    /// How that `claude` is logged in.
    fn claude_login(&self) -> Result<String, String>;
    /// The `pi` this host's native agents run, and its version.
    fn pi(&self) -> Result<(PathBuf, String), String>;
    /// Minutes of idleness before the system sleeps, `Some(0)` for never;
    /// `None` where there's nothing to check, as on a Linux host.
    fn system_sleep(&self) -> Result<Option<u32>, String>;
    /// Whether this user's services run while they're logged out; `None` where
    /// there's nothing to check, as on a Mac. Only a look at a file, so it can be
    /// asked often.
    fn linger(&self) -> Result<Option<bool>, String>;
    /// Turns that on, every way there is that needs no password. Fails with what
    /// was said if none works. What it tries that an admin may see in a log, it
    /// tries once a run.
    fn enable_linger(&self) -> Result<(), String>;
    /// Whether VibeOS may install what's missing here. Only for this user's own home:
    /// a `HOME` that isn't, as in a test, is no place to go downloading to.
    fn installs_tools(&self) -> bool;
}

/// Where systemd keeps who lingers: an empty file named for each user.
const LINGER_DIR: &str = "/var/lib/systemd/linger";

/// The user lingering is about, and the directory their file would be in.
#[derive(Debug, Clone)]
struct LingerFor {
    user: String,
    dir: PathBuf,
}

/// The real machine: `HOME`'s, with the `PATH` its native agents get (on a
/// Mac, the login shell's).
#[derive(Debug)]
pub struct Machine {
    home: PathBuf,
    /// `VIBE_CLAUDE`, if it names the `claude` agents run.
    claude: Option<PathBuf>,
    /// `VIBE_PI`, if it names the `pi` agents run.
    pi: Option<PathBuf>,
    path: Mutex<Option<String>>,
    /// Set by [`Machine::with_linger`], and otherwise found once.
    linger_for: OnceLock<Option<LingerFor>>,
    /// `sudo -n` was tried for lingering. A refused try is in the server's
    /// logs, and can mail its admin, so once a run is enough.
    sudo_tried: AtomicBool,
}

impl Machine {
    pub fn new(home: impl Into<PathBuf>) -> Machine {
        Machine {
            home: home.into(),
            claude: std::env::var_os("VIBE_CLAUDE")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            pi: std::env::var_os("VIBE_PI")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            path: Mutex::new(None),
            linger_for: OnceLock::new(),
            sudo_tried: AtomicBool::new(false),
        }
    }

    /// Looks for `user`'s lingering in `dir`, rather than as this machine's own
    /// user's.
    pub fn with_linger(self, user: impl Into<String>, dir: impl Into<PathBuf>) -> Machine {
        let _ = self.linger_for.set(Some(LingerFor {
            user: user.into(),
            dir: dir.into(),
        }));
        self
    }

    /// Whose lingering there is to look at: this user's on a Linux host with
    /// systemd. None on a Mac, or for a `vibed` whose `HOME` isn't its user's own,
    /// which is a test's, whose settings for the real user are none of its business.
    fn lingers(&self) -> Option<&LingerFor> {
        self.linger_for
            .get_or_init(|| {
                if !cfg!(target_os = "linux") {
                    return None;
                }
                let (user, home) = passwd_entry()?;
                (home == self.home).then(|| LingerFor {
                    user,
                    dir: PathBuf::from(LINGER_DIR),
                })
            })
            .as_ref()
    }

    /// Runs this `claude` rather than looking for one, as `VIBE_CLAUDE`
    /// does.
    pub fn with_claude(mut self, claude: impl Into<PathBuf>) -> Machine {
        self.claude = Some(claude.into());
        self
    }

    /// Searches this `PATH`, rather than the login shell's.
    pub fn with_path(self, path: impl Into<String>) -> Machine {
        *lock(&self.path) = Some(path.into());
        self
    }

    /// The `PATH` agents get: on a Mac, the owner's login shell's, asked
    /// once; elsewhere `vibed`'s, with `~/.local/bin`.
    fn path(&self) -> String {
        let mut path = lock(&self.path);
        if let Some(p) = &*path {
            return p.clone();
        }
        let found = if cfg!(target_os = "macos") {
            match vibe_env::macos::login_path(&self.home) {
                Ok(p) => p,
                // Not kept: the login shell may answer next time.
                Err(fallback) => return fallback,
            }
        } else {
            let mut p = std::env::var("PATH").unwrap_or_default();
            p.push(':');
            p.push_str(&self.home.join(".local/bin").to_string_lossy());
            p
        };
        *path = Some(found.clone());
        found
    }

    /// The first executable `name` on the agents' `PATH`, then in the
    /// usual places ([`find_tool`]).
    fn which(&self, name: &str) -> Option<PathBuf> {
        let path = self.path();
        find_tool(name, Some(OsStr::new(&path)), Some(&self.home))
    }

    fn run(&self, argv: &[&str]) -> Result<(bool, String), String> {
        let out = vibe_env::native::run(
            &Run {
                argv: argv.iter().map(|s| (*s).to_owned()).collect(),
                env: vec![
                    ("HOME".into(), self.home.to_string_lossy().into_owned()),
                    ("PATH".into(), self.path()),
                    ("GH_PROMPT_DISABLED".into(), "1".into()),
                    ("NO_COLOR".into(), "1".into()),
                ],
                cwd: self.home.clone(),
                timeout: COMMAND_TIMEOUT,
            },
            &[],
        )
        .map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&out.output).trim().to_owned();
        if out.timed_out {
            return Err(format!(
                "{} didn't finish in {}s",
                argv.join(" "),
                COMMAND_TIMEOUT.as_secs()
            ));
        }
        Ok((out.success(), text))
    }
}

impl Machine {
    /// The git agents run, unless it's only macOS's stand-in for it.
    fn git_program(&self) -> Result<PathBuf, String> {
        let git = self.which("git").ok_or("git isn't installed")?;
        // On a Mac without the Command Line Tools, /usr/bin/git is a stub
        // that offers to install them; asking it anything opens that offer.
        if cfg!(target_os = "macos") && git == Path::new("/usr/bin/git") {
            let (ok, _) = self.run(&["/usr/bin/xcode-select", "-p"])?;
            if !ok {
                return Err("git needs Apple's Command Line Tools, which aren't installed".into());
            }
        }
        Ok(git)
    }
}

impl HostChecks for Machine {
    fn find(&self, name: &str) -> Option<PathBuf> {
        self.which(name)
    }

    fn git(&self) -> Result<String, String> {
        let git = self.git_program()?;
        match self.run(&[&git.to_string_lossy(), "--version"])? {
            (true, text) => Ok(first_line(&text)),
            (false, text) => Err(format!("git --version failed: {}", first_line(&text))),
        }
    }

    fn git_uses_gh(&self) -> Result<bool, String> {
        let git = self.git_program()?;
        let git = git.to_string_lossy();
        let mut helpers = String::new();
        for key in ["credential.https://github.com.helper", "credential.helper"] {
            // Exit 1 is "not set", which is an answer too.
            let (_, text) = self.run(&[&git, "config", "--global", "--get-all", key])?;
            // A way of its own for GitHub alone is the owner's, and `gh auth setup-git`
            // would replace it, so it counts.
            if key.contains("github.com") && !text.trim().is_empty() && !uses_gh(&text) {
                return Ok(true);
            }
            helpers.push_str(&text);
            helpers.push('\n');
        }
        Ok(uses_gh(&helpers))
    }

    fn setup_git(&self, gh: &OsStr) -> Result<(), String> {
        match self.run(&[
            &gh.to_string_lossy(),
            "auth",
            "setup-git",
            "--hostname",
            "github.com",
        ])? {
            (true, _) => Ok(()),
            (false, text) => Err(format!("gh auth setup-git failed: {}", first_line(&text))),
        }
    }

    fn claude(&self) -> Result<(PathBuf, String), String> {
        let claude = match &self.claude {
            Some(c) => c.clone(),
            None => self
                .which("claude")
                .or_else(|| {
                    let legacy = self.home.join(".claude/local/claude");
                    legacy.is_file().then_some(legacy)
                })
                .ok_or("claude isn't on your login shell's PATH")?,
        };
        match self.run(&[&claude.to_string_lossy(), "--version"])? {
            (true, text) => Ok((claude, first_line(&text))),
            (false, text) => Err(format!(
                "{} --version failed: {}",
                claude.display(),
                first_line(&text)
            )),
        }
    }

    fn pi(&self) -> Result<(PathBuf, String), String> {
        let pi = match &self.pi {
            Some(p) => p.clone(),
            None => self
                .which("pi")
                .ok_or("Pi isn't on your login shell's PATH")?,
        };
        match self.run(&[&pi.to_string_lossy(), "--version"])? {
            (true, text) => Ok((pi, first_line(&text))),
            (false, text) => Err(format!(
                "{} --version failed: {}",
                pi.display(),
                first_line(&text)
            )),
        }
    }

    fn claude_login(&self) -> Result<String, String> {
        if cfg!(target_os = "macos") && keychain::login_open() {
            let (ok, _) = self.run(&[
                keychain::SECURITY,
                "find-generic-password",
                "-s",
                CLAUDE_KEYCHAIN_SERVICE,
            ])?;
            if ok {
                return Ok("Claude Code is signed in, with its login in your Keychain".into());
            }
        }
        if self.home.join(".claude/.credentials.json").is_file() {
            return Ok("Claude Code is signed in".into());
        }
        Err("Claude Code isn't signed in".into())
    }

    fn system_sleep(&self) -> Result<Option<u32>, String> {
        if !cfg!(target_os = "macos") {
            return Ok(None);
        }
        match self.run(&["/usr/bin/pmset", "-g"])? {
            (true, text) => system_sleep_minutes(&text)
                .map(Some)
                .ok_or_else(|| "pmset -g doesn't say when this Mac sleeps".into()),
            (false, text) => Err(format!("pmset -g failed: {}", first_line(&text))),
        }
    }

    fn linger(&self) -> Result<Option<bool>, String> {
        let Some(l) = self.lingers() else {
            return Ok(None);
        };
        // No logind, no lingering to want: such a host can't run vibed's service.
        if self.which("loginctl").is_none() {
            return Ok(None);
        }
        Ok(Some(l.dir.join(&l.user).exists()))
    }

    fn installs_tools(&self) -> bool {
        self.own_home()
    }

    fn enable_linger(&self) -> Result<(), String> {
        let Some(l) = self.lingers() else {
            return Ok(());
        };
        let loginctl = self
            .which("loginctl")
            .ok_or("this server has no loginctl")?
            .to_string_lossy()
            .into_owned();
        let (ok, said) = self.run(&[&loginctl, "enable-linger", &l.user])?;
        if ok {
            return Ok(());
        }
        // A server without polkit turns the user down. Passwordless sudo, for this
        // one command, is the least that does it.
        if !self.sudo_tried.swap(true, Ordering::SeqCst)
            && let Some(sudo) = self.which("sudo")
        {
            let (ok, _) = self.run(&[
                &sudo.to_string_lossy(),
                "-n",
                &loginctl,
                "enable-linger",
                &l.user,
            ])?;
            if ok {
                return Ok(());
            }
        }
        Err(format!(
            "loginctl enable-linger was refused: {}",
            first_line(&said)
        ))
    }
}

impl Machine {
    /// Whether this is this user's real home.
    fn own_home(&self) -> bool {
        passwd_entry().is_some_and(|(_, home)| home == self.home)
    }
}

/// This process's user's name and home, as the system has them.
fn passwd_entry() -> Option<(String, PathBuf)> {
    use std::os::unix::ffi::OsStrExt;
    let mut buffer = vec![0u8; 16 * 1024];
    // SAFETY: zeroed is a valid passwd, for getpwuid_r to fill; the buffer outlives
    // the strings it points them into, which are copied out before it goes.
    unsafe {
        let mut entry = std::mem::zeroed::<libc::passwd>();
        let mut found = std::ptr::null_mut();
        let failed = libc::getpwuid_r(
            libc::getuid(),
            &raw mut entry,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &raw mut found,
        );
        if failed != 0 || found.is_null() {
            return None;
        }
        let text = |p: *const libc::c_char| std::ffi::CStr::from_ptr(p).to_bytes().to_vec();
        let user = String::from_utf8(text(entry.pw_name)).ok()?;
        let home = PathBuf::from(OsStr::from_bytes(&text(entry.pw_dir)));
        Some((user, home))
    }
}

/// Fixed answers, for tests and the golden examples. `setup_git` makes
/// `git_uses_gh` true.
#[derive(Debug)]
pub struct FixedChecks {
    pub git: Result<String, String>,
    pub git_uses_gh: Mutex<Result<bool, String>>,
    pub claude: Result<(PathBuf, String), String>,
    pub claude_login: Result<String, String>,
    pub pi: Result<(PathBuf, String), String>,
    pub system_sleep: Result<Option<u32>, String>,
    pub linger: Mutex<Result<Option<bool>, String>>,
    /// What turning lingering on does: `Ok` turns it on.
    pub enable_linger: Result<(), String>,
    /// [`HostChecks::installs_tools`].
    pub installs_tools: bool,
}

impl HostChecks for FixedChecks {
    /// Nothing: the tools a test runs are given by their paths.
    fn find(&self, _: &str) -> Option<PathBuf> {
        None
    }
    fn git(&self) -> Result<String, String> {
        self.git.clone()
    }
    fn git_uses_gh(&self) -> Result<bool, String> {
        lock(&self.git_uses_gh).clone()
    }
    fn setup_git(&self, _: &OsStr) -> Result<(), String> {
        *lock(&self.git_uses_gh) = Ok(true);
        Ok(())
    }
    fn claude(&self) -> Result<(PathBuf, String), String> {
        self.claude.clone()
    }
    fn claude_login(&self) -> Result<String, String> {
        self.claude_login.clone()
    }
    fn pi(&self) -> Result<(PathBuf, String), String> {
        self.pi.clone()
    }
    fn system_sleep(&self) -> Result<Option<u32>, String> {
        self.system_sleep.clone()
    }
    fn linger(&self) -> Result<Option<bool>, String> {
        lock(&self.linger).clone()
    }
    fn installs_tools(&self) -> bool {
        self.installs_tools
    }
    fn enable_linger(&self) -> Result<(), String> {
        if self.enable_linger.is_ok() {
            *lock(&self.linger) = Ok(Some(true));
        }
        self.enable_linger.clone()
    }
}

/// Whether git's credential helpers, one per line, include `gh`'s.
pub fn uses_gh(helpers: &str) -> bool {
    helpers.lines().any(|l| {
        let l = l.trim();
        l.contains("gh auth git-credential") || l.contains("gh.exe auth git-credential")
    })
}

/// The minutes before idle system sleep in `pmset -g`'s report: its
/// ` sleep  N` line, where 0 is never.
pub fn system_sleep_minutes(pmset: &str) -> Option<u32> {
    pmset.lines().find_map(|l| {
        let mut words = l.split_whitespace();
        (words.next()? == "sleep")
            .then(|| words.next()?.parse().ok())
            .flatten()
    })
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_owned()
}

/// Commands a sentence may start with, kept in lowercase, as the menu
/// keeps them (swift/VibeOS's `commandNames`).
const COMMANDS: [&str; 8] = [
    "git", "gh", "claude", "pi", "vibe", "vibed", "pmset", "curl",
];

/// `s` as a sentence: capitalized unless it starts with a command's name,
/// with a full stop.
fn sentence(s: &str) -> String {
    let s = s.trim();
    let word: String = s
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '-')
        .collect();
    let mut out = match s.chars().next() {
        Some(_) if COMMANDS.contains(&word.as_str()) => s.to_owned(),
        Some(c) => c.to_uppercase().collect::<String>() + &s[c.len_utf8()..],
        None => String::new(),
    };
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

fn pass(id: CheckId, detail: impl AsRef<str>) -> DoctorCheck {
    DoctorCheck {
        id,
        state: CheckState::Pass,
        detail: sentence(detail.as_ref()),
        hint: None,
        fixable: false,
        progress: None,
    }
}

fn fail(id: CheckId, detail: impl AsRef<str>, hint: &str) -> DoctorCheck {
    DoctorCheck {
        id,
        state: CheckState::Fail,
        detail: sentence(detail.as_ref()),
        hint: Some(hint.to_owned()),
        fixable: false,
        progress: None,
    }
}

/// How long what the machine said is taken as still true: the welcome
/// asks again every few seconds while Linux downloads, and each time would
/// otherwise run git, claude, security and pmset again.
pub const FOUND_FOR: Duration = Duration::from_secs(15);

/// Whether git asks `gh` for GitHub's credentials, having set that up if not:
/// nobody's asked to, once `gh` is logged in.
pub(crate) fn git_through_gh(checks: &dyn HostChecks, gh: &OsStr) -> Result<bool, String> {
    if checks.git_uses_gh()? {
        return Ok(true);
    }
    checks.setup_git(gh)?;
    crate::log!("vibed: git now asks gh for GitHub's credentials (gh auth setup-git)");
    checks.git_uses_gh()
}

/// Whether this user's services run while they're logged out, having turned that
/// on if not: nobody's asked to. `Err` with why, if it's off and stays off.
pub(crate) fn lingering(checks: &dyn HostChecks) -> Result<Option<bool>, String> {
    if checks.linger()? != Some(false) {
        return checks.linger();
    }
    checks.enable_linger()?;
    crate::log!("vibed: lingering is on, so vibed runs while you're logged out");
    checks.linger()
}

/// Where Pi stands for this host's agents, as far as it's VibeOS's to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PiWanted {
    /// No profile here runs it, so nothing needs it.
    No,
    /// A profile runs it, and it's on its way.
    Installing,
    /// A profile runs it, and installing or updating it failed for this reason.
    Failed(String),
    /// A profile runs it, and one's there that VibeOS leaves alone.
    Present,
}

/// What the machine said, before any of it is put into checks.
#[derive(Debug, Clone)]
pub(crate) struct Found {
    git: Result<String, String>,
    git_uses_gh: Result<bool, String>,
    claude: Result<(PathBuf, String), String>,
    claude_login: Result<String, String>,
    pi: Result<(PathBuf, String), String>,
    system_sleep: Result<Option<u32>, String>,
    linger: Result<Option<bool>, String>,
}

/// What only a person can do about a login, and where the one click is. A host on its
/// own, with no Mac to click on, has its provider's own command: there's nothing else.
const SIGN_IN_HINT: &str = "One click in VibeOS: its setup on the Mac, or Settings → Hosts for a \
                            Linux host. A host with no Mac signs in with the tool's own login.";

/// The checks, in [`CheckId`]'s order, from what was found.
fn report(
    found: Found,
    gh: &GhStatus,
    tailscale: &[std::net::IpAddr],
    linux: Option<DoctorCheck>,
    mac: bool,
    pi_wanted: &PiWanted,
    tools: &crate::tool_install::ToolsNow,
) -> DoctorReport {
    let mut checks = Vec::new();
    let install_git = if mac {
        "It takes Apple's Command Line Tools, and VibeOS's setup has a button for them."
    } else {
        "VibeOS installs it by itself if it can, or Settings → Hosts has a button for it."
    };
    checks.push(match &found.git {
        Ok(version) => pass(
            CheckId::Git,
            format!(
                "git {} is installed",
                version.strip_prefix("git version ").unwrap_or(version)
            ),
        ),
        Err(why) => fail(CheckId::Git, why, install_git),
    });
    let gh_ok = gh.state == GhState::LoggedIn;
    checks.push(match gh.state {
        GhState::LoggedIn => pass(
            CheckId::Gh,
            match &gh.account {
                Some(a) => format!("GitHub's gh is logged in to github.com as {a}"),
                None => "GitHub's gh is logged in to github.com".to_owned(),
            },
        ),
        GhState::NotInstalled => tools.gh.check(CheckId::Gh, "GitHub's gh"),
        GhState::LoggedOut => fail(CheckId::Gh, "GitHub's gh isn't logged in", SIGN_IN_HINT),
        GhState::Unknown | GhState::Error => fail(
            CheckId::Gh,
            gh.detail.as_deref().unwrap_or("gh auth status failed"),
            "VibeOS looks again every hour, and whenever the doctor asks.",
        ),
    });
    // VibeOS's to set up, so it only needs the owner if that fails. The doctor has
    // tried by now, and tries again next time.
    let retry = "VibeOS sets this up itself, and tries again each time it checks.";
    checks.push(match (&found.git_uses_gh, gh_ok) {
        (Ok(true), _) => pass(CheckId::GhGit, "git asks gh for GitHub's credentials"),
        (_, false) => DoctorCheck {
            state: CheckState::Optional,
            hint: None,
            ..fail(
                CheckId::GhGit,
                "VibeOS has git use gh for GitHub as soon as gh is logged in",
                "",
            )
        },
        (Ok(false), true) => DoctorCheck {
            fixable: true,
            ..fail(
                CheckId::GhGit,
                "git doesn't ask gh for GitHub's credentials yet",
                retry,
            )
        },
        (Err(why), true) => match found.git {
            Ok(_) => DoctorCheck {
                fixable: true,
                ..fail(CheckId::GhGit, why, retry)
            },
            Err(_) => fail(CheckId::GhGit, why, install_git),
        },
    });
    checks.push(match &found.claude {
        Ok((path, version)) => pass(
            CheckId::Claude,
            format!(
                "Claude Code {} is at {}",
                version.split_whitespace().next().unwrap_or(version),
                path.display()
            ),
        ),
        Err(why) => tools.claude.check_or(CheckId::Claude, "Claude Code", why),
    });
    checks.push(match (&found.claude, &found.claude_login) {
        (Err(_), _) => DoctorCheck {
            state: CheckState::Optional,
            hint: None,
            ..fail(
                CheckId::ClaudeLogin,
                "Claude Code can sign in once it's installed",
                "",
            )
        },
        (Ok(_), Ok(how)) => pass(CheckId::ClaudeLogin, how),
        (Ok(_), Err(why)) => fail(CheckId::ClaudeLogin, why, SIGN_IN_HINT),
    });
    match &found.system_sleep {
        Ok(None) => {}
        Ok(Some(0)) => checks.push(pass(CheckId::Power, "This Mac never sleeps on its own")),
        Ok(Some(minutes)) => checks.push(fail(
            CheckId::Power,
            format!(
                "This Mac sleeps after {minutes} minute{} idle, and agents stop answering while it sleeps",
                if *minutes == 1 { "" } else { "s" }
            ),
            "In System Settings → Energy, turn on Prevent automatic sleeping when the display is \
             off (on a laptop, in Battery → Options, on power adapter). VibeOS's setup has a button \
             for it too.",
        )),
        Err(why) => checks.push(fail(
            CheckId::Power,
            why,
            "VibeOS looks again whenever the doctor asks.",
        )),
    }
    // VibeOS's to set up, and it has tried by now. What it can't do without a
    // password takes one button, on the Mac.
    let lingering_off = |why: String| DoctorCheck {
        fixable: true,
        ..fail(
            CheckId::Linger,
            why,
            "It takes someone with sudo here, once: Settings → Hosts on the Mac has a button that \
             asks for the password. VibeOS tries again by itself meanwhile. With no Mac linked, \
             ask whoever has sudo here to turn lingering on for you.",
        )
    };
    match &found.linger {
        Ok(None) => {}
        Ok(Some(true)) => checks.push(pass(
            CheckId::Linger,
            "vibed runs while you're logged out, and starts again after a reboot",
        )),
        Ok(Some(false)) => checks.push(lingering_off(
            "Lingering is off, so vibed stops when you log out of this server".to_owned(),
        )),
        Err(why) => checks.push(lingering_off(format!(
            "Lingering is off, so vibed stops when you log out of this server: {why}"
        ))),
    }
    checks.push(match tailscale.first() {
        Some(ip) => pass(
            CheckId::Tailscale,
            format!("Tailscale is up, at {ip}; Settings can serve MCP there too"),
        ),
        None => DoctorCheck {
            id: CheckId::Tailscale,
            state: CheckState::Optional,
            detail: "Tailscale isn't running. It's optional: it lets your other machines reach \
                     MCP here."
                .into(),
            hint: Some("Install Tailscale from tailscale.com and log in, if you want it.".into()),
            fixable: false,
            progress: None,
        },
    });
    checks.extend(linux);
    checks.push(match (&found.pi, pi_wanted) {
        // Whether or not an older one is there to go on with.
        (_, PiWanted::Failed(why)) => DoctorCheck {
            fixable: true,
            ..fail(
                CheckId::Pi,
                format!("A profile here runs Pi, and it can't be installed or updated yet: {why}"),
                "VibeOS tries again every hour, and whenever a Pi agent starts.",
            )
        },
        (Ok((path, version)), _) => pass(
            CheckId::Pi,
            format!(
                "Pi {} is at {}",
                version.split_whitespace().next().unwrap_or(version),
                path.display()
            ),
        ),
        (Err(why), PiWanted::No) => DoctorCheck {
            id: CheckId::Pi,
            state: CheckState::Optional,
            detail: format!(
                "{} VibeOS installs it by itself when an agent here needs it.",
                sentence(why)
            ),
            hint: None,
            fixable: false,
            progress: None,
        },
        (Err(_), PiWanted::Installing) => DoctorCheck {
            id: CheckId::Pi,
            state: CheckState::Working,
            detail: "Installing Pi, as a profile here runs it.".to_owned(),
            hint: None,
            fixable: false,
            progress: None,
        },
        (Err(why), PiWanted::Present) => fail(
            CheckId::Pi,
            format!("A profile here runs Pi, and the pi it would run doesn't: {why}"),
            "It isn't one VibeOS will replace: remove it or put it right, and VibeOS takes it \
             from there.",
        ),
    });
    DoctorReport { checks }
}

impl Core {
    /// `doctor.check`.
    pub async fn doctor(&self) -> DoctorReport {
        // A login that was missing may have just been made, so ask again;
        // one that worked is taken from the hourly check.
        let gh = {
            let last = lock(&self.gh_status).clone();
            if last.state == GhState::LoggedIn {
                last
            } else {
                self.check_gh().await
            }
        };
        let cached = lock(&self.doctor_found)
            .as_ref()
            .filter(|(at, _)| at.elapsed() < FOUND_FOR)
            .map(|(_, f)| f.clone());
        let checks = Arc::clone(&self.checks);
        let linux = self.linux_images.clone();
        let logged_in = gh.state == GhState::LoggedIn;
        let program = self.gh_program().await;
        let pi_wanted = self.pi_wanted().await;
        let tools = self.tools_now().await;
        let found = tokio::task::spawn_blocking(move || {
            let found = cached.unwrap_or_else(|| {
                let git = checks.git();
                // Without git there's no config to read, and on a Mac
                // without the Command Line Tools, asking opens their
                // installer again.
                let git_uses_gh = match &git {
                    Ok(_) if logged_in => git_through_gh(&*checks, &program),
                    Ok(_) => checks.git_uses_gh(),
                    Err(why) => Err(why.clone()),
                };
                Found {
                    git,
                    git_uses_gh,
                    claude: checks.claude(),
                    claude_login: checks.claude_login(),
                    pi: checks.pi(),
                    system_sleep: checks.system_sleep(),
                    linger: lingering(&*checks),
                }
            });
            (found, linux.map(|w| w.check()))
        })
        .await;
        let (found, linux) = match found {
            Ok((found, linux)) => {
                *lock(&self.doctor_found) = Some((std::time::Instant::now(), found.clone()));
                (found, linux)
            }
            Err(e) => (Self::found_failed(&e.to_string()), None),
        };
        let mut report = report(
            found,
            &gh,
            &(self.tailscale)(),
            linux,
            self.info().os == "macos",
            &pi_wanted,
            &tools,
        );
        self.say_how_the_template_stands(&mut report);
        report
    }

    /// The `linux` check, once the images are there: the template's on its way to a
    /// new base by itself, or it's waiting because that would lose something. Agents
    /// work either way, so neither is on the list of what needs the owner.
    fn say_how_the_template_stands(&self, report: &mut DoctorReport) {
        let Some(check) = report.checks.iter_mut().find(|c| c.id == CheckId::Linux) else {
            return;
        };
        if !matches!(check.state, CheckState::Pass | CheckState::Optional) {
            return;
        }
        let Some(host) = &self.toolbox else {
            return;
        };
        let held = self.toolbox_held(&host.versions());
        if *lock(&self.toolbox_state) == vibe_core::model::ToolboxState::Rebuilding
            && self
                .toolbox_moving
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            check.state = CheckState::Optional;
            check.detail = "VibeOS is moving the Linux template to the new base image.".to_owned();
            check.hint = None;
        } else if !held.is_empty() {
            check.state = CheckState::Optional;
            check.detail = format!(
                "The Linux template stays on its base image for now: moving it would lose what \
                 was installed by hand and VibeOS can't put back: {}. Settings → Linux Toolbox \
                 moves it with one click.",
                held.join(", ")
            );
            check.hint = None;
        }
    }

    fn found_failed(e: &str) -> Found {
        {
            let why = format!("the check failed: {e}");
            Found {
                git: Err(why.clone()),
                git_uses_gh: Err(why.clone()),
                claude: Err(why.clone()),
                claude_login: Err(why.clone()),
                pi: Err(why.clone()),
                system_sleep: Err(why.clone()),
                linger: Err(why),
            }
        }
    }

    /// `doctor.fix`: tries now what VibeOS does by itself anyway: `gh_git`,
    /// `linger`, `pi` and `linux`. The rest are refused, with what to do.
    pub async fn doctor_fix(&self, p: DoctorFixParams) -> Result<DoctorReport, ErrorObject> {
        match p.check {
            CheckId::GhGit => {
                let gh = self.check_gh().await;
                if gh.state != GhState::LoggedIn {
                    return Err(error(
                        ErrorCode::InvalidParams,
                        "Invalid params: gh_git needs gh logged in first: sign in from VibeOS"
                            .to_owned(),
                    ));
                }
                let (checks, gh) = (Arc::clone(&self.checks), self.gh_program().await);
                tokio::task::spawn_blocking(move || checks.setup_git(&gh))
                    .await
                    .map_err(|e| error(ErrorCode::InternalError, e.to_string()))?
                    .map_err(|e| error(ErrorCode::CommandFailed, e))?;
                crate::log!("vibed: git now asks gh for GitHub's credentials (gh auth setup-git)");
                *lock(&self.doctor_found) = None;
            }
            CheckId::Gh | CheckId::Claude if self.tool_try_again(p.check).await => {
                return Ok(self.doctor().await);
            }
            CheckId::Linger => {
                let checks = Arc::clone(&self.checks);
                tokio::task::spawn_blocking(move || lingering(&*checks))
                    .await
                    .map_err(|e| error(ErrorCode::InternalError, e.to_string()))?
                    .map_err(|e| error(ErrorCode::CommandFailed, e))?;
                *lock(&self.doctor_found) = None;
            }
            CheckId::Pi => {
                self.repair_pi().await;
                if let PiWanted::Failed(why) = self.pi_wanted().await {
                    return Err(error(ErrorCode::CommandFailed, why));
                }
            }
            CheckId::Linux => match &self.linux_images {
                Some(watch) => watch.kick(),
                None => {
                    return Err(error(
                        ErrorCode::InvalidParams,
                        "Invalid params: this host has no Linux images to download".to_owned(),
                    ));
                }
            },
            other => {
                let report = self.doctor().await;
                let hint = report
                    .checks
                    .iter()
                    .find(|c| c.id == other)
                    .and_then(|c| c.hint.clone())
                    .unwrap_or_else(|| "it needs nothing".to_owned());
                return Err(error(
                    ErrorCode::InvalidParams,
                    format!("Invalid params: VibeOS can't fix {other} for you: {hint}"),
                ));
            }
        }
        Ok(self.doctor().await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn pmset_says_when_the_mac_sleeps() {
        let pmset = "System-wide power settings:\nCurrently in use:\n standby              0\n \
                     Sleep On Power Button 1\n hibernatefile        /var/vm/sleepimage\n \
                     displaysleep         10\n sleep                0 (sleep prevented by \
                     coreaudiod)\n disksleep            10\n";
        assert_eq!(system_sleep_minutes(pmset), Some(0));
        assert_eq!(
            system_sleep_minutes(" displaysleep 2\n sleep                1\n"),
            Some(1)
        );
        assert_eq!(system_sleep_minutes(" displaysleep 2\n"), None);
        assert_eq!(system_sleep_minutes(" sleep  never\n"), None);
    }

    /// #42: a gh (and a claude) only in `~/.local/bin`, which the `PATH`
    /// launchd gives `vibed` lacks, are found, by the same lookup.
    #[test]
    fn tools_only_in_local_bin_are_found() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, sys) = (tmp.path().join("home"), tmp.path().join("sys"));
        let local_bin = home.join(".local/bin");
        std::fs::create_dir_all(&local_bin).unwrap();
        std::fs::create_dir_all(&sys).unwrap();
        // As minimal as launchd's, but empty, so a gh the system has
        // can't be found instead.
        let m = Machine::new(&home).with_path(sys.to_string_lossy());
        assert_eq!(m.find("vibe-no-such-tool"), None);
        let gh = script(&local_bin, "gh", "exit 0");
        assert_eq!(m.find("gh"), Some(gh));
        let claude = script(&local_bin, "claude", r#"echo "2.1.314 (Claude Code)""#);
        assert_eq!(m.claude(), Ok((claude, "2.1.314 (Claude Code)".into())));
        // The PATH comes first.
        let first = script(&sys, "gh", "exit 0");
        assert_eq!(m.find("gh"), Some(first));
    }

    #[test]
    fn git_uses_gh_when_a_helper_is_gh() {
        assert!(uses_gh("\n!/opt/homebrew/bin/gh auth git-credential\n"));
        assert!(uses_gh("!gh auth git-credential"));
        assert!(!uses_gh("osxkeychain\n"));
        assert!(!uses_gh(""));
    }

    #[test]
    fn sentences() {
        assert_eq!(sentence("git isn't installed"), "git isn't installed.");
        assert_eq!(sentence("github is down"), "Github is down.");
        assert_eq!(sentence("Done."), "Done.");
        assert_eq!(sentence("état"), "État.");
        assert_eq!(sentence(""), ".");
    }

    fn found() -> Found {
        Found {
            git: Ok("git version 2.50.1".into()),
            git_uses_gh: Ok(true),
            claude: Ok((
                "/usr/local/bin/claude".into(),
                "2.1.314 (Claude Code)".into(),
            )),
            claude_login: Ok("Claude Code is signed in".into()),
            pi: Err("Pi isn't on your login shell's PATH".into()),
            system_sleep: Ok(Some(0)),
            linger: Ok(None),
        }
    }

    /// The report for a host, with no one wanting Pi.
    fn report(
        found: Found,
        gh: &GhStatus,
        tailscale: &[std::net::IpAddr],
        linux: Option<DoctorCheck>,
        mac: bool,
    ) -> DoctorReport {
        super::report(
            found,
            gh,
            tailscale,
            linux,
            mac,
            &PiWanted::No,
            &Default::default(),
        )
    }

    fn logged_in() -> GhStatus {
        GhStatus {
            state: GhState::LoggedIn,
            account: Some("octocat".into()),
            detail: None,
            checked_at: None,
        }
    }

    fn states(r: &DoctorReport) -> Vec<(CheckId, CheckState)> {
        r.checks.iter().map(|c| (c.id, c.state)).collect()
    }

    #[test]
    fn a_ready_mac_passes_everything_but_tailscale() {
        let r = report(found(), &logged_in(), &[], None, true);
        assert_eq!(
            states(&r),
            [
                (CheckId::Git, CheckState::Pass),
                (CheckId::Gh, CheckState::Pass),
                (CheckId::GhGit, CheckState::Pass),
                (CheckId::Claude, CheckState::Pass),
                (CheckId::ClaudeLogin, CheckState::Pass),
                (CheckId::Power, CheckState::Pass),
                (CheckId::Tailscale, CheckState::Optional),
                (CheckId::Pi, CheckState::Optional),
            ]
        );
        assert!(r.checks.iter().all(|c| !c.fixable));
        assert!(r.checks[..6].iter().all(|c| c.hint.is_none()));
        assert_eq!(
            r.checks[1].detail,
            "GitHub's gh is logged in to github.com as octocat."
        );
    }

    #[test]
    fn what_a_fresh_mac_lacks_says_what_to_do() {
        let fresh = Found {
            git: Err("git needs Apple's Command Line Tools, which aren't installed".into()),
            git_uses_gh: Ok(false),
            claude: Err("claude isn't on your login shell's PATH".into()),
            claude_login: Err("Claude Code isn't signed in".into()),
            pi: Err("Pi isn't on your login shell's PATH".into()),
            system_sleep: Ok(Some(10)),
            linger: Ok(None),
        };
        let gh = GhStatus {
            state: GhState::NotInstalled,
            account: None,
            detail: Some("gh isn't installed".into()),
            checked_at: None,
        };
        let r = report(fresh, &gh, &[], None, true);
        // Claude's login waits for Claude, as git using gh does for gh: one thing to
        // do, not two.
        let waits = [CheckId::GhGit, CheckId::ClaudeLogin];
        for c in r.checks[..6].iter().filter(|c| !waits.contains(&c.id)) {
            assert_eq!(c.state, CheckState::Fail, "{c:?}");
            assert!(c.hint.as_deref().is_some_and(|h| h.ends_with('.')), "{c:?}");
            assert!(c.detail.ends_with('.'), "{c:?}");
        }
        assert!(
            r.checks[0]
                .hint
                .as_deref()
                .unwrap()
                .contains("Command Line Tools")
        );
        assert!(r.checks[5].detail.contains("10 minutes"));

        // Logged in, and it's not set up, which the doctor would have just tried.
        let mut half = found();
        half.git_uses_gh = Ok(false);
        let r = report(half, &logged_in(), &[], None, true);
        assert_eq!(r.checks[2].state, CheckState::Fail);
        assert!(r.checks[2].fixable);

        // Logged in, and setting git up didn't take: VibeOS is still on it, and it
        // says what's in the way.
        let mut half = found();
        half.git_uses_gh = Err("gh auth setup-git failed: no space left".into());
        let r = report(half, &logged_in(), &[], None, true);
        assert_eq!(r.checks[2].state, CheckState::Fail);
        assert!(r.checks[2].detail.contains("no space left"));
        assert!(r.checks[2].hint.as_deref().unwrap().contains("itself"));
        assert!(r.checks[2].fixable);
    }

    #[test]
    fn git_through_gh_asks_nothing_of_the_owner() {
        // gh's not logged in, or not there: that's one thing to do, not two.
        let mut f = found();
        f.git_uses_gh = Ok(false);
        for state in [GhState::LoggedOut, GhState::NotInstalled] {
            let gh = GhStatus {
                state,
                account: None,
                detail: None,
                checked_at: None,
            };
            let r = report(f.clone(), &gh, &[], None, false);
            let c = &r.checks[2];
            assert_eq!((c.id, c.state), (CheckId::GhGit, CheckState::Optional));
            assert_eq!(c.hint, None);
            assert!(!c.fixable);
            assert_eq!(r.checks[1].state, CheckState::Fail);
        }
        // Nothing's found failing that it can put right.
        let tmp = tempfile::tempdir().unwrap();
        let checks = FixedChecks {
            git: Ok("git version 2.50.1".into()),
            git_uses_gh: Mutex::new(Ok(false)),
            claude: Err(String::new()),
            claude_login: Err(String::new()),
            pi: Err(String::new()),
            system_sleep: Ok(None),
            linger: Mutex::new(Ok(None)),
            enable_linger: Ok(()),
            installs_tools: false,
        };
        assert_eq!(git_through_gh(&checks, tmp.path().as_os_str()), Ok(true));
    }

    #[test]
    fn a_linux_host_has_no_sleep_to_check_and_tailscale_can_pass() {
        let mut f = found();
        f.system_sleep = Ok(None);
        let ip: std::net::IpAddr = "100.64.0.7".parse().unwrap();
        let r = report(f, &logged_in(), &[ip], None, false);
        assert!(r.checks.iter().all(|c| c.id != CheckId::Power));
        let ts = r
            .checks
            .iter()
            .find(|c| c.id == CheckId::Tailscale)
            .unwrap();
        assert_eq!((ts.id, ts.state), (CheckId::Tailscale, CheckState::Pass));
        let pi = r.checks.last().unwrap();
        assert_eq!((pi.id, pi.state), (CheckId::Pi, CheckState::Optional));
        assert!(ts.detail.contains("100.64.0.7"));
    }

    #[test]
    fn the_linux_check_comes_after_the_machines_and_pi_last() {
        let linux = pass(CheckId::Linux, "ready");
        let r = report(found(), &logged_in(), &[], Some(linux), true);
        let last: Vec<CheckId> = r.checks.iter().rev().take(2).map(|c| c.id).collect();
        assert_eq!(last, [CheckId::Pi, CheckId::Linux]);
        let ids: Vec<CheckId> = r.checks.iter().map(|c| c.id).collect();
        let mut sorted = ids.clone();
        sorted.sort_by_key(|id| CheckId::ALL.iter().position(|x| x == id));
        assert_eq!(ids, sorted, "checks come in CheckId's order");
    }

    /// #74: nothing about Pi is ever the owner's chore. Nobody wanting it asks
    /// nothing, wanting it shows as under way, and only a failure is on the list,
    /// with a button to try again.
    #[test]
    fn pi_asks_nothing_of_the_owner() {
        let r = report(found(), &logged_in(), &[], None, false);
        let pi = r.checks.last().unwrap();
        assert_eq!((pi.id, pi.state), (CheckId::Pi, CheckState::Optional));
        assert_eq!(pi.hint, None, "{pi:?}");
        assert!(!pi.fixable);
        assert!(pi.detail.contains("installs it by itself"), "{pi:?}");
        assert!(!pi.detail.contains("vibe pi"), "{pi:?}");

        let installing = super::report(
            found(),
            &logged_in(),
            &[],
            None,
            false,
            &PiWanted::Installing,
            &Default::default(),
        );
        let pi = installing.checks.last().unwrap();
        assert_eq!(pi.state, CheckState::Working);
        assert_eq!(pi.hint, None);

        let failed = super::report(
            found(),
            &logged_in(),
            &[],
            None,
            false,
            &PiWanted::Failed("the network is down".into()),
            &Default::default(),
        );
        let pi = failed.checks.last().unwrap();
        assert_eq!((pi.state, pi.fixable), (CheckState::Fail, true));
        assert!(pi.detail.contains("the network is down"), "{pi:?}");
        assert!(pi.hint.as_ref().unwrap().contains("tries again"), "{pi:?}");

        // A move to a new pin that failed is said, with the old Pi still there.
        let mut has_pi = found();
        has_pi.pi = Ok(("/h/.local/bin/pi".into(), "1.4.0".into()));
        let failed = super::report(
            has_pi,
            &logged_in(),
            &[],
            None,
            false,
            &PiWanted::Failed("the network is down".into()),
            &Default::default(),
        );
        assert_eq!(failed.checks.last().unwrap().state, CheckState::Fail);

        // One that's there and doesn't run isn't called something VibeOS will fix.
        let mut broken = found();
        broken.pi = Err("pi --version failed: bad".into());
        let r = super::report(
            broken,
            &logged_in(),
            &[],
            None,
            false,
            &PiWanted::Present,
            &Default::default(),
        );
        let pi = r.checks.last().unwrap();
        assert_eq!((pi.state, pi.fixable), (CheckState::Fail, false));
        assert!(pi.detail.contains("pi --version failed"), "{pi:?}");
    }

    #[test]
    fn lingering_is_only_a_linux_hosts_and_says_what_it_costs() {
        // Nothing to check on a Mac.
        let r = report(found(), &logged_in(), &[], None, true);
        assert!(r.checks.iter().all(|c| c.id != CheckId::Linger));
        let with = |linger| Found { linger, ..found() };
        let row = |r: &DoctorReport| r.checks.iter().find(|c| c.id == CheckId::Linger).cloned();
        let on = report(with(Ok(Some(true))), &logged_in(), &[], None, false);
        assert_eq!(row(&on).unwrap().state, CheckState::Pass);
        for off in [Ok(Some(false)), Err("polkit said no".to_owned())] {
            let r = report(with(off), &logged_in(), &[], None, false);
            let c = row(&r).unwrap();
            assert_eq!((c.state, c.fixable), (CheckState::Fail, true), "{c:?}");
            assert!(c.detail.contains("stops when you log out"), "{c:?}");
            let hint = c.hint.unwrap();
            assert!(hint.contains("Settings → Hosts"), "{hint}");
            // A button, never a command.
            assert!(!hint.contains('`'), "{hint}");
        }
        // Where it comes in the list.
        let ids: Vec<CheckId> = on.checks.iter().map(|c| c.id).collect();
        let mut sorted = ids.clone();
        sorted.sort_by_key(|id| CheckId::ALL.iter().position(|x| x == id));
        assert_eq!(ids, sorted);
    }

    /// The real checks, on a stand-in `loginctl` that does as the real one does and
    /// a `sudo` that asks nothing, or is refused.
    #[test]
    fn lingering_turns_itself_on_every_way_that_needs_no_password() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, bin, dir) = (
            tmp.path().join("home"),
            tmp.path().join("bin"),
            tmp.path().join("linger"),
        );
        for d in [&home, &bin, &dir] {
            std::fs::create_dir_all(d).unwrap();
        }
        let path = format!("{}:/usr/bin:/bin", bin.display());
        let machine = || Machine::new(&home).with_path(&path).with_linger("me", &dir);
        // Allowed as the user: that's all it takes, and sudo isn't touched.
        let log = tmp.path().join("sudo-log");
        let sudo_body = format!(
            "echo \"$*\" >> \"{log}\"\nshift\nexec \"$@\"",
            log = log.display()
        );
        script(&bin, "sudo", &sudo_body);
        script(
            &bin,
            "loginctl",
            &format!(
                "[ -e \"{refuse}\" ] && {{ echo 'Could not enable linger: Access denied' >&2; rm -f \"{refuse}\"; exit 1; }}\n[ \"$1 $2\" = 'enable-linger me' ] && touch \"{dir}/me\"",
                refuse = tmp.path().join("refuse").display(),
                dir = dir.display(),
            ),
        );
        let m = machine();
        assert_eq!(m.linger(), Ok(Some(false)));
        assert_eq!(lingering(&m), Ok(Some(true)));
        assert!(dir.join("me").exists());
        assert!(!log.exists(), "sudo wasn't needed");
        // On already: nothing is run at all.
        std::fs::remove_file(bin.join("sudo")).unwrap();
        assert_eq!(lingering(&m), Ok(Some(true)));

        // Refused as the user, as without polkit: passwordless sudo, for that alone.
        std::fs::remove_file(dir.join("me")).unwrap();
        std::fs::write(tmp.path().join("refuse"), "").unwrap();
        script(&bin, "sudo", &sudo_body);
        let m = machine();
        assert_eq!(lingering(&m), Ok(Some(true)));
        let said = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            said.trim(),
            format!("-n {}/loginctl enable-linger me", bin.display())
        );

        // Refused both ways: it says so, and sudo's asked no more this run.
        std::fs::remove_file(dir.join("me")).unwrap();
        std::fs::remove_file(&log).unwrap();
        script(
            &bin,
            "loginctl",
            "echo 'Could not enable linger: Access denied' >&2; exit 1",
        );
        script(
            &bin,
            "sudo",
            &format!(
                "echo \"$*\" >> \"{log}\"\necho 'sudo: a password is required' >&2\nexit 1",
                log = log.display()
            ),
        );
        let m = machine();
        let why = lingering(&m).unwrap_err();
        assert!(why.contains("Access denied"), "{why}");
        lingering(&m).unwrap_err();
        assert_eq!(
            std::fs::read_to_string(&log).unwrap().lines().count(),
            1,
            "a refused sudo is in the server's logs"
        );

        // A HOME that isn't this user's, as a test's: left alone, whatever's there.
        let other = Machine::new(&home).with_path(&path);
        assert_eq!(other.linger(), Ok(None));
        assert_eq!(lingering(&other), Ok(None));
    }

    #[test]
    fn pi_says_where_it_is_when_it_is_there() {
        let mut f = found();
        f.pi = Ok(("/h/.local/bin/pi".into(), "1.5.0".into()));
        let r = report(f, &logged_in(), &[], None, true);
        let pi = r.checks.last().unwrap();
        assert_eq!(pi.state, CheckState::Pass);
        assert_eq!(pi.detail, "Pi 1.5.0 is at /h/.local/bin/pi.");
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// The real checks, on stand-ins for git, gh and claude on a `PATH` of
    /// their own, in a home of their own.
    #[test]
    fn the_machine_checks_run_what_the_path_has() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, bin) = (tmp.path().join("home"), tmp.path().join("bin"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let path = format!("{}:/usr/bin:/bin", bin.display());
        let m = Machine::new(&home).with_path(&path);
        // Nothing on PATH but the system's: no claude, and not signed in.
        assert!(
            m.claude()
                .unwrap_err()
                .contains("isn't on your login shell's PATH")
        );
        if !cfg!(target_os = "macos") {
            assert_eq!(m.claude_login().unwrap_err(), "Claude Code isn't signed in");
            assert_eq!(m.system_sleep(), Ok(None));
        }

        // git that answers as git does, and keeps its global config in
        // the home, as real git would.
        let config = home.join("helpers");
        script(
            &bin,
            "git",
            &format!(
                r#"case "$1" in
--version) echo "git version 2.50.1" ;;
config) case "$4" in
  credential.https://github.com.helper) cat "{c}" 2>/dev/null || exit 1 ;;
  *) exit 1 ;;
  esac ;;
esac"#,
                c = config.display()
            ),
        );
        // gh auth setup-git writes the helper, as the real one does.
        let gh = script(
            &bin,
            "gh",
            &format!(
                r#"[ "$1 $2" = "auth setup-git" ] || exit 2
printf '\n!%s auth git-credential\n' "$0" > "{c}""#,
                c = config.display()
            ),
        );
        let claude = script(&bin, "claude", r#"echo "2.1.314 (Claude Code)""#);
        if !cfg!(target_os = "macos") {
            assert_eq!(m.git(), Ok("git version 2.50.1".into()));
        }
        assert_eq!(m.git_uses_gh(), Ok(false));
        // A helper of the owner's own, just for GitHub, is theirs, and counts.
        std::fs::write(&config, "/opt/custom-helper\n").unwrap();
        assert_eq!(m.git_uses_gh(), Ok(true));
        std::fs::remove_file(&config).unwrap();
        m.setup_git(gh.as_os_str()).unwrap();
        assert_eq!(m.git_uses_gh(), Ok(true));
        assert_eq!(
            m.claude(),
            Ok((claude.clone(), "2.1.314 (Claude Code)".into()))
        );
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(home.join(".claude/.credentials.json"), "{}").unwrap();
        assert!(m.claude_login().is_ok());

        // A gh that fails says so; a claude given by VIBE_CLAUDE is the one.
        let bad = script(&bin, "bad-gh", "echo 'not logged in' >&2; exit 1");
        assert!(
            m.setup_git(bad.as_os_str())
                .unwrap_err()
                .contains("not logged in")
        );
        let other = script(&bin, "other-claude", "echo 9.9.9");
        let m = Machine::new(&home).with_path(&path).with_claude(&other);
        assert_eq!(m.claude(), Ok((other, "9.9.9".into())));
    }
}
