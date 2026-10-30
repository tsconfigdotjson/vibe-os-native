//! `vibed`: serves the JSON-RPC socket until SIGTERM or SIGINT.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use vibe_core::paths;
use vibe_core::rpc::MAX_MESSAGE_BYTES;
use vibed::dispatch::DaemonInfo;
use vibed::server;

const USAGE: &str = "\
usage: vibed [--socket PATH]

Serves the VibeOS JSON-RPC API on a Unix socket until SIGTERM or SIGINT.

options:
  --socket PATH   the socket to serve (default: $VIBE_SOCKET, or
                  ~/Library/Application Support/VibeOS/run/vibed.sock on macOS,
                  ~/.local/share/vibeos/run/vibed.sock elsewhere)
  -V, --version   print the version
  -h, --help      print this help";

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
    let path = match paths::socket_path(socket) {
        Ok(p) => p,
        Err(e) => {
            vibed::log!("vibed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            vibed::log!("vibed: can't start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(path)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            vibed::log!("vibed: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    // Install the handlers before the socket exists, so a client that sees
    // the socket can always stop the daemon cleanly.
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let (listener, guard) = server::bind(&path)?;
    let listener = UnixListener::from_std(listener)?;
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
    server::serve(
        listener,
        Arc::new(DaemonInfo::current()),
        MAX_MESSAGE_BYTES,
        shutdown,
    )
    .await;
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
