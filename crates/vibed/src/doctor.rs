//! `vibe doctor` and the first run's checklist (PRD: Key user flows,
//! Install and setup): what this host needs for agents to work, as `vibed`
//! sees it. git; `gh`'s login, and git using `gh` for GitHub (`gh auth
//! setup-git`); the `claude` macos agents run, and its login; system sleep
//! off; Tailscale, which is optional; and on a Mac, the Linux images. The
//! checks that look at the machine go through [`HostChecks`], so tests can
//! give fixed answers. VibeOS.app adds its own permission to control
//! Terminal, which only the app can see.

use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    /// setup-git` sets it up.
    fn git_uses_gh(&self) -> Result<bool, String>;
    /// Runs `<gh> auth setup-git`.
    fn setup_git(&self, gh: &OsStr) -> Result<(), String>;
    /// The `claude` this host's native agents run, and its version.
    fn claude(&self) -> Result<(PathBuf, String), String>;
    /// How that `claude` is logged in.
    fn claude_login(&self) -> Result<String, String>;
    /// Minutes of idleness before the system sleeps, `Some(0)` for never;
    /// `None` where there's nothing to check, as on a Linux host.
    fn system_sleep(&self) -> Result<Option<u32>, String>;
}

/// The real machine: `HOME`'s, with the `PATH` its native agents get (on a
/// Mac, the login shell's).
#[derive(Debug)]
pub struct Machine {
    home: PathBuf,
    /// `VIBE_CLAUDE`, if it names the `claude` agents run.
    claude: Option<PathBuf>,
    path: Mutex<Option<String>>,
}

