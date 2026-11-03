//! `vibe toolbox`: the Linux toolbox shell (PRD: The toolbox shell). It
//! asks `vibed` to open the toolbox and attaches this terminal to its
//! shell; when the shell exits, `vibed` saves a new template and Linux home
//! version, and this waits to say which. `Ctrl-\` detaches and leaves the
//! shell open, for `vibe toolbox` to attach to again. Closing the window
//! saves and closes it, as Save & Close does, rather than leave it open
//! with no terminal (#54); unless another terminal is still attached to it.
//!
//! `vibe toolbox status`, `update`, `rebuild` and `close` show the
//! versions, run `toolbox.toml`'s `[update]` now, rebuild the template on
//! the newest base image, and save and close an open shell.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use vibe_core::model::{LoginTool, ToolboxAction, ToolboxState};
use vibe_core::rpc::{
    NoParams, ToolboxClose, ToolboxLogins, ToolboxLoginsGet, ToolboxOpen, ToolboxRebuild,
    ToolboxRun, ToolboxStatus, ToolboxStatusGet, ToolboxUpdate,
};

use crate::attach::{self, End};
use crate::client;

/// How long a boot of the toolbox may take, its disks' clones included.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// How long an update or a rebuild may take: installs download a lot.
const RUN_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);

/// How long saving may take once the shell exits: flushing a big install.
const SAVE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// How long, after a hangup, the shim may take to see this terminal go,
/// before another terminal still attached is taken to hold the shell.
const LEAVING: Duration = Duration::from_secs(2);

/// Which `vibe toolbox`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Open,
    Status,
    Update,
    Rebuild,
    Logins,
    Close,
}

