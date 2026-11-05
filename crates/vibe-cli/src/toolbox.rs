//! `vibe toolbox`: the Linux toolbox shell (PRD: The toolbox shell). It
//! asks `vibed` to open the toolbox and attaches this terminal to its
//! shell; when the shell exits, `vibed` saves a new template and Linux home
//! version, and this waits to say which. `Ctrl-\` detaches and leaves the
//! shell open, for `vibe toolbox` to attach to again. Closing the window
//! saves and closes it, as Save & Close does, rather than leave it open
//! with no terminal (#54); unless another terminal is still attached to it.
//! Over SSH a hangup only detaches: a dropped connection says nothing about
//! wanting the shell ended and saved.
//!
//! `vibe toolbox status`, `update`, `rebuild` and `close` show the
//! versions, run `toolbox.toml`'s `[update]` now, rebuild the template on
//! the newest base image, and save and close an open shell.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use vibe_core::model::{LoginTool, SessionState, ToolboxAction, ToolboxState};
use vibe_core::rpc::{
    HostParams, NoParams, SessionAttach, SessionInfo, SessionsList, ToolboxClose, ToolboxLogins,
    ToolboxLoginsGet, ToolboxOpen, ToolboxRebuild, ToolboxRun, ToolboxStatus, ToolboxStatusGet,
    ToolboxUpdate,
};

use crate::attach::{self, AttachError, End};
use crate::client;

/// How long a boot of the toolbox may take, its disks' clones included.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// How long an update or a rebuild may take: installs download a lot.
const RUN_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);

/// How long saving may take once the shell exits: flushing a big install.
const SAVE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// How long, after a hangup, to keep trying to reach `vibed` and save: a
/// shim or `vibed` under heavy swap can be slow to answer, and nobody is
/// left to tell if this gives up.
const LEAVING: Duration = Duration::from_secs(60);

