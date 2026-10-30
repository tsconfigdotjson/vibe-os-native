//! `vibe`: the command-line client of `vibed`.

mod attach;
mod client;

use std::io::{self, Write};
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use vibe_core::paths;
use vibe_core::rpc::{
    Hello, HelloParams, HostParams, PROTOCOL_VERSION, SessionInfo, SessionState, SessionsList,
};

use crate::client::ClientError;

const USAGE: &str = "\
usage: vibe [--socket PATH] <command>

commands:
  attach REF  join an agent's terminal; Ctrl-\\ detaches, and so does closing
              the window. From elsewhere: ssh -t <host> vibe attach REF
  ls          list the sessions on this host
  hello       check that vibed is up, and print what it reports
  rpc         the vibed socket API on stdin and stdout, one JSON-RPC message per line

options:
  --socket PATH   vibed's socket (default: $VIBE_SOCKET, or the platform default)
  -V, --version   print the version
  -h, --help      print this help";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Attach(String),
    Ls,
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
            "attach" => Command::Attach(iter.next().ok_or("attach needs a ref")?.clone()),
            "ls" => Command::Ls,
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
        Command::Attach(agent) => {
            return match attach::attach(&socket, &agent) {
                Ok(attach::End::Lost) => ExitCode::FAILURE,
                Ok(_) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("vibe: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Command::Ls => ls(&socket),
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

fn ls(socket: &Path) -> Result<(), ClientError> {
    let list =
        client::call::<SessionsList>(socket, &HostParams::default(), Duration::from_secs(30))?;
    print!("{}", ls_table(&list.sessions));
    Ok(())
}

/// One line per session: its ref, its state, and who's attached or how it
/// ended.
fn ls_table(sessions: &[SessionInfo]) -> String {
    if sessions.is_empty() {
        return "no sessions\n".into();
    }
    let width = sessions.iter().map(|s| s.agent.len()).max().unwrap_or(0);
    let mut out = String::new();
    for s in sessions {
        let detail = match (s.state, &s.exit) {
            (SessionState::Exited, Some(exit)) => {
                let how = match (&exit.code, &exit.signal) {
                    (Some(code), _) => format!("code {code}"),
                    (None, Some(signal)) => signal.clone(),
                    (None, None) => "ended".into(),
                };
                format!("{how} at {}", exit.at)
            }
            _ if s.clients == 0 => "not attached".into(),
            _ if s.ttys.is_empty() => format!("{} attached", s.clients),
            _ => format!("{} attached ({})", s.clients, s.ttys.join(", ")),
        };
        let title = if s.title == s.agent {
            String::new()
        } else {
            format!("  {}", s.title)
        };
        out.push_str(&format!(
            "{:width$}  {:7}  {detail}{title}\n",
            s.agent,
            s.state.as_str()
        ));
    }
    out
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
        assert_eq!(
            parse(&["attach", "quiet-amber-otter"]),
            Ok(Args::Run {
                socket: None,
                command: Command::Attach("quiet-amber-otter".into())
            })
        );
        assert_eq!(
            parse(&["ls", "--socket=/s"]),
            Ok(Args::Run {
                socket: Some("/s".into()),
                command: Command::Ls
            })
        );
        assert!(parse(&["ls", "attach", "x"]).is_err());
    }

    #[test]
    fn ls_lines() {
        use vibe_core::rpc::SessionExit;
        let session = |r: &str, state, clients, ttys: &[&str], exit| SessionInfo {
            host: "local".into(),
            agent: r.into(),
            state,
            title: r.into(),
            clients,
            ttys: ttys.iter().map(|t| t.to_string()).collect(),
            exit,
        };
        assert_eq!(ls_table(&[]), "no sessions\n");
        let mut titled = session(
            "quiet-amber-otter",
            SessionState::Running,
            2,
            &["/dev/ttys003"],
            None,
        );
        titled.title = "quiet-amber-otter · reviewer".into();
        let at = "2026-10-30T11:00:00Z".to_owned();
        let table = ls_table(&[
            titled,
            session("calm", SessionState::Running, 0, &[], None),
            session("w", SessionState::Running, 1, &[], None),
            session(
                "brave",
                SessionState::Exited,
                0,
                &[],
                Some(SessionExit {
                    code: Some(0),
                    signal: None,
                    at: at.clone(),
                }),
            ),
            session(
                "dead",
                SessionState::Exited,
                0,
                &[],
                Some(SessionExit {
                    code: None,
                    signal: Some("SIGKILL".into()),
                    at,
                }),
            ),
        ]);
        assert_eq!(
            table,
            "\
quiet-amber-otter  running  2 attached (/dev/ttys003)  quiet-amber-otter · reviewer
calm               running  not attached
w                  running  1 attached
brave              exited   code 0 at 2026-10-30T11:00:00Z
dead               exited   SIGKILL at 2026-10-30T11:00:00Z
"
        );
    }
}
