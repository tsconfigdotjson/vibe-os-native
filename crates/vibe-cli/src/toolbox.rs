//! `vibe toolbox`: the Linux toolbox shell (PRD: The toolbox shell). It
//! asks `vibed` to open the toolbox and attaches this terminal to its
//! shell; when the shell exits, `vibed` saves a new template and Linux home
//! version, and this waits to say which. `Ctrl-\` detaches and leaves the
//! shell open, for `vibe toolbox` to attach to again.
//!
//! `vibe toolbox status`, `update` and `rebuild` show the versions, run
//! `toolbox.toml`'s `[update]` now, and rebuild the template on the newest
//! base image.

use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use vibe_core::model::{LoginTool, ToolboxAction, ToolboxState};
use vibe_core::rpc::{
    NoParams, ToolboxLogins, ToolboxLoginsGet, ToolboxOpen, ToolboxRebuild, ToolboxRun,
    ToolboxStatus, ToolboxStatusGet, ToolboxUpdate,
};

use crate::attach::{self, End};
use crate::client;

/// How long a boot of the toolbox may take, its disks' clones included.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// How long an update or a rebuild may take: installs download a lot.
const RUN_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);

/// How long saving may take once the shell exits: flushing a big install.
const SAVE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Which `vibe toolbox`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Open,
    Status,
    Update,
    Rebuild,
    Logins,
}

impl Command {
    pub(crate) fn parse(word: &str) -> Option<Command> {
        Some(match word {
            "status" => Command::Status,
            "update" => Command::Update,
            "rebuild" => Command::Rebuild,
            "logins" => Command::Logins,
            _ => return None,
        })
    }
}

/// Runs `command`; an error is a sentence for the owner.
pub(crate) fn run(socket: &Path, command: Command) -> Result<(), String> {
    match command {
        Command::Open => open(socket),
        Command::Status => {
            let status = status(socket)?;
            print!("{}", describe_status(&status));
            Ok(())
        }
        Command::Logins => {
            let logins =
                client::call::<ToolboxLoginsGet>(socket, &NoParams {}, Duration::from_secs(30))
                    .map_err(|e| e.to_string())?;
            print!("{}", describe_logins(&logins));
            Ok(())
        }
        Command::Update => {
            eprintln!("Running toolbox.toml's [update] commands on the template…");
            let run = client::call::<ToolboxUpdate>(socket, &NoParams {}, RUN_TIMEOUT)
                .map_err(|e| e.to_string())?;
            println!("{}", describe_run(&run));
            Ok(())
        }
        Command::Rebuild => {
            eprintln!(
                "Rebuilding the template on the newest base image from toolbox.toml's recipe…"
            );
            let run = client::call::<ToolboxRebuild>(socket, &NoParams {}, RUN_TIMEOUT)
                .map_err(|e| e.to_string())?;
            println!("{}", describe_run(&run));
            Ok(())
        }
    }
}

fn status(socket: &Path) -> Result<ToolboxStatus, String> {
    client::call::<ToolboxStatusGet>(socket, &NoParams {}, Duration::from_secs(30))
        .map_err(|e| e.to_string())
}

fn open(socket: &Path) -> Result<(), String> {
    if !attach::is_tty(libc::STDIN_FILENO) || !attach::is_tty(libc::STDOUT_FILENO) {
        return Err(
            "the toolbox shell needs a terminal; over SSH, use `ssh -t <host> vibe toolbox`".into(),
        );
    }
    eprintln!("Opening the Linux toolbox…");
    let mut found = client::call::<ToolboxOpen>(socket, &NoParams {}, OPEN_TIMEOUT)
        .map_err(|e| e.to_string())?;
    eprintln!(
        "What you install here (sudo apt install …, curl … | sh, npm -g) and log in to \
         (gh auth login) every new linux agent gets. Exit the shell to save; Ctrl-\\ \
         leaves it open."
    );
    let mut lost = 0;
    loop {
        match attach::attach_to(&found).map_err(|e| e.to_string())? {
            End::Detached => {
                eprintln!(
                    "The toolbox is still open: `vibe toolbox` attaches to it again. Exit its \
                     shell to save."
                );
                return Ok(());
            }
            End::Exited(_) => break,
            // The shim went without saying why. If the shell runs on, vibed
            // holds it again: attach to that.
            End::Lost => {
                std::thread::sleep(Duration::from_secs(2));
                if lost >= 3 || status(socket)?.state != ToolboxState::Open {
                    break;
                }
                lost += 1;
                found = client::call::<ToolboxOpen>(socket, &NoParams {}, OPEN_TIMEOUT)
                    .map_err(|e| e.to_string())?;
            }
        }
    }
    eprintln!("Saving the template and the Linux home…");
    let status = wait_saved(socket)?;
    match &status.last {
        Some(run) if run.action == ToolboxAction::Shell => match &run.error {
            Some(e) => Err(format!("the toolbox couldn't save: {e}")),
            None => {
                println!("{}", describe_saved(&status));
                Ok(())
            }
        },
        _ => Err("the toolbox closed, but vibed didn't say what it saved".into()),
    }
}