impl Machine {
    pub fn new(home: impl Into<PathBuf>) -> Machine {
        Machine {
            home: home.into(),
            claude: std::env::var_os("VIBE_CLAUDE")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            path: Mutex::new(None),
        }
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

    fn claude_login(&self) -> Result<String, String> {
        if cfg!(target_os = "macos") {
            let (ok, _) = self.run(&[
                "/usr/bin/security",
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
}

/// Fixed answers, for tests and the golden examples. `setup_git` makes
/// `git_uses_gh` true.
#[derive(Debug)]
pub struct FixedChecks {
    pub git: Result<String, String>,
    pub git_uses_gh: Mutex<Result<bool, String>>,
    pub claude: Result<(PathBuf, String), String>,
    pub claude_login: Result<String, String>,
    pub system_sleep: Result<Option<u32>, String>,
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
    fn system_sleep(&self) -> Result<Option<u32>, String> {
        self.system_sleep.clone()
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
const COMMANDS: [&str; 6] = ["git", "gh", "claude", "vibe", "pmset", "curl"];

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

/// What the machine said, before any of it is put into checks.
#[derive(Debug, Clone)]
pub(crate) struct Found {
    git: Result<String, String>,
    git_uses_gh: Result<bool, String>,
    claude: Result<(PathBuf, String), String>,
    claude_login: Result<String, String>,
    system_sleep: Result<Option<u32>, String>,
}

/// The checks, in [`CheckId`]'s order, from what was found.
fn report(
    found: Found,
    gh: &GhStatus,
    tailscale: &[std::net::IpAddr],
    linux: Option<DoctorCheck>,
    mac: bool,
) -> DoctorReport {
    let mut checks = Vec::new();
    let install_git = if mac {
        "Install Apple's Command Line Tools: run `xcode-select --install` in Terminal."
    } else {
        "Install git with this server's package manager."
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
        GhState::NotInstalled => fail(
            CheckId::Gh,
            "GitHub's gh isn't installed",
            "Install gh from https://cli.github.com, then run `gh auth login` in Terminal.",
        ),
        GhState::LoggedOut => fail(
            CheckId::Gh,
            "GitHub's gh isn't logged in",
            "Run `gh auth login` in Terminal.",
        ),
        GhState::Unknown | GhState::Error => fail(
            CheckId::Gh,
            gh.detail.as_deref().unwrap_or("gh auth status failed"),
            "Run `gh auth status` in Terminal to see why.",
        ),
    });
    checks.push(match (&found.git_uses_gh, gh_ok) {
        (Ok(true), _) => pass(CheckId::GhGit, "git asks gh for GitHub's credentials"),
        (_, false) => fail(
            CheckId::GhGit,
            "git can use gh for GitHub once gh is logged in",
            "Log gh in first, with `gh auth login`.",
        ),
        (Ok(false), true) => DoctorCheck {
            fixable: true,
            ..fail(
                CheckId::GhGit,
                "git doesn't ask gh for GitHub's credentials yet",
                "Run `gh auth setup-git` in Terminal.",
            )
        },
        (Err(why), true) => fail(CheckId::GhGit, why, install_git),
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
        Err(why) => fail(
            CheckId::Claude,
            why,
            "Install Claude Code (claude.com/claude-code), then check `claude` runs in Terminal.",
        ),
    });
    checks.push(match (&found.claude, &found.claude_login) {
        (Err(_), _) => fail(
            CheckId::ClaudeLogin,
            "Claude Code can sign in once it's installed",
            "Install Claude Code first.",
        ),
        (Ok(_), Ok(how)) => pass(CheckId::ClaudeLogin, how),
        (Ok(_), Err(why)) => fail(
            CheckId::ClaudeLogin,
            why,
            "Run `claude` in Terminal and sign in with /login.",
        ),
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
             off (on a laptop, in Battery → Options, on power adapter); or run `sudo pmset -c \
             sleep 0` in Terminal.",
        )),
        Err(why) => checks.push(fail(
            CheckId::Power,
            why,
            "Run `pmset -g` in Terminal to see this Mac's sleep settings.",
        )),
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
        let found = tokio::task::spawn_blocking(move || {
            let found = cached.unwrap_or_else(|| {
                let git = checks.git();
                // Without git there's no config to read, and on a Mac
                // without the Command Line Tools, asking opens their
                // installer again.
                let git_uses_gh = match &git {
                    Ok(_) => checks.git_uses_gh(),
                    Err(why) => Err(why.clone()),
                };
                Found {
                    git,
                    git_uses_gh,
                    claude: checks.claude(),
                    claude_login: checks.claude_login(),
                    system_sleep: checks.system_sleep(),
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
        report(
            found,
            &gh,
            &(self.tailscale)(),
            linux,
            self.info().os == "macos",
        )
    }

    fn found_failed(e: &str) -> Found {
        {
            let why = format!("the check failed: {e}");
            Found {
                git: Err(why.clone()),
                git_uses_gh: Err(why.clone()),
                claude: Err(why.clone()),
                claude_login: Err(why.clone()),
                system_sleep: Err(why),
            }
        }
    }

    /// `doctor.fix`: `gh_git` and `linux` are VibeOS's to fix; the rest are
    /// refused, with what to do.
    pub async fn doctor_fix(&self, p: DoctorFixParams) -> Result<DoctorReport, ErrorObject> {
        match p.check {
            CheckId::GhGit => {
                let gh = self.check_gh().await;
                if gh.state != GhState::LoggedIn {
                    return Err(error(
                        ErrorCode::InvalidParams,
                        "Invalid params: gh_git needs gh logged in first: run `gh auth login`"
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
            system_sleep: Ok(Some(0)),
        }
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
            system_sleep: Ok(Some(10)),
        };
        let gh = GhStatus {
            state: GhState::NotInstalled,
            account: None,
            detail: Some("gh isn't installed".into()),
            checked_at: None,
        };
        let r = report(fresh, &gh, &[], None, true);
        for c in &r.checks[..6] {
            assert_eq!(c.state, CheckState::Fail, "{c:?}");
            assert!(c.hint.as_deref().is_some_and(|h| h.ends_with('.')), "{c:?}");
            assert!(c.detail.ends_with('.'), "{c:?}");
        }
        assert!(
            r.checks[0]
                .hint
                .as_deref()
                .unwrap()
                .contains("xcode-select --install")
        );
        // git can't use gh until gh is logged in, so that isn't fixable yet.
        assert!(!r.checks[2].fixable);
        assert!(r.checks[5].detail.contains("10 minutes"));

        // Logged in, git not set up: VibeOS can fix that one.
        let mut half = found();
        half.git_uses_gh = Ok(false);
        let r = report(half, &logged_in(), &[], None, true);
        assert_eq!(r.checks[2].state, CheckState::Fail);
        assert!(r.checks[2].fixable);
    }

    #[test]
    fn a_linux_host_has_no_sleep_to_check_and_tailscale_can_pass() {
        let mut f = found();
        f.system_sleep = Ok(None);
        let ip: std::net::IpAddr = "100.64.0.7".parse().unwrap();
        let r = report(f, &logged_in(), &[ip], None, false);
        assert!(r.checks.iter().all(|c| c.id != CheckId::Power));
        let ts = r.checks.last().unwrap();
        assert_eq!((ts.id, ts.state), (CheckId::Tailscale, CheckState::Pass));
        assert!(ts.detail.contains("100.64.0.7"));
    }

    #[test]
    fn the_linux_check_goes_last() {
        let linux = pass(CheckId::Linux, "ready");
        let r = report(found(), &logged_in(), &[], Some(linux), true);
        assert_eq!(r.checks.last().unwrap().id, CheckId::Linux);
        let ids: Vec<CheckId> = r.checks.iter().map(|c| c.id).collect();
        let mut sorted = ids.clone();
        sorted.sort_by_key(|id| CheckId::ALL.iter().position(|x| x == id));
        assert_eq!(ids, sorted, "checks come in CheckId's order");
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
