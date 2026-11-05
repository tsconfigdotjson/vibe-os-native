//! `vibe`: the command-line client of `vibed`.

mod attach;
mod client;
mod doctor;
mod key;
mod mcp;
mod service;
mod setup;
mod setup_token;
mod toolbox;

use std::io::{self, Write};
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use vibe_core::claude_token::TokenStore;
use vibe_core::paths;
use vibe_core::rpc::{
    Hello, HelloParams, HostParams, PROTOCOL_VERSION, SessionInfo, SessionState, SessionsList,
};
use vibe_relay::{local, proxy};

use crate::client::ClientError;

const USAGE: &str = "\
usage: vibe [--socket PATH] <command>

commands:
  attach REF  join an agent's terminal; Ctrl-\\ detaches, and so does closing
              the window. From elsewhere: ssh -t <host> vibe attach REF
  ls          list the sessions on this host
  hello       check that vibed is up, and print what it reports
  doctor      check what agents here need: git, gh's login and git using it,
              claude and its login, system sleep off, on a Linux host that
              vibed stays up while you're logged out, Tailscale (optional), on
              a Mac the Linux images, and Pi; exits 1 if one needs you. vibed
              sets up all it can by itself, and asking has it try again.
              VibeOS.app checks its own Terminal permission
  rpc         the vibed socket API on stdin and stdout, one JSON-RPC message per line
  setup-token log linux agents in to Claude: runs `claude setup-token` and keeps
              its long-lived token in the Keychain, for vibed to hand each VM
  toolbox     open the Linux toolbox shell, where what you install and log in to
              every new linux agent gets; exiting or closing the window saves
              it, Ctrl-\\ leaves it open, and so does a dropped SSH connection
  toolbox status   the template and Linux home versions, and the last run
  toolbox update   run toolbox.toml's [update] commands on the template now
  toolbox rebuild  rebuild the template on the newest base image from toolbox.toml
  toolbox logins   what every new linux agent is logged in to: the Linux
                   home's gh, gcloud, git and SSH logins, and Claude's
  toolbox close    save and close the open toolbox shell, as exiting it does
  mcp         MCP over stdin and stdout, for clients that can only start a
              command: a bridge to vibed's http://127.0.0.1:7878/mcp, with the
              API key in VIBE_API_KEY (--key KEY works too, but shows it to
              anyone who can run ps); --url URL for another endpoint
  mcp-proxy SERVICE
              a shared service over stdin and stdout, as agents' MCP clients
              start it (vibe-linear, say). It goes through this host's vibed,
              which uses the one login, or a token from the host that holds it
  service     how this host's shared services stand. Connect them in VibeOS on
              the Mac, by signing in with a browser there
  service on SERVICE, service off SERVICE
              turn a shared service on or off on this host, with no Mac needed
  service disconnect SERVICE
              forget a service's login, if it's on this host
  service audit
              the last calls this host's agents made to its services
  key         list this host's API keys for MCP
  key create NAME  make an API key, and print it once
  key revoke NAME  revoke an API key
  project     list this host's projects and their profiles
  project add OWNER/NAME|PATH [--name NAME]
              clone a GitHub repo with this host's gh login, or register a
              local checkout; it starts with this host's starter profiles
  project remove NAME [--delete]
              forget a project. On the Mac its files stay on disk. On a
              Linux host, --delete also deletes the clone VibeOS made in
              ~/VibeOS/projects (with any branches never pushed); without
              it, a removal that would delete one is refused
  profile list PROJECT  a project's profiles
  profile create PROJECT NAME [--environment linux|macos] [--model opus|sonnet]
              [--reasoning low|medium|high|xhigh|max] [--mode auto|bypass]
              [--memory 4G] [--shared-services on|off] [--agent claude|pi]
              [--pi-model MODEL]
  profile set PROJECT PROFILE [--name NEW] [the same settings]
  profile delete PROJECT PROFILE
              project and profile take --host NAME for a Linux host this one
              is linked to
  pi          Pi on this host: what's installed and which providers it's
              signed in to
  pi forget PROVIDER...
              on a Mac, take these providers' logins from what linux agents
              share, such as one an agent offered; stop that agent as well
  pi install [--vm]
              download Pi, checksummed, and link it at ~/.local/bin/pi; on a
              Mac, --vm for the layer linux agents boot instead. Sign in from
              the app, in Settings → Agents. At a terminal, it's Pi's own: run
              `pi`, then /login (for linux agents on a Mac, in `vibe toolbox`,
              with a headless option: `vibe pi` says which)
              pi takes --host NAME for a Linux host this one is linked to
  host        the hosts this one knows, and their links
  host add NAME SSH-DESTINATION
              install vibed on a Linux host over SSH (as Add Linux Host…
              does) and link it, as NAME
  host upgrade NAME    install this build's vibed there now, unless it runs a newer
                       one; it happens by itself, so this is to try again sooner
  host remove NAME     forget a Linux host; it runs on there, on its own
  host external NAME DOMAIN
              make a Linux host the external address: it serves
              https://DOMAIN/mcp to outside MCP callers, and this Mac keeps
              a tunnel to it (on a Linux host on its own, NAME is local)
  host external NAME off
              stop it being the external address