/// Waits for `vibed` to notice the shell exited and save.
fn wait_saved(socket: &Path) -> Result<ToolboxStatus, String> {
    let deadline = Instant::now() + SAVE_TIMEOUT;
    loop {
        let status = status(socket)?;
        if !matches!(status.state, ToolboxState::Open | ToolboxState::Saving) {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err("the toolbox is taking too long to save; see `vibe toolbox status`".into());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn describe_saved(status: &ToolboxStatus) -> String {
    let version = |v: Option<u32>| v.map_or_else(|| "none".into(), |v| v.to_string());
    format!(
        "Saved template {} and Linux home {}: new linux agents boot them; running and sleeping \
         agents keep theirs.",
        version(status.template),
        version(status.home)
    )
}

pub(crate) fn describe_run(run: &ToolboxRun) -> String {
    let what = match run.action {
        ToolboxAction::Shell => "The toolbox shell",
        ToolboxAction::Update => "The update",
        ToolboxAction::Rebuild => "The rebuild",
    };
    let mut out = match (&run.error, run.template) {
        (Some(e), _) => format!("{what} failed at {}, and nothing was kept: {e}", run.at),
        (None, Some(t)) => format!("{what} saved template {t} at {}", run.at),
        (None, None) => format!("{what} finished at {}", run.at),
    };
    if !run.uncovered.is_empty() {
        out.push_str(
            "\nInstalled in the old template, but not by toolbox.toml (add them to it, or install \
             them again in `vibe toolbox`):",
        );
        for item in &run.uncovered {
            let _ = write!(out, "\n  {item}");
        }
    }
    out
}

/// A login's tool, for people.
pub(crate) fn tool_name(tool: LoginTool) -> &'static str {
    match tool {
        LoginTool::Gh => "GitHub (gh)",
        LoginTool::Gcloud => "Google Cloud",
        LoginTool::Git => "Git identity",
        LoginTool::Ssh => "SSH key",
    }
}

/// What `vibe toolbox logins` prints: what linux agents start logged in
/// to, or plainly that there's nothing yet, or that it isn't known.
pub(crate) fn describe_logins(l: &ToolboxLogins) -> String {
    let mut out = String::new();
    match (l.home, &l.logins) {
        (None, _) => out.push_str(
            "There's no Linux home yet. Open the toolbox (`vibe toolbox`) and log in to what your \
             linux agents need, such as `gh auth login`; exiting saves it.\n",
        ),
        (Some(home), None) => {
            let _ = writeln!(
                out,
                "Linux home {home} was saved before VibeOS kept track of its logins, so it can't \
                 say what's in it. Open the toolbox (`vibe toolbox`) and exit, and they'll show \
                 here."
            );
        }
        (Some(home), Some(logins)) if logins.is_empty() => {
            let _ = writeln!(
                out,
                "Linux home {home} has no logins yet. Open the toolbox (`vibe toolbox`), log in \
                 there (`gh auth login`, `gcloud auth login`, `git config --global user.name …`), \
                 and exit to save."
            );
        }
        (Some(home), Some(logins)) => {
            let _ = writeln!(
                out,
                "Every new linux agent starts with what Linux home {home} holds:"
            );
            for login in logins {
                let _ = writeln!(out, "  {:<14}{}", tool_name(login.tool), login.account);
            }
        }
    }
    out.push_str(if l.claude {
        "Claude: linux agents are logged in.\n"
    } else {
        "Claude: linux agents aren't logged in yet. Run `vibe setup-token`.\n"
    });
    out
}

pub(crate) fn describe_status(s: &ToolboxStatus) -> String {
    if s.state == ToolboxState::Unavailable {
        return "This host has no Linux toolbox: install tools on it directly.\n".into();
    }
    let mut out = String::new();
    let _ = writeln!(out, "state     {}", s.state);
    let _ = writeln!(
        out,
        "template  {}",
        s.template.map_or_else(
            || "none yet: new agents boot the base image".into(),
            |t| t.to_string()
        )
    );
    let _ = writeln!(
        out,
        "home      {}",
        s.home.map_or_else(|| "none yet".into(), |h| h.to_string())
    );
    match (s.base, s.newest_base) {
        (Some(b), Some(n)) if n > b => {
            let _ = writeln!(
                out,
                "base      {b}; base {n} is installed: `vibe toolbox rebuild` moves the template to it"
            );
        }
        (Some(b), _) => {
            let _ = writeln!(out, "base      {b}");
        }
        (None, _) => {
            let _ = writeln!(out, "base      not downloaded yet");
        }
    }
    if let Some(run) = &s.last {
        let text = describe_run(run).replace('\n', "\n          ");
        let _ = writeln!(out, "last      {text}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> ToolboxStatus {
        ToolboxStatus {
            state: ToolboxState::Closed,
            template: Some(3),
            home: Some(2),
            base: Some(1),
            newest_base: Some(1),
            last: None,
        }
    }

    #[test]
    fn subcommands() {
        assert_eq!(Command::parse("status"), Some(Command::Status));
        assert_eq!(Command::parse("update"), Some(Command::Update));
        assert_eq!(Command::parse("rebuild"), Some(Command::Rebuild));
        assert_eq!(Command::parse("logins"), Some(Command::Logins));
        assert_eq!(Command::parse("open"), None);
    }

    #[test]
    fn logins_say_what_agents_start_with_or_honestly_that_its_nothing() {
        use vibe_core::rpc::Login;
        let mut l = ToolboxLogins {
            home: Some(2),
            logins: Some(vec![
                Login {
                    tool: LoginTool::Gh,
                    account: "octocat on github.com".into(),
                },
                Login {
                    tool: LoginTool::Git,
                    account: "Octo Cat <octo@example.com>".into(),
                },
            ]),
            claude: true,
        };
        assert_eq!(
            describe_logins(&l),
            "Every new linux agent starts with what Linux home 2 holds:\n  \
             GitHub (gh)   octocat on github.com\n  \
             Git identity  Octo Cat <octo@example.com>\n\
             Claude: linux agents are logged in.\n"
        );
        l.logins = Some(vec![]);
        l.claude = false;
        let out = describe_logins(&l);
        assert!(out.starts_with("Linux home 2 has no logins yet."), "{out}");
        assert!(out.ends_with("Run `vibe setup-token`.\n"), "{out}");
        l.logins = None;
        assert!(describe_logins(&l).contains("can't say what's in it"));
        l.home = None;
        assert!(describe_logins(&l).starts_with("There's no Linux home yet."));
    }

    #[test]
    fn the_status_says_what_new_agents_boot() {
        assert_eq!(
            describe_status(&status()),
            "state     closed\ntemplate  3\nhome      2\nbase      1\n"
        );
        let mut s = status();
        s.newest_base = Some(2);
        s.last = Some(ToolboxRun {
            action: ToolboxAction::Rebuild,
            at: "2026-10-30T12:00:00Z".into(),
            template: Some(3),
            uncovered: vec!["apt: htop".into()],
            error: None,
        });
        let text = describe_status(&s);
        assert!(
            text.contains("base 2 is installed: `vibe toolbox rebuild`"),
            "{text}"
        );
        assert!(
            text.contains("last      The rebuild saved template 3 at 2026-10-30T12:00:00Z\n"),
            "{text}"
        );
        assert!(text.contains("\n            apt: htop\n"), "{text}");
        let none = ToolboxStatus {
            state: ToolboxState::Unavailable,
            template: None,
            home: None,
            base: None,
            newest_base: None,
            last: None,
        };
        assert!(describe_status(&none).contains("no Linux toolbox"));
    }

    #[test]
    fn a_failed_run_says_nothing_was_kept() {
        let run = ToolboxRun {
            action: ToolboxAction::Update,
            at: "2026-10-30T12:00:00Z".into(),
            template: None,
            uncovered: vec![],
            error: Some("`false` exited with code 1".into()),
        };
        assert_eq!(
            describe_run(&run),
            "The update failed at 2026-10-30T12:00:00Z, and nothing was kept: `false` exited \
             with code 1"
        );
        assert_eq!(
            describe_saved(&status()),
            "Saved template 3 and Linux home 2: new linux agents boot them; running and \
             sleeping agents keep theirs."
        );
    }
}
