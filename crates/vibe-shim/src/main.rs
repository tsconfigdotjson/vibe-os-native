//! `vibe-shim`: one per awake agent. It holds the agent's terminal, a
//! scrollback ring and a screen model, and serves `run/<ref>.sock` (PRD:
//! Architecture), so the session outlives every client, `vibed` included.
//!
//! It detaches like `dtach -n`: it binds the socket, forks a process that
//! leads a session of its own and starts the command on a new terminal,
//! prints that process's pid once the command is running, and exits.
//!
//! For a `linux` agent on the Mac the command runs in the VM instead, on
//! `vibe-guest`'s terminal: with `--vm run/<ref>.vm` the shim attaches to
//! that through `vibe-vm` rather than starting anything. If the stream
//! drops (the VM went to sleep), the shim exits and leaves the command
//! running; the next shim picks the terminal up where it was.

mod pty;
mod screen;
mod serve;

use std::ffi::OsString;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::OnceLock;

use vibe_core::model::validate_ref;
use vibe_core::shim::{self, Size};
use vibe_core::socket;
use vibe_core::time::{now, rfc3339};
use vibe_env::guest::{self, TermSize};

use crate::screen::DEFAULT_HISTORY;
use crate::serve::Session;

const USAGE: &str = "\
usage: vibe-shim --ref REF --socket PATH --state DIR [options] -- COMMAND [ARG...]
       vibe-shim --ref REF --socket PATH --state DIR --vm VMSOCKET [options]

Starts COMMAND on a new terminal in a detached session, serves that terminal
on the Unix socket PATH for `vibe attach` and vibed, prints the shim's pid
once COMMAND is running, and exits. vibed starts shims; run one by hand only
to test. When COMMAND exits, the shim writes session.json in DIR, removes
the socket and exits too. Its log is shim.log in DIR, and COMMAND's TMPDIR is
tmp/ in DIR.

With --vm, the command is a linux agent's, already running in its VM: the
shim serves vibe-guest's terminal, reached through the vibe-vm serving
VMSOCKET (run/REF.vm). If that stream drops, or the shim is asked to stop,
it exits without a record and the command goes on in the VM.

options:
  --ref REF           the agent's ref, which the session goes by
  --socket PATH       the socket to serve, normally run/REF.sock
  --state DIR         where the log and the record go, created if needed
  --vm VMSOCKET       serve the terminal of vibe-guest in this VM
  --cwd DIR           where COMMAND starts (default: here; not with --vm)
  --size COLSxROWS    the terminal's size until a client attaches (default:
                      80x24, or with --vm the size it has)
  --history LINES     lines that scrolled off the screen to keep (default: 10000)
  -V, --version       print the version
  -h, --help          print this help";

#[derive(Debug, PartialEq, Eq)]
struct Options {
    agent: String,
    socket: PathBuf,
    state: PathBuf,
    cwd: Option<PathBuf>,
    size: Size,
    /// Whether `--size` was given, rather than the default.
    size_given: bool,
    history: usize,
    /// For a `linux` agent: the `vibe-vm` socket to reach its guest by.
    vm: Option<PathBuf>,
    command: Vec<OsString>,
}

#[derive(Debug, PartialEq, Eq)]
enum Args {
    Run(Options),
    Version,
    Help,
}

fn parse_size(s: &str) -> Result<Size, String> {
    let bad = || format!("--size takes COLSxROWS, such as 120x40, not `{s}`");
    let (cols, rows) = s.split_once('x').ok_or_else(bad)?;
    let size = Size {
        rows: rows.parse().map_err(|_| bad())?,
        cols: cols.parse().map_err(|_| bad())?,
    };
    if size.is_valid() {
        Ok(size)
    } else {
        Err(bad())
    }
}

