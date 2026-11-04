//! `vibe service`: this host's shared MCP services, from its own shell. Their
//! logins are this host's own, so this needs no Mac, which is how a host mends
//! itself while the Mac is away. A personal API key is read here and goes to
//! this host's `vibed` alone: never on a command line, where `ps` would show it,
//! and never over a link.

use std::io::{self, BufRead};
use std::path::Path;
use std::time::Duration;

use vibe_core::rpc::{
    HostParams, ServiceInfo, ServiceKeyParams, ServiceParams, ServiceSetParams, ServicesAudit,
    ServicesConnectKey, ServicesDisconnect, ServicesList, ServicesSet,
};

use crate::attach::is_tty;
use crate::client;
use crate::setup_token::read_hidden;

/// How long a call may take: a key is checked with the service first.
const TIMEOUT: Duration = Duration::from_secs(90);

/// Which `vibe service`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    List,
    Audit,
    SetKey(String),
    Disconnect(String),
    Turn { service: String, on: bool },
}

impl Command {
    /// From the words after `service`.
    pub(crate) fn parse(words: &[String]) -> Result<Command, String> {
        match words {
            [] => Ok(Command::List),
            [w] if w == "list" || w == "ls" || w == "status" => Ok(Command::List),
            [w] if w == "audit" => Ok(Command::Audit),
            [w, name] if w == "set-key" => Ok(Command::SetKey(name.clone())),
            [w, name] if w == "disconnect" => Ok(Command::Disconnect(name.clone())),
            [w, name] if w == "on" || w == "off" => Ok(Command::Turn {
                service: name.clone(),
                on: w == "on",
            }),
            [w] if w == "set-key" || w == "disconnect" || w == "on" || w == "off" => {
                Err(format!("service {w} needs a service, such as linear"))
            }
            [w, _, ..] if w == "set-key" => Err(
                "service set-key takes only the service: give it the key on the terminal or \
                 stdin, so it stays off command lines"
                    .into(),
            ),
            _ => Err(
                "service takes list, audit, on SERVICE, off SERVICE, set-key SERVICE or \
                 disconnect SERVICE"
                    .into(),
            ),
        }
    }
}

/// One line for a service, for people.
pub(crate) fn describe(s: &ServiceInfo) -> String {
    let mut line = format!("{}  {}", s.name, s.state);
    if let Some(auth) = s.auth {
        line.push_str(&format!("  by {auth}"));
    }
    line.push_str(&format!(
        "  {} tool{}",
        s.tools,
        if s.tools == 1 { "" } else { "s" }
    ));
    if !s.enabled {
        line.push_str("  turned off");
    }
    if s.read_only {
        line.push_str("  read-only");
    }
    if let Some(detail) = &s.detail {
        line.push_str(&format!("\n  {detail}"));
        if let Some(at) = &s.detail_at {
            line.push_str(&format!(" ({at})"));
        }
    }
    line
}

/// One line from stdin, with no echo if that's a terminal.
fn read_key() -> Result<String, String> {
    let key = if is_tty(libc::STDIN_FILENO) {
        read_hidden("vibe: paste the personal API key, then press Return: ")
    } else {
        let mut line = String::new();
        io::stdin()
            .lock()
            .read_line(&mut line)
            .map(|_| line.trim().to_owned())
    }
    .map_err(|e| format!("can't read the key: {e}"))?;
    if key.is_empty() {
        return Err("no key was given".into());
    }
    Ok(key)
}

pub(crate) fn run(socket: &Path, command: Command) -> Result<(), String> {
    match command {
        Command::List => {
            let list = client::call::<ServicesList>(socket, &HostParams::default(), TIMEOUT)
                .map_err(|e| e.to_string())?;
            for s in &list.services {
                println!("{}", describe(s));
            }
        }
        Command::Audit => {
            let list = client::call::<ServicesAudit>(socket, &HostParams::default(), TIMEOUT)
                .map_err(|e| e.to_string())?;
            for e in &list.entries {
                println!(
                    "{}  {}  {} {}  {}  {}ms",
                    e.at, e.agent, e.service, e.tool, e.outcome, e.ms
                );
            }
        }
        Command::SetKey(service) => {
            let key = read_key()?;
            let params = ServiceKeyParams { service, key };
            let info = client::call::<ServicesConnectKey>(socket, &params, TIMEOUT)
                .map_err(|e| e.to_string())?;
            println!("{}", describe(&info));
        }
        Command::Disconnect(service) => {
            let params = ServiceParams {
                host: None,
                service,
            };
            let info = client::call::<ServicesDisconnect>(socket, &params, TIMEOUT)
                .map_err(|e| e.to_string())?;
            println!("{}", describe(&info));
        }
        Command::Turn { service, on } => {
            let params = ServiceSetParams {
                service,
                enabled: Some(on),
                ..Default::default()
            };
            let info =
                client::call::<ServicesSet>(socket, &params, TIMEOUT).map_err(|e| e.to_string())?;
            println!("{}", describe(&info));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vibe_core::model::{ServiceAuth, ServiceState};

    fn parse(words: &[&str]) -> Result<Command, String> {
        Command::parse(&words.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn words() {
        assert_eq!(parse(&[]), Ok(Command::List));
        assert_eq!(parse(&["status"]), Ok(Command::List));
        assert_eq!(parse(&["audit"]), Ok(Command::Audit));
        assert_eq!(
            parse(&["set-key", "linear"]),
            Ok(Command::SetKey("linear".into()))
        );
        assert_eq!(
            parse(&["disconnect", "linear"]),
            Ok(Command::Disconnect("linear".into()))
        );
        assert_eq!(
            parse(&["on", "linear"]),
            Ok(Command::Turn {
                service: "linear".into(),
                on: true
            })
        );
        assert_eq!(
            parse(&["off", "linear"]),
            Ok(Command::Turn {
                service: "linear".into(),
                on: false
            })
        );
        assert!(parse(&["on"]).is_err());
        assert!(parse(&["set-key"]).is_err());
        assert!(parse(&["nonsense"]).is_err());
    }

    #[test]
    fn a_key_is_never_taken_from_the_command_line() {
        let err = parse(&["set-key", "linear", "lin_api_secret"]).unwrap_err();
        assert!(err.contains("off command lines"), "{err}");
        assert!(!err.contains("lin_api_secret"), "{err}");
    }

    #[test]
    fn a_service_reads_as_it_stands() {
        let mut s = ServiceInfo {
            name: "linear".into(),
            title: "Linear".into(),
            state: ServiceState::NeedsReconnect,
            auth: Some(ServiceAuth::Oauth),
            enabled: true,
            read_only: true,
            denied_tools: Vec::new(),
            auto_allow: false,
            tools: 1,
            detail: Some("Linear refused the refresh (invalid_grant)".into()),
            detail_at: Some("2026-11-04T00:00:00Z".into()),
        };
        assert_eq!(
            describe(&s),
            "linear  needs_reconnect  by oauth  1 tool  read-only\n  Linear refused the refresh \
             (invalid_grant) (2026-11-04T00:00:00Z)"
        );
        s.detail = None;
        s.read_only = false;
        s.enabled = false;
        s.tools = 3;
        assert_eq!(
            describe(&s),
            "linear  needs_reconnect  by oauth  3 tools  turned off"
        );
    }
}
