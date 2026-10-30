//! `vibe`: the command-line client of `vibed`.

mod client;

use std::io::{self, Write};
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use vibe_core::paths;
use vibe_core::rpc::{Hello, HelloParams, PROTOCOL_VERSION};

use crate::client::ClientError;

const USAGE: &str = "\
usage: vibe [--socket PATH] <command>

commands:
  hello   check that vibed is up, and print what it reports
  rpc     the vibed socket API on stdin and stdout, one JSON-RPC message per line

options:
  --socket PATH   vibed's socket (default: $VIBE_SOCKET, or the platform default)
  -V, --version   print the version
  -h, --help      print this help";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Hello,
    Rpc,
}

#[derive(Debug, PartialEq, Eq)]
enum Args {
    Run {
        socket: Option<PathBuf>,
        command: Command,
    },
    Version,
    Help,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut socket = None;
    let mut command = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let next = match arg.as_str() {
            "-V" | "--version" => return Ok(Args::Version),
            "-h" | "--help" => return Ok(Args::Help),
            "--socket" => {
                socket = Some(PathBuf::from(iter.next().ok_or("--socket needs a path")?));
                continue;
            }
            "hello" => Command::Hello,
            "rpc" => Command::Rpc,
            other => match other.strip_prefix("--socket=") {
                Some(path) => {
                    socket = Some(PathBuf::from(path));
                    continue;
                }
                None => return Err(format!("unexpected argument: {other}")),
            },
        };
        if command.replace(next).is_some() {
            return Err("give one command".into());
        }
    }
    let command = command.ok_or("missing a command")?;
    Ok(Args::Run { socket, command })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (socket, command) = match parse_args(&args) {
        Ok(Args::Run { socket, command }) => (socket, command),
        Ok(Args::Version) => {
            println!("vibe {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Ok(Args::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("vibe: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let socket = match paths::socket_path(socket) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("vibe: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = match command {
        Command::Hello => hello(&socket),
        Command::Rpc => rpc(&socket),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("vibe: {e}");
            ExitCode::FAILURE
        }
    }
}

fn hello(socket: &Path) -> Result<(), ClientError> {
    let params = HelloParams {
        client: "vibe-cli".into(),
        protocol: PROTOCOL_VERSION,
    };
    let r = client::call::<Hello>(socket, &params, Duration::from_secs(10))?;
    println!(
        "{} {} · protocol {} · {}/{} · pid {} · {}",
        r.daemon,
        r.version,
        r.protocol,
        r.os,
        r.arch,
        r.pid,
        socket.display()
    );
    Ok(())
}

/// Copies stdin to the socket and the socket to stdout, so `ssh host vibe
/// rpc` carries the same API as the local socket. Ends when vibed closes the
/// connection, which it does once stdin ends and every reply is out.
fn rpc(socket: &Path) -> Result<(), ClientError> {
    let stream = client::connect(socket)?;
    let mut to_daemon = stream.try_clone()?;
    std::thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut to_daemon);
        let _ = to_daemon.shutdown(Shutdown::Write);
    });
    let mut stdout = io::stdout().lock();
    io::copy(&mut &stream, &mut stdout)?;
    stdout.flush()?;
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
        assert_eq!(
            parse(&["hello"]),
            Ok(Args::Run {
                socket: None,
                command: Command::Hello
            })
        );
        assert_eq!(
            parse(&["--socket", "/s", "rpc"]),
            Ok(Args::Run {
                socket: Some("/s".into()),
                command: Command::Rpc
            })
        );
        assert_eq!(
            parse(&["rpc", "--socket=/s"]),
            Ok(Args::Run {
                socket: Some("/s".into()),
                command: Command::Rpc
            })
        );
        assert_eq!(parse(&["-V"]), Ok(Args::Version));
        assert_eq!(parse(&["hello", "--help"]), Ok(Args::Help));
        assert!(parse(&[]).is_err());
        assert!(parse(&["hello", "rpc"]).is_err());
        assert!(parse(&["attach"]).is_err());
        assert!(parse(&["hello", "--socket"]).is_err());
    }
}