fn parse_args(args: &[OsString]) -> Result<Args, String> {
    let mut agent = None;
    let mut socket = None;
    let mut state = None;
    let mut cwd = None;
    let mut size = Size::DEFAULT;
    let mut size_given = false;
    let mut history = DEFAULT_HISTORY;
    let mut vm = None;
    let mut iter = args.iter();
    let command: Vec<OsString> = loop {
        let Some(arg) = iter.next() else {
            if vm.is_some() {
                break Vec::new();
            }
            return Err("missing `-- COMMAND`".into());
        };
        let arg = arg
            .to_str()
            .ok_or_else(|| format!("unexpected argument: {}", arg.to_string_lossy()))?;
        let mut value = |name: &str| {
            iter.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        let text = |v: OsString, name: &str| {
            v.into_string()
                .map_err(|_| format!("{name} must be valid UTF-8"))
        };
        match arg {
            "-V" | "--version" => return Ok(Args::Version),
            "-h" | "--help" => return Ok(Args::Help),
            "--ref" => agent = Some(text(value(arg)?, arg)?),
            "--socket" => socket = Some(PathBuf::from(value(arg)?)),
            "--state" => state = Some(PathBuf::from(value(arg)?)),
            "--cwd" => cwd = Some(PathBuf::from(value(arg)?)),
            "--size" => {
                size = parse_size(&text(value(arg)?, arg)?)?;
                size_given = true;
            }
            "--vm" => vm = Some(PathBuf::from(value(arg)?)),
            "--history" => {
                let v = text(value(arg)?, arg)?;
                history = v
                    .parse()
                    .map_err(|_| format!("--history takes a number of lines, not `{v}`"))?;
            }
            "--" => break iter.cloned().collect(),
            other => return Err(format!("unexpected argument: {other}")),
        }
    };
    let agent = agent.ok_or("missing --ref")?;
    validate_ref(&agent)?;
    if vm.is_some() {
        if !command.is_empty() {
            return Err("with --vm the guest runs the command; give none".into());
        }
        if cwd.is_some() {
            return Err("with --vm the command's directory is the guest's; drop --cwd".into());
        }
    } else if command.is_empty() {
        return Err("missing the COMMAND after `--`".into());
    }
    Ok(Args::Run(Options {
        agent,
        socket: socket.ok_or("missing --socket")?,
        state: state.ok_or("missing --state")?,
        cwd,
        size,
        size_given,
        history,
        vm,
        command,
    }))
}

static REF: OnceLock<String> = OnceLock::new();

/// Writes a line to the shim's log: stderr, which is `shim.log` once the
/// shim has detached.
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::write_log(format_args!($($arg)*))
    };
}
pub(crate) use log;