impl Command {
    pub(crate) fn parse(word: &str) -> Option<Command> {
        Some(match word {
            "status" => Command::Status,
            "update" => Command::Update,
            "rebuild" => Command::Rebuild,
            "logins" => Command::Logins,
            "close" => Command::Close,
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
        Command::Close => {
            let was_open = matches!(
                status(socket)?.state,
                ToolboxState::Open | ToolboxState::Saving
            );
            if was_open {
                eprintln!("Saving the template and the Linux home…");
            }
            let after = client::call::<ToolboxClose>(socket, &NoParams {}, SAVE_TIMEOUT)
                .map_err(|e| e.to_string())?;
            if was_open {
                report_saved(&after)
            } else {
                println!("The toolbox isn't open: there's nothing to save.");
                Ok(())
            }
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
         (gh auth login) every new linux agent gets. Exit the shell to save, or close the \
         window; Ctrl-\\ leaves it open."
    );
    let mut lost = 0;
    loop {
        match attach::attach_to(&found).map_err(|e| e.to_string())? {
            End::HungUp => {
                close_after_hangup(socket);
                return Ok(());
            }
            End::Detached => {
                eprintln!(
                    "The toolbox is still open: `vibe toolbox` attaches to it again. Exit its \
                     shell, or run `vibe toolbox close`, to save."
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
    report_saved(&wait_saved(socket)?)
}

/// Says what a save kept, or why it kept nothing.
fn report_saved(status: &ToolboxStatus) -> Result<(), String> {
    match &status.last {
        Some(run) if run.action == ToolboxAction::Shell => match &run.error {
            Some(e) => Err(format!("the toolbox couldn't save: {e}")),
            None => {
                println!("{}", describe_saved(status));
                Ok(())
            }
        },
        _ => Err("the toolbox closed, but vibed didn't say what it saved".into()),
    }
}

/// After the window closed: Save & Close, unless another terminal still
/// holds the shell. Nobody sees this terminal any more, so it says nothing
/// it can't, and an error goes only where stderr still leads, if anywhere.
fn close_after_hangup(socket: &Path) {
    let deadline = Instant::now() + LEAVING;
    loop {
        match status(socket) {
            Ok(s) if s.state != ToolboxState::Open => return,
            Ok(s) if s.clients == 0 => break,
            Ok(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            // Another terminal is attached: it keeps the shell open.
            Ok(_) => return,
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "vibe: {e}");
                return;
            }
        }
    }
    if let Err(e) = client::call::<ToolboxClose>(socket, &NoParams {}, SAVE_TIMEOUT) {
        let _ = writeln!(std::io::stderr(), "vibe: the toolbox couldn't save: {e}");
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
                "VibeOS couldn't tell which logins Linux home {home} holds when it was saved (the \
                 look failed, or an older VibeOS saved it). Open the toolbox (`vibe toolbox`) and \
                 exit, and they'll show here."
            );
        }
        (Some(home), Some(logins)) if logins.is_empty() => {
            let _ = writeln!(
                out,
                "VibeOS found no gh, gcloud, git or SSH logins in Linux home {home}. Open the \
                 toolbox (`vibe toolbox`), log in there (`gh auth login`, `gcloud auth login`, \
                 `git config --global user.name …`), and exit to save."
            );
        }
        (Some(home), Some(logins)) => {
            let _ = writeln!(
                out,
                "Every new linux agent starts with what Linux home {home} holds. Its gh, gcloud, \
                 git and SSH logins:"
            );
            for login in logins {
                let _ = writeln!(out, "  {:<14}{}", tool_name(login.tool), login.account);
            }
        }
    }
    match (&l.claude_problem, l.claude) {
        (_, true) => out.push_str("Claude: linux agents are logged in.\n"),
        (Some(why), false) => {
            let _ = writeln!(out, "Claude: {why}");
        }
        (None, false) => {
            out.push_str("Claude: linux agents aren't logged in yet. Run `vibe setup-token`.\n")
        }
    }
    out
}

pub(crate) fn describe_status(s: &ToolboxStatus) -> String {
    if s.state == ToolboxState::Unavailable {
        return "This host has no Linux toolbox: install tools on it directly.\n".into();
    }
    let mut out = String::new();
    if s.state == ToolboxState::Open && s.clients == 0 {
        let _ = writeln!(
            out,
            "state     open in the background, no terminal attached: `vibe toolbox` attaches to \
             it, `vibe toolbox close` saves it"
        );
    } else {
        let _ = writeln!(out, "state     {}", s.state);
    }
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
            clients: 0,
        }
    }

    #[test]
    fn subcommands() {
        assert_eq!(Command::parse("status"), Some(Command::Status));
        assert_eq!(Command::parse("update"), Some(Command::Update));
        assert_eq!(Command::parse("rebuild"), Some(Command::Rebuild));
        assert_eq!(Command::parse("logins"), Some(Command::Logins));
        assert_eq!(Command::parse("close"), Some(Command::Close));
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
            claude_problem: None,
        };
        assert_eq!(
            describe_logins(&l),
            "Every new linux agent starts with what Linux home 2 holds. Its gh, gcloud, git and \
             SSH logins:\n  \
             GitHub (gh)   octocat on github.com\n  \
             Git identity  Octo Cat <octo@example.com>\n\
             Claude: linux agents are logged in.\n"
        );
        l.logins = Some(vec![]);
        l.claude = false;
        let out = describe_logins(&l);
        assert!(
            out.starts_with("VibeOS found no gh, gcloud, git or SSH logins in Linux home 2."),
            "{out}"
        );
        assert!(out.ends_with("Run `vibe setup-token`.\n"), "{out}");
        // vibed's reason, when it has one: a Keychain that can't be read.
        l.claude_problem = Some("can't read the Claude login for linux agents: locked".into());
        let out = describe_logins(&l);
        assert!(
            out.ends_with("Claude: can't read the Claude login for linux agents: locked\n"),
            "{out}"
        );
        l.logins = None;
        assert!(describe_logins(&l).contains("couldn't tell which logins Linux home 2 holds"));
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
        // Open with no terminal attached says so, and how to go on.
        s.state = ToolboxState::Open;
        assert!(
            describe_status(&s).starts_with(
                "state     open in the background, no terminal attached: `vibe toolbox` attaches"
            ),
            "{}",
            describe_status(&s)
        );
        s.clients = 1;
        assert!(describe_status(&s).starts_with("state     open\n"));
        let none = ToolboxStatus {
            state: ToolboxState::Unavailable,
            template: None,
            home: None,
            base: None,
            newest_base: None,
            last: None,
            clients: 0,
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