options:
  --socket PATH   vibed's socket (default: $VIBE_SOCKET, or the platform default)
  -V, --version   print the version
  -h, --help      print this help

environment:
  VIBE_CLAUDE     the claude that setup-token runs (default: claude on PATH)
  VIBE_API_KEY    the API key vibe mcp sends
  VIBE_LOGIN_KEYCHAIN
                  allow lets a vibe outside an app bundle reach the login
                  Keychain for setup-token; never keeps any vibe out";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Attach(String),
    Ls,
    Hello,
    Doctor,
    Rpc,
    SetupToken,
    Toolbox(toolbox::Command),
    Mcp {
        url: Option<String>,
        key: Option<String>,
    },
    McpProxy {
        service: String,
    },
    Service(service::Command),
    Key(key::Command),
    Setup(setup::Command),
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
            "doctor" => {
                // Asking fixes what can be fixed, so this means nothing now. An
                // old habit or script still works.
                if iter.as_slice().first().is_some_and(|w| w == "--fix") {
                    iter.next();
                }
                Command::Doctor
            }
            "rpc" => Command::Rpc,
            "mcp-proxy" => Command::McpProxy {
                service: iter
                    .next()
                    .ok_or("mcp-proxy needs a service, such as linear")?
                    .clone(),
            },
            "service" => {
                let mut words = Vec::new();
                while let Some(w) = iter.next() {
                    if w == "--socket" {
                        socket = Some(PathBuf::from(iter.next().ok_or("--socket needs a path")?));
                    } else if let Some(path) = w.strip_prefix("--socket=") {
                        socket = Some(PathBuf::from(path));
                    } else if w == "-h" || w == "--help" {
                        return Ok(Args::Help);
                    } else {
                        words.push(w.clone());
                    }
                }
                Command::Service(service::Command::parse(&words)?)
            }
            "setup-token" => Command::SetupToken,
            "toolbox" => {
                let sub = iter
                    .as_slice()
                    .first()
                    .and_then(|w| toolbox::Command::parse(w));
                if sub.is_some() {
                    iter.next();
                }
                Command::Toolbox(sub.unwrap_or(toolbox::Command::Open))
            }
            "mcp" => {
                let (mut url, mut key) = (None, None);
                while let Some(flag) = iter.as_slice().first() {
                    let slot = match flag.as_str() {
                        "--url" => &mut url,
                        "--key" => &mut key,
                        _ => break,
                    };
                    iter.next();
                    let value = iter.next().ok_or(format!("{flag} needs a value"))?;
                    *slot = Some(value.clone());
                }
                Command::Mcp { url, key }
            }
            word @ ("project" | "profile" | "host" | "pi") => {
                // Everything after it is its, but --socket.
                let mut words = Vec::new();
                while let Some(w) = iter.next() {
                    if w == "--socket" {
                        socket = Some(PathBuf::from(iter.next().ok_or("--socket needs a path")?));
                    } else if let Some(path) = w.strip_prefix("--socket=") {
                        socket = Some(PathBuf::from(path));
                    } else if w == "-h" || w == "--help" {
                        return Ok(Args::Help);
                    } else {
                        words.push(w.clone());
                    }
                }
                Command::Setup(match word {
                    "project" => setup::Command::parse_project(&words)?,
                    "profile" => setup::Command::parse_profile(&words)?,
                    "pi" => setup::Command::parse_pi(&words)?,
                    _ => setup::Command::parse_host(&words)?,
                })
            }
            "key" => {
                let words: Vec<String> = iter
                    .as_slice()
                    .iter()
                    .take_while(|w| !w.starts_with('-'))
                    .cloned()
                    .collect();
                for _ in 0..words.len() {
                    iter.next();
                }
                Command::Key(key::Command::parse(&words)?)
            }
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
    if command == Command::SetupToken {
        let claude = std::env::var_os("VIBE_CLAUDE")
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "claude".into());
        return match setup_token::setup_token(&claude, &TokenStore::current()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("vibe: {e}");
                ExitCode::FAILURE
            }
        };
    }
    if let Command::Mcp { url, key } = &command {
        let key = key
            .clone()
            .or_else(|| std::env::var("VIBE_API_KEY").ok().filter(|k| !k.is_empty()));
        let Some(key) = key else {
            eprintln!(
                "vibe: vibe mcp needs an API key: set VIBE_API_KEY, or pass --key. Make one in \
                 Settings → API keys, or with vibe key create NAME"
            );
            return ExitCode::from(2);
        };
        let url = url.clone().unwrap_or_else(default_mcp_url);
        let run = mcp::Url::parse(&url).and_then(|u| mcp::run(u, key));
        return match run {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("vibe: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let socket = match paths::socket_path(socket) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("vibe: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Command::McpProxy { service } = &command {
        let config = proxy::Config {
            service: service.clone(),
            socket: local::socket_beside(socket),
            agent: local::agent_from_env(),
            cache: local::cache_dir(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            timing: proxy::Timing::default(),
        };
        let ended = proxy::run(config, io::stdin().lock(), io::stdout());
        // Its threads go with the process, whatever they're waiting on.
        std::process::exit(i32::from(ended.is_err()));
    }
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
        Command::Doctor => {
            return match doctor::run(&socket) {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => ExitCode::FAILURE,
                Err(e) => {
                    eprintln!("vibe: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Command::Rpc => rpc(&socket),
        Command::SetupToken | Command::Mcp { .. } | Command::McpProxy { .. } => {
            unreachable!("handled above")
        }
        Command::Service(command) => {
            return match service::run(&socket, command) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("vibe: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Command::Key(command) => {
            return match key::run(&socket, command, &default_mcp_url()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("vibe: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Command::Setup(command) => {
            return match setup::run(&socket, command) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("vibe: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Command::Toolbox(command) => {
            return match toolbox::run(&socket, command) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("vibe: {e}");
                    ExitCode::FAILURE
                }
            };
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("vibe: {e}");
            ExitCode::FAILURE
        }
    }
}

/// This host's MCP endpoint on loopback, at the port in `host.toml`.
fn default_mcp_url() -> String {
    let port = paths::Layout::current()
        .ok()
        .and_then(|l| vibe_core::limits::load(&l.host_config()).ok())
        .map_or(vibe_core::limits::MCP_PORT, |l| l.mcp_port);
    format!("http://127.0.0.1:{port}/mcp")
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
        let _ = pump(&mut io::stdin().lock(), &mut to_daemon);
        let _ = to_daemon.shutdown(Shutdown::Write);
    });
    pump(&mut &stream, &mut io::stdout().lock())?;
    Ok(())
}

/// Copies `from` to `to` as it comes, each read written and flushed at
/// once. Not `io::copy`: on Linux, between a socket and a pipe it splices,
/// and a splice from a Unix socket into a pipe can hold what it read until
/// the socket ends, which would hold every reply until stdin closes.
fn pump(from: &mut impl io::Read, to: &mut impl Write) -> io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        to.write_all(&buf[..n])?;
        to.flush()?;
    }
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
        assert_eq!(
            parse(&["doctor"]),
            Ok(Args::Run {
                socket: None,
                command: Command::Doctor
            })
        );
        assert_eq!(
            parse(&["doctor", "--fix", "--socket", "/s"]),
            Ok(Args::Run {
                socket: Some("/s".into()),
                command: Command::Doctor
            })
        );
        assert!(parse(&["doctor", "--fxi"]).is_err());
        assert!(parse(&["attach"]).is_err());
        assert_eq!(
            parse(&["setup-token"]),
            Ok(Args::Run {
                socket: None,
                command: Command::SetupToken
            })
        );
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
        let toolbox = |command| {
            Ok(Args::Run {
                socket: None,
                command: Command::Toolbox(command),
            })
        };
        assert_eq!(parse(&["toolbox"]), toolbox(toolbox::Command::Open));
        assert_eq!(
            parse(&["toolbox", "status"]),
            toolbox(toolbox::Command::Status)
        );
        assert_eq!(
            parse(&["toolbox", "update"]),
            toolbox(toolbox::Command::Update)
        );
        assert_eq!(
            parse(&["toolbox", "rebuild"]),
            toolbox(toolbox::Command::Rebuild)
        );
        assert_eq!(
            parse(&["toolbox", "logins"]),
            toolbox(toolbox::Command::Logins)
        );
        assert_eq!(
            parse(&["toolbox", "close"]),
            toolbox(toolbox::Command::Close)
        );
        assert_eq!(
            parse(&["toolbox", "--socket", "/s"]),
            Ok(Args::Run {
                socket: Some("/s".into()),
                command: Command::Toolbox(toolbox::Command::Open),
            })
        );
        assert_eq!(
            parse(&["mcp"]),
            Ok(Args::Run {
                socket: None,
                command: Command::Mcp {
                    url: None,
                    key: None
                },
            })
        );
        assert_eq!(
            parse(&["mcp", "--url", "http://h:1/mcp", "--key", "vk_k"]),
            Ok(Args::Run {
                socket: None,
                command: Command::Mcp {
                    url: Some("http://h:1/mcp".into()),
                    key: Some("vk_k".into())
                },
            })
        );
        assert!(parse(&["mcp", "--url"]).is_err());
        assert_eq!(
            parse(&["key", "create", "laptop", "--socket", "/s"]),
            Ok(Args::Run {
                socket: Some("/s".into()),
                command: Command::Key(key::Command::Create("laptop".into())),
            })
        );
        assert_eq!(
            parse(&["key"]),
            Ok(Args::Run {
                socket: None,
                command: Command::Key(key::Command::List),
            })
        );
        assert!(parse(&["key", "create"]).is_err());
        assert_eq!(
            parse(&[
                "profile", "create", "web", "x", "--socket", "/s", "--model", "sonnet"
            ]),
            Ok(Args::Run {
                socket: Some("/s".into()),
                command: Command::Setup(setup::Command::CreateProfile {
                    host: None,
                    project: "web".into(),
                    name: "x".into(),
                    settings: setup::Settings {
                        model: Some(vibe_core::model::Model::Sonnet),
                        ..Default::default()
                    },
                }),
            })
        );
        assert_eq!(
            parse(&["--socket=/s", "host", "add", "vps", "me@vps"]),
            Ok(Args::Run {
                socket: Some("/s".into()),
                command: Command::Setup(setup::Command::AddHost {
                    name: "vps".into(),
                    address: "me@vps".into(),
                }),
            })
        );
        assert_eq!(
            parse(&["project", "--socket=/s"]),
            Ok(Args::Run {
                socket: Some("/s".into()),
                command: Command::Setup(setup::Command::Projects { host: None }),
            })
        );
        assert!(parse(&["project", "add"]).is_err());
        assert!(parse(&["toolbox", "rebiuld"]).is_err());
        assert!(parse(&["toolbox", "status", "update"]).is_err());
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