pub(crate) fn write_log(args: std::fmt::Arguments<'_>) {
    let agent = REF.get().map_or("?", String::as_str);
    let _ = writeln!(
        io::stderr().lock(),
        "{} vibe-shim[{agent}]: {args}",
        rfc3339(now())
    );
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match parse_args(&args) {
        Ok(Args::Run(options)) => match start(options) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("vibe-shim: {e}");
                ExitCode::FAILURE
            }
        },
        Ok(Args::Version) => {
            println!("vibe-shim {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Args::Help) => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("vibe-shim: {e}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn check(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

/// Binds the socket, forks the shim, and reports whether its command
/// started. Runs before any thread exists, so forking is safe.
fn start(options: Options) -> Result<ExitCode, String> {
    REF.set(options.agent.clone()).expect("set once");
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&options.state)
        .map_err(|e| format!("can't create {}: {e}", options.state.display()))?;
    let log_path = options.state.join(shim::LOG_FILE);
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_path)
        .map_err(|e| format!("can't open {}: {e}", log_path.display()))?;
    let cwd = match &options.cwd {
        Some(d) => d.clone(),
        None => std::env::current_dir().map_err(|e| format!("no current directory: {e}"))?,
    };
    let (listener, guard) = bind(&options.socket)?;

    let mut fds = [0; 2];
    // SAFETY: fds has room for the two descriptors pipe writes.
    check(unsafe { libc::pipe(fds.as_mut_ptr()) }).map_err(|e| format!("pipe: {e}"))?;
    // SAFETY: pipe just made these, and nothing else owns them.
    let (ready_read, ready_write) =
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in [&ready_read, &ready_write] {
        // SAFETY: plain fcntl on descriptors we own. Not inherited by the
        // command, so the launcher never waits on it.
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    }

    // SAFETY: the process has one thread, so the child may do anything.
    match check(unsafe { libc::fork() }).map_err(|e| format!("fork: {e}"))? {
        0 => {
            drop(ready_read);
            let code = detached(options, cwd, listener, guard, log_file, ready_write);
            std::process::exit(code);
        }
        pid => {
            drop(ready_write);
            // The child owns the socket now; this copy mustn't remove it.
            std::mem::forget(guard);
            drop(listener);
            let mut line = String::new();
            let _ = BufReader::new(File::from(ready_read)).read_line(&mut line);
            let line = line.trim_end();
            if line == "ok" {
                println!("{pid}");
                return Ok(ExitCode::SUCCESS);
            }
            match line.strip_prefix("error ") {
                Some(message) => Err(message.to_owned()),
                None => Err(format!(
                    "the session didn't start; see {}",
                    log_path.display()
                )),
            }
        }
    }
}

/// The longest socket path the OS takes, without its terminating NUL.
const MAX_SOCKET_PATH: usize = if cfg!(target_vendor = "apple") {
    103
} else {
    107
};

/// Binds the session's socket. Its lock may be held for a moment by `vibed`
/// clearing a stale socket, so a held lock is tried again briefly before
/// it means another shim is running.
fn bind(path: &std::path::Path) -> Result<(UnixListener, socket::SocketGuard), String> {
    let len = path.as_os_str().len();
    if len > MAX_SOCKET_PATH {
        return Err(format!(
            "the socket path {} is {len} bytes; this OS takes at most {MAX_SOCKET_PATH}",
            path.display()
        ));
    }
    let mut tries = 0;
    loop {
        match socket::bind(path) {
            Ok(bound) => return Ok(bound),
            Err(socket::BindError::AlreadyRunning { .. }) if tries < 25 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(socket::BindError::AlreadyRunning { path }) => {
                return Err(format!(
                    "a session is already running on {}",
                    path.display()
                ));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// The shim proper: a session leader with no terminal of its own, its
/// output going to its log.
fn detached(
    options: Options,
    cwd: PathBuf,
    listener: UnixListener,
    guard: socket::SocketGuard,
    log_file: File,
    ready: OwnedFd,
) -> i32 {
    let mut ready = File::from(ready);
    let mut report = |line: &str| {
        let _ = writeln!(ready, "{line}");
    };
    // SAFETY: setsid has no memory effects; we're a fresh child, never a
    // process group leader, so it can't fail with EPERM.
    if let Err(e) = check(unsafe { libc::setsid() }) {
        report(&format!("error setsid: {e}"));
        return 1;
    }
    if let Err(e) = redirect(&log_file) {
        report(&format!("error can't redirect output to the log: {e}"));
        return 1;
    }
    // SAFETY: setting a signal's disposition has no memory effects.
    unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
    // The shim is in the terminal's session but not its foreground, and its
    // process group is orphaned. On macOS that makes its own ioctls on the
    // terminal, such as a resize through the master, fail with EIO unless
    // SIGTTOU is ignored; reading would do the same with SIGTTIN. The
    // command gets both back at their defaults.
    // SAFETY: as above.
    unsafe {
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        libc::signal(libc::SIGTTIN, libc::SIG_IGN);
    }
    let started = match &options.vm {
        None => start_command(&options, &cwd),
        Some(vm) => attach_guest(&options, vm),
    };
    let started = match started {
        Ok(s) => s,
        Err(message) => {
            log!("{message}");
            // The socket goes before the launcher hears, so a retry finds
            // it free.
            drop(listener);
            drop(guard);
            report(&format!("error {message}"));
            return 1;
        }
    };
    log!(
        "{} {:?} in {}{}, serving {}",
        if options.vm.is_some() {
            "attached to"
        } else {
            "started"
        },
        started.command,
        started.cwd,
        match &options.vm {
            Some(vm) => format!(
                " as pid {} in the VM of {}",
                started.child_pid,
                vm.display()
            ),
            None => format!(" as pid {}", started.child_pid),
        },
        options.socket.display()
    );
    // A new session supersedes the record of the last one.
    let _ = std::fs::remove_file(options.state.join(shim::RECORD_FILE));
    report("ok");
    drop(ready);

    let session = Session {
        agent: options.agent,
        command: started.command,
        cwd: started.cwd,
        state_dir: options.state,
        socket: options.socket,
        started_at: now(),
        history: options.history,
        size: started.size,
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log!("can't start the runtime: {e}");
            return 1;
        }
    };
    let result = runtime.block_on(serve::run(
        session,
        listener,
        started.end,
        started.child_pid,
    ));
    drop(guard);
    match result {
        Ok(_) => 0,
        Err(e) => {
            log!("{e}");
            1
        }
    }
}

/// The terminal the shim serves, and what's running on it.
#[derive(Debug)]
struct Started {
    end: serve::TerminalEnd,
    child_pid: u32,
    command: Vec<String>,
    cwd: String,
    size: Size,
}

/// Starts the command on a PTY.
fn start_command(options: &Options, cwd: &std::path::Path) -> Result<Started, String> {
    let command: Vec<String> = options
        .command
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    // The agent's temp files stay with the rest of what VibeOS keeps of
    // it, so deleting the agent takes them too (PRD: Delete).
    let tmp = options.state.join(vibe_core::paths::TMP_DIR);
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&tmp)
        .map_err(|e| format!("can't create {}: {e}", tmp.display()))?;
    let pty = pty::spawn(&options.command, cwd, &tmp, options.size)
        .map_err(|e| format!("can't start {}: {e}", command[0]))?;
    Ok(Started {
        child_pid: pty.child.id(),
        end: serve::TerminalEnd::Pty(pty),
        command,
        cwd: cwd.to_string_lossy().into_owned(),
        size: options.size,
    })
}

/// Attaches to the terminal of `vibe-guest` in the VM `vm` serves. Its
/// size is the guest's unless `--size` says otherwise.
fn attach_guest(options: &Options, vm: &std::path::Path) -> Result<Started, String> {
    let size = options.size_given.then_some(TermSize {
        rows: options.size.rows,
        cols: options.size.cols,
    });
    let terminal = guest::attach(vm, "vibe-shim", size)
        .map_err(|e| format!("can't attach to the guest's terminal: {e}"))?;
    let session = terminal.welcome.session;
    let size = terminal.welcome.size;
    Ok(Started {
        end: serve::TerminalEnd::Guest(terminal.stream),
        child_pid: session.pid,
        command: session.command,
        cwd: session.cwd,
        size: Size {
            rows: size.rows,
            cols: size.cols,
        },
    })
}

/// Points stdin and stdout at /dev/null and stderr at the log.
fn redirect(log: &File) -> io::Result<()> {
    let null = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    // SAFETY: dup2 onto the standard descriptors, from descriptors we own.
    unsafe {
        check(libc::dup2(null.as_raw_fd(), 0))?;
        check(libc::dup2(null.as_raw_fd(), 1))?;
        check(libc::dup2(log.as_raw_fd(), 2))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(&args.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn args() {
        let got = parse(&[
            "--ref",
            "quiet-amber-otter",
            "--socket",
            "/r/q.sock",
            "--state",
            "/a/q",
            "--cwd",
            "/w",
            "--size",
            "120x40",
            "--history",
            "5",
            "--",
            "claude",
            "--resume",
        ]);
        assert_eq!(
            got,
            Ok(Args::Run(Options {
                agent: "quiet-amber-otter".into(),
                socket: "/r/q.sock".into(),
                state: "/a/q".into(),
                cwd: Some("/w".into()),
                size: Size {
                    rows: 40,
                    cols: 120
                },
                size_given: true,
                history: 5,
                vm: None,
                command: vec!["claude".into(), "--resume".into()],
            }))
        );
        let Ok(Args::Run(defaults)) =
            parse(&["--ref", "r", "--socket", "s", "--state", "d", "--", "sh"])
        else {
            panic!("defaults")
        };
        assert_eq!(
            (defaults.size, defaults.size_given, defaults.history),
            (Size::DEFAULT, false, DEFAULT_HISTORY)
        );
        assert_eq!((defaults.cwd, defaults.vm), (None, None));
        // A linux agent's shim runs nothing itself.
        let Ok(Args::Run(o)) = parse(&[
            "--ref",
            "r",
            "--socket",
            "s",
            "--state",
            "d",
            "--vm",
            "/run/r.vm",
        ]) else {
            panic!("--vm")
        };
        assert_eq!((o.vm, o.command), (Some("/run/r.vm".into()), vec![]));
        let Ok(Args::Run(o)) = parse(&[
            "--ref", "r", "--socket", "s", "--state", "d", "--vm", "v", "--",
        ]) else {
            panic!("--vm and an empty --")
        };
        assert!(o.command.is_empty());
        // Everything after `--` is the command's, flags included.
        let Ok(Args::Run(o)) = parse(&["--ref", "r", "--socket", "s", "--state", "d", "--", "-V"])
        else {
            panic!("command flags")
        };
        assert_eq!(o.command, ["-V"]);
        assert_eq!(parse(&["-V"]), Ok(Args::Version));
        assert_eq!(parse(&["--help"]), Ok(Args::Help));
    }

    #[test]
    fn bad_args() {
        for bad in [
            &[][..],
            &["--ref", "r", "--socket", "s", "--state", "d"],
            &["--ref", "r", "--socket", "s", "--state", "d", "--"],
            &["--socket", "s", "--state", "d", "--", "sh"],
            &["--ref", "r", "--state", "d", "--", "sh"],
            &["--ref", "r", "--socket", "s", "--", "sh"],
            &["--ref", "a/b", "--socket", "s", "--state", "d", "--", "sh"],
            &[
                "--ref", "vibed", "--socket", "s", "--state", "d", "--", "sh",
            ],
            &[
                "--ref", "r", "--socket", "s", "--state", "d", "--size", "80", "--", "sh",
            ],
            &[
                "--ref", "r", "--socket", "s", "--state", "d", "--size", "0x24", "--", "sh",
            ],
            &[
                "--ref",
                "r",
                "--socket",
                "s",
                "--state",
                "d",
                "--history",
                "x",
                "--",
                "sh",
            ],
            &["--ref"],
            &["--bogus"],
            &[
                "--ref", "r", "--socket", "s", "--state", "d", "--vm", "v", "--", "sh",
            ],
            &[
                "--ref", "r", "--socket", "s", "--state", "d", "--vm", "v", "--cwd", "/w",
            ],
            &["--ref", "r", "--socket", "s", "--state", "d", "--vm"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}
