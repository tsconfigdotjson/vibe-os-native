//! `vibed`: serves the JSON-RPC socket until SIGTERM or SIGINT.

use std::ffi::OsString;
use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use vibe_core::paths::{self, Layout};
use vibe_core::rpc::MAX_MESSAGE_BYTES;
use vibe_env::testing::TestBackend;
use vibed::agents::Backends;
use vibed::core::{Config, Core, DaemonInfo};
use vibed::{logging, server};

const USAGE: &str = "\
usage: vibed [--socket PATH]

Serves the VibeOS JSON-RPC API on a Unix socket until SIGTERM or SIGINT.
State lives under $HOME: on macOS in ~/Library/Application Support/VibeOS
(logs in ~/Library/Logs/VibeOS), elsewhere in ~/.local/share/vibeos. Host
limits are in ~/.config/vibeos/host.toml.

options:
  --socket PATH   the socket to serve (default: $VIBE_SOCKET, or run/vibed.sock
                  in the state directory)
  -V, --version   print the version
  -h, --help      print this help

environment:
  VIBE_GH         the gh to run (default: gh on PATH)
  VIBE_SHIM       the vibe-shim to start sessions with (default: the one
                  beside vibed, or on PATH)
  VIBE_TEST_BACKEND
                  for tests only: run every environment's agents natively,
                  unlimited but for a supervisor, with this program as
                  Claude Code";

/// How often `gh auth status` runs (PRD: Auth, security and trust).
const GH_CHECK_EVERY: Duration = Duration::from_secs(60 * 60);

/// How often projects whose agents are all at rest get `git maintenance`.
const MAINTENANCE_EVERY: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, PartialEq, Eq)]
enum Args {
    Run { socket: Option<PathBuf> },
    Version,
    Help,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut socket = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-V" | "--version" => return Ok(Args::Version),
            "-h" | "--help" => return Ok(Args::Help),
            "--socket" => {
                let path = iter.next().ok_or("--socket needs a path")?;
                socket = Some(PathBuf::from(path));
            }
            other => match other.strip_prefix("--socket=") {
                Some(path) => socket = Some(PathBuf::from(path)),
                None => return Err(format!("unexpected argument: {other}")),
            },
        }
    }
    Ok(Args::Run { socket })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let socket = match parse_args(&args) {
        Ok(Args::Run { socket }) => socket,
        Ok(Args::Version) => {
            println!("vibed {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Ok(Args::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            vibed::log!("vibed: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let layout = match Layout::current() {
        Ok(l) => l,
        Err(e) => {
            vibed::log!("vibed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let path = match paths::socket_path(socket) {
        Ok(p) => p,
        Err(e) => {
            vibed::log!("vibed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let logs = layout.logs_dir();
    let logged = DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&logs)
        .and_then(|()| logging::to_file(&logs.join("vibed.log")));
    if let Err(e) = logged {
        vibed::log!("vibed: warning: can't log to {}: {e}", logs.display());
    }
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            vibed::log!("vibed: can't start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(path, layout)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            vibed::log!("vibed: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(path: PathBuf, layout: Layout) -> Result<(), Box<dyn std::error::Error>> {
    // Install the handlers before the socket exists, so a client that sees
    // the socket can always stop the daemon cleanly.
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let (listener, guard) = server::bind(&path)?;
    let listener = UnixListener::from_std(listener)?;
    let mut config = Config::new(DaemonInfo::current(), layout);
    if let Some(gh) = std::env::var_os("VIBE_GH").filter(|v| !v.is_empty()) {
        config.gh = gh;
    }
    if let Some(shim) = std::env::var_os("VIBE_SHIM").filter(|v| !v.is_empty()) {
        config.shim = shim;
    }
    if let Some(claude) = std::env::var_os("VIBE_TEST_BACKEND").filter(|v| !v.is_empty()) {
        vibed::log!(
            "vibed: VIBE_TEST_BACKEND is set: agents run under the test backend, with {} as \
             Claude Code",
            PathBuf::from(&claude).display()
        );
        config.backends = Backends::all(Arc::new(TestBackend::new(PathBuf::from(claude))));
    }
    let gh: OsString = config.gh.clone();
    let core = Arc::new(Core::open(config)?);
    vibed::log!(
        "vibed: state in {}, gh is {}",
        core.layout().data_dir().display(),
        gh.to_string_lossy()
    );
    core.warn_about_limits();
    // In the background, so a shim that's slow to answer doesn't hold up
    // the socket.
    let adopting = {
        let core = Arc::clone(&core);
        tokio::spawn(async move {
            let adopted = core.adopt_sessions().await;
            vibed::log!(
                "vibed: {adopted} running session{} found",
                if adopted == 1 { "" } else { "s" }
            );
        })
    };
    let gh_checks = core.spawn_gh_checks(GH_CHECK_EVERY);
    let maintenance = core.spawn_maintenance(MAINTENANCE_EVERY);
    vibed::log!(
        "vibed {} listening on {}",
        env!("CARGO_PKG_VERSION"),
        path.display()
    );
    let shutdown = async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    };
    server::serve(listener, core, MAX_MESSAGE_BYTES, shutdown).await;
    gh_checks.abort();
    maintenance.abort();
    adopting.abort();
    drop(guard);
    vibed::log!("vibed: stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn args() {
        assert_eq!(parse(&[]), Ok(Args::Run { socket: None }));
        assert_eq!(
            parse(&["--socket", "/s"]),
            Ok(Args::Run {
                socket: Some("/s".into())
            })
        );
        assert_eq!(
            parse(&["--socket=/s"]),
            Ok(Args::Run {
                socket: Some("/s".into())
            })
        );
        assert_eq!(parse(&["-V"]), Ok(Args::Version));
        assert_eq!(parse(&["--help"]), Ok(Args::Help));
        assert!(parse(&["--socket"]).is_err());
        assert!(parse(&["serve"]).is_err());
    }
}
