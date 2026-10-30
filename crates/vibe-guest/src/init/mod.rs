//! The guest proper: Linux only.

mod net;
mod root;
mod serve;
mod session;
mod sys;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use vibe_guest::boot::Boot;
use vibe_guest::proto::{CONTROL_PORT, ShareMount, TERMINAL_PORT};

use crate::USAGE;
use serve::{Guest, bind_patiently, serve};
use session::{Agent, Settings};

/// Writes a line to the guest's log: stderr, which is the console, which
/// `vibe-vm` appends to the agent's `console.log` on the host. Never log a
/// launch's environment: it holds the Claude token.
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::init::write_log(format_args!($($arg)*))
    };
}
pub(crate) use log;

pub(crate) fn write_log(args: std::fmt::Arguments<'_>) {
    let ms = sys::uptime_ms();
    let _ = writeln!(
        std::io::stderr().lock(),
        "[{:5}.{:03}] vibe-guest: {args}",
        ms / 1000,
        ms % 1000
    );
}

/// How the guest was asked to run.
#[derive(Debug, PartialEq, Eq)]
struct TestMode {
    vsock_dir: PathBuf,
    cgroup: Option<PathBuf>,
    shares: Vec<ShareMount>,
}

fn parse_args(args: &[String]) -> Result<Option<TestMode>, String> {
    if args.is_empty() {
        return Ok(None);
    }
    let mut vsock_dir = None;
    let mut cgroup = None;
    let mut shares = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let mut value = || iter.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--vsock-dir" => vsock_dir = Some(PathBuf::from(value()?)),
            "--cgroup" => cgroup = Some(PathBuf::from(value()?)),
            "--share" => {
                let (boot, problems) = Boot::parse(&format!("vibe.share={}", value()?));
                if let Some(p) = problems.into_iter().next() {
                    return Err(p);
                }
                shares.extend(boot.shares.into_iter().map(|s| ShareMount {
                    tag: s.tag,
                    path: s.path,
                }));
            }
            other => return Err(format!("unexpected argument: {other}")),
        }
    }
    Ok(Some(TestMode {
        vsock_dir: vsock_dir.ok_or("missing --vsock-dir")?,
        cgroup,
        shares,
    }))
}

pub(crate) fn main(args: Vec<String>) -> ExitCode {
    // The kernel hands PID 1 the command line's words it doesn't know, and
    // PID 1 must never return: as such it takes no arguments, and says so.
    let pid1 = std::process::id() == 1;
    let test = if pid1 {
        if !args.is_empty() {
            log!("ignoring the arguments the kernel passed: {args:?}");
        }
        None
    } else {
        match parse_args(&args) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("vibe-guest: {e}\n\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    };
    if test.is_none() && std::process::id() != 1 {
        eprintln!("vibe-guest: runs as PID 1 of a linux VM\n\n{USAGE}");
        return ExitCode::from(2);
    }
    // Before any thread starts, so they all inherit it.
    session::block_sigchld();
    let network = Arc::new(Mutex::new(None));
    let (guest, unix_dir) = match test {
        None => {
            log!("vibe-guest {} starting", env!("CARGO_PKG_VERSION"));
            let setup = root::setup();
            net::start(network.clone());
            let settings = Settings {
                user: Some((setup.uid, setup.gid)),
                home: PathBuf::from(root::HOME),
                env: setup.env,
                cgroup: setup.cgroup,
            };
            let guest = Guest {
                agent: Arc::new(Agent::new(settings)),
                pid1: true,
                layers: setup.layers,
                shares: setup.shares,
                problems: Mutex::new(setup.problems),
                network,
            };
            (guest, None)
        }
        Some(test) => {
            // Orphans come to it, as they would to PID 1.
            // SAFETY: prctl with integer arguments has no memory effects.
            unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) };
            let mut env: Vec<(String, String)> = std::env::vars().collect();
            env.retain(|(k, _)| k != "TERM");
            env.push(("TERM".into(), "xterm-256color".into()));
            let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
            let settings = Settings {
                user: None,
                home,
                env,
                cgroup: test.cgroup,
            };
            let guest = Guest {
                agent: Arc::new(Agent::new(settings)),
                pid1: false,
                layers: Vec::new(),
                shares: test.shares,
                problems: Mutex::new(Vec::new()),
                network,
            };
            (guest, Some(test.vsock_dir))
        }
    };
    let guest = Arc::new(guest);
    for (port, name, handle) in [
        (
            CONTROL_PORT,
            "control",
            serve::control as fn(&Guest, std::fs::File),
        ),
        (TERMINAL_PORT, "terminal", serve::terminal),
    ] {
        let guest = guest.clone();
        let dir = unix_dir.clone();
        let started = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || serve(guest, bind_patiently(port, dir), name, handle));
        if let Err(e) = started {
            log!("can't start the {name} port: {e}");
        }
    }
    // PID 1 never returns: the main thread reaps.
    session::reap(guest.agent.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<TestMode>, String> {
        parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn args() {
        assert_eq!(parse(&[]), Ok(None));
        assert_eq!(
            parse(&["--vsock-dir", "/d", "--cgroup", "/c", "--share", "w:/a%20b"]),
            Ok(Some(TestMode {
                vsock_dir: "/d".into(),
                cgroup: Some("/c".into()),
                shares: vec![ShareMount {
                    tag: "w".into(),
                    path: "/a b".into()
                }],
            }))
        );
        assert!(parse(&["--cgroup", "/c"]).is_err());
        assert!(parse(&["--vsock-dir"]).is_err());
        assert!(parse(&["--vsock-dir", "/d", "--share", "nopath"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
    }
}