/// The toolbox shell's session.
const TOOLBOX_REF: &str = "toolbox";

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
                return report_saved(&after);
            }
            // Closed, but with changes a failed save kept for next time.
            match after
                .last
                .as_ref()
                .filter(|r| r.action == ToolboxAction::Shell)
            {
                Some(ToolboxRun { error: Some(e), .. }) => Err(format!(
                    "the toolbox isn't open, but its last save failed: {e}"
                )),
                _ => {
                    println!("The toolbox isn't open: there's nothing to save.");
                    Ok(())
                }
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
    // Before the boot, which can take a while: the window may close during
    // it, and that closes what it's booting too.
    attach::catch_hangup().map_err(|e| format!("the terminal: {e}"))?;
    let terminal = attach::tty_name();
    eprintln!("Opening the Linux toolbox…");
    let opened = client::call::<ToolboxOpen>(socket, &NoParams {}, OPEN_TIMEOUT);
    if attach::hung_up() {
        after_hangup(socket, terminal.as_deref());
        return Ok(());
    }
    let mut found = opened.map_err(|e| e.to_string())?;
    say(&format!(
        "What you install here (sudo apt install …, curl … | sh, npm -g) and log in to \
         (gh auth login) every new linux agent gets. {}",
        if over_ssh() {
            "Exit the shell to save; Ctrl-\\ or a dropped connection leaves it open."
        } else {
            "Exit the shell to save, or close the window; Ctrl-\\ leaves it open."
        }
    ));
    let mut lost = 0;
    loop {
        match attach::attach_to(&found) {
            // It was a terminal when this began, so it went since.
            Ok(End::HungUp) | Err(AttachError::NotATerminal) => {
                after_hangup(socket, terminal.as_deref());
                return Ok(());
            }
            // Whatever else went wrong, the window has closed.
            Err(_) if attach::hung_up() => {
                after_hangup(socket, terminal.as_deref());
                return Ok(());
            }
            Err(e) => return Err(e.to_string()),
            Ok(End::Detached) => {
                say(
                    "The toolbox is still open: `vibe toolbox` attaches to it again. Exit its \
                     shell, or run `vibe toolbox close`, to save.",
                );
                return Ok(());
            }
            Ok(End::Exited(_)) => break,
            // The shim went without saying why. If the shell runs on, vibed
            // holds it again: attach to that.
            Ok(End::Lost) => {
                std::thread::sleep(Duration::from_secs(2));
                match held_again(socket, lost) {
                    Ok(Some(again)) => {
                        lost += 1;
                        found = again;
                    }
                    Ok(None) => break,
                    Err(_) if attach::hung_up() => {
                        after_hangup(socket, terminal.as_deref());
                        return Ok(());
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }
    say("Saving the template and the Linux home…");
    report_saved(&wait_saved(socket)?)
}

/// Where the shell is held again after its shim went, `lost` times now;
/// `None` if it isn't to be attached to again.
fn held_again(socket: &Path, lost: u32) -> Result<Option<SessionAttach>, String> {
    if lost >= 3 || status(socket)?.state != ToolboxState::Open {
        return Ok(None);
    }
    client::call::<ToolboxOpen>(socket, &NoParams {}, OPEN_TIMEOUT)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// A line for the owner on stderr, if the terminal is still there: unlike
/// `eprintln!`, a window that has closed doesn't panic it.
fn say(line: &str) {
    let _ = writeln!(std::io::stderr(), "{line}");
}

/// Whether this runs over SSH, where a hangup can be the network dropping.
fn over_ssh() -> bool {
    ["SSH_CONNECTION", "SSH_TTY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

/// Says what a save kept, or why it kept nothing.
fn report_saved(status: &ToolboxStatus) -> Result<(), String> {
    match &status.last {
        Some(run) if run.action == ToolboxAction::Shell => match &run.error {
            Some(e) => Err(format!("the toolbox couldn't save: {e}")),
            None => {
                let _ = writeln!(std::io::stdout(), "{}", describe_saved(status));
                Ok(())
            }
        },
        _ => Err("the toolbox closed, but vibed didn't say what it saved".into()),
    }
}

/// After the terminal hung up: Save & Close, unless this is over SSH or
/// another terminal still holds the shell. `terminal` is this one's name,
/// noted while it was there. Nobody sees this terminal any more, so an
/// error goes only where stderr still leads, if anywhere.
fn after_hangup(socket: &Path, terminal: Option<&str>) {
    if over_ssh() {
        return;
    }
    let deadline = Instant::now() + LEAVING;
    loop {
        // On every try: someone may have attached since the last one failed.
        if another_terminal_holds_it(socket, terminal, deadline) {
            return;
        }
        match client::call::<ToolboxClose>(socket, &NoParams {}, SAVE_TIMEOUT) {
            Ok(_) => return,
            Err(e) if Instant::now() >= deadline => {
                say(&format!("vibe: the toolbox couldn't save: {e}"));
                return;
            }
            Err(_) => std::thread::sleep(Duration::from_secs(1)),
        }
    }
}

/// Whether a terminal other than `terminal` is attached to the shell. If
/// that still can't be told at `deadline`, it's taken as yes: the shell
/// stays open, which Settings shows and loses nothing, rather than being
/// ended under a terminal that may be in use.
fn another_terminal_holds_it(socket: &Path, terminal: Option<&str>, deadline: Instant) -> bool {
    loop {
        match who_holds_it(socket, terminal) {
            Some(answer) => return answer,
            None if Instant::now() >= deadline => {
                say("vibe: can't tell who's attached to the toolbox; leaving it open");
                return true;
            }
            None => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

/// One look at who's attached; `None` if it can't be told yet. The list can
/// still show this terminal, as a shim under load is slow to see it go, so
/// it goes by whose terminals they are, not how many.
fn who_holds_it(socket: &Path, terminal: Option<&str>) -> Option<bool> {
    let list =
        client::call::<SessionsList>(socket, &HostParams::default(), Duration::from_secs(30))
            .ok()?;
    let session = list
        .sessions
        .iter()
        .find(|s| s.agent == TOOLBOX_REF && s.state == SessionState::Running);
    let Some(session) = session else {
        return Some(false);
    };
    let answer = holds_it(session, terminal)?;
    // The list gives a shim that didn't answer as having no clients: that
    // isn't "nobody" unless the shim itself says so. A count it gives now
    // that isn't zero means the list was out of date: look again.
    if !answer && session.clients == 0 && !shim_says_nobody(socket) {
        return None;
    }
    Some(answer)
}

/// Whether `vibed` asked the shell's shim and it counted no terminals, or
/// the toolbox isn't open any more. `clients` is there only if it answered.
fn shim_says_nobody(socket: &Path) -> bool {
    status(socket).is_ok_and(|s| s.state != ToolboxState::Open || s.clients == Some(0))
}

/// Whether `session` has a terminal attached other than `terminal`; `None`
/// if that can't be told yet. A client with no terminal name can be told
/// from this one only if this one has a name.
fn holds_it(session: &SessionInfo, terminal: Option<&str>) -> Option<bool> {
    if session.ttys.iter().any(|t| Some(t.as_str()) != terminal) {
        return Some(true);
    }
    let unnamed = session.clients as usize > session.ttys.len();
    match (terminal, unnamed) {
        (Some(_), unnamed) => Some(unnamed),
        (None, false) => Some(false),
        // One of them may be this one, until the shim sees it go.
        (None, true) => None,
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
    if s.state == ToolboxState::Open && s.clients == Some(0) {
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
            let how = if s.rebuild_held.is_empty() {
                "VibeOS moves the template to it by itself"
            } else {
                "the template is waiting"
            };
            let _ = writeln!(out, "base      {b}; base {n} is installed, and {how}");
        }
        (Some(b), _) => {
            let _ = writeln!(out, "base      {b}");
        }
        (None, _) => {
            let _ = writeln!(out, "base      not downloaded yet");
        }
    }
    if !s.rebuild_held.is_empty() {
        let _ = writeln!(
            out,
            "waiting   moving would lose what toolbox.toml doesn't cover: {}. `vibe toolbox \
             rebuild` moves it anyway",
            s.rebuild_held.join(", ")
        );
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
            rebuild_held: Vec::new(),
            last: None,
            clients: None,
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
            text.contains("base 2 is installed, and VibeOS moves the template"),
            "{text}"
        );
        assert!(
            text.contains("last      The rebuild saved template 3 at 2026-10-30T12:00:00Z\n"),
            "{text}"
        );
        assert!(text.contains("\n            apt: htop\n"), "{text}");
        // Open with no terminal attached says so, and how to go on.
        s.state = ToolboxState::Open;
        // Not counted (a boot under way, a shim too slow to ask, an older
        // vibed): it doesn't claim either way.
        assert!(describe_status(&s).starts_with("state     open\n"));
        s.clients = Some(0);
        assert!(
            describe_status(&s).starts_with(
                "state     open in the background, no terminal attached: `vibe toolbox` attaches"
            ),
            "{}",
            describe_status(&s)
        );
        s.clients = Some(1);
        assert!(describe_status(&s).starts_with("state     open\n"));
        let none = ToolboxStatus {
            state: ToolboxState::Unavailable,
            template: None,
            home: None,
            base: None,
            newest_base: None,
            rebuild_held: Vec::new(),
            last: None,
            clients: None,
        };
        assert!(describe_status(&none).contains("no Linux toolbox"));
    }

    #[test]
    fn who_holds_the_shell_goes_by_the_terminals_names() {
        let session = |clients, ttys: &[&str]| SessionInfo {
            host: "local".into(),
            agent: TOOLBOX_REF.into(),
            state: SessionState::Running,
            title: "Linux toolbox".into(),
            clients,
            ttys: ttys.iter().map(|t| (*t).to_owned()).collect(),
            exit: None,
        };
        let me = Some("/dev/ttys001");
        // Nobody; or only this one, which the shim hasn't seen go.
        assert_eq!(holds_it(&session(0, &[]), me), Some(false));
        assert_eq!(holds_it(&session(1, &["/dev/ttys001"]), me), Some(false));
        // Another, at once, with this one still listed or not.
        assert_eq!(holds_it(&session(1, &["/dev/ttys002"]), me), Some(true));
        let both = session(2, &["/dev/ttys001", "/dev/ttys002"]);
        assert_eq!(holds_it(&both, me), Some(true));
        // One with no terminal name isn't this one, which has a name.
        assert_eq!(holds_it(&session(1, &[]), me), Some(true));
        // With no name of its own, this one might be it: wait and see.
        assert_eq!(holds_it(&session(1, &[]), None), None);
        assert_eq!(holds_it(&session(0, &[]), None), Some(false));
        assert_eq!(holds_it(&session(1, &["/dev/ttys002"]), None), Some(true));
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
