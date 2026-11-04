//! `vibe service`: how this host stands with its shared MCP services, and its own
//! switch for each, from its own shell, with no Mac needed. Signing in happens in
//! the app on the Mac: a browser sign-in is the only way to connect a service.

use std::path::Path;
use std::time::Duration;

use vibe_core::model::ServiceRole;
use vibe_core::rpc::{
    HostParams, ServiceInfo, ServiceParams, ServiceSetParams, ServicesAudit, ServicesDisconnect,
    ServicesList, ServicesSet,
};

use crate::client;

/// How long a call may take.
const TIMEOUT: Duration = Duration::from_secs(90);

/// Which `vibe service`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    List,
    Audit,
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
            [w, name] if w == "disconnect" => Ok(Command::Disconnect(name.clone())),
            [w, name] if w == "on" || w == "off" => Ok(Command::Turn {
                service: name.clone(),
                on: w == "on",
            }),
            [w] if w == "disconnect" || w == "on" || w == "off" => {
                Err(format!("service {w} needs a service, such as linear"))
            }
            _ => Err(
                "service takes list, audit, on SERVICE, off SERVICE or disconnect SERVICE".into(),
            ),
        }
    }
}

/// One line for a service, for people.
pub(crate) fn describe(s: &ServiceInfo) -> String {
    let mut line = format!("{}  {}", s.name, s.state);
    match (&s.owner, s.role) {
        (Some(owner), _) => line.push_str(&format!("  login on {owner}")),
        (None, ServiceRole::Owner) => line.push_str("  login here"),
        (None, ServiceRole::Follower) => {}
    }
    if let Some(until) = &s.access_until {
        line.push_str(&format!("  access until {until}"));
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
    use vibe_core::model::ServiceState;

    fn parse(words: &[&str]) -> Result<Command, String> {
        Command::parse(&words.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn words() {
        assert_eq!(parse(&[]), Ok(Command::List));
        assert_eq!(parse(&["status"]), Ok(Command::List));
        assert_eq!(parse(&["audit"]), Ok(Command::Audit));
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
        assert!(parse(&["set-key", "linear"]).is_err(), "there are no keys");
        assert!(parse(&["nonsense"]).is_err());
    }

    #[test]
    fn a_service_reads_as_it_stands() {
        let mut s = ServiceInfo {
            name: "linear".into(),
            title: "Linear".into(),
            state: ServiceState::NeedsReconnect,
            role: ServiceRole::Owner,
            owner: None,
            access_until: Some("2026-11-05T00:00:00Z".into()),
            wants_token: false,
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
            "linear  needs_reconnect  login here  access until 2026-11-05T00:00:00Z  1 tool  \
             read-only\n  Linear refused the refresh (invalid_grant) (2026-11-04T00:00:00Z)"
        );
        s.detail = None;
        s.read_only = false;
        s.enabled = false;
        s.tools = 3;
        s.role = ServiceRole::Follower;
        s.owner = Some("vps-1".into());
        s.access_until = None;
        assert_eq!(
            describe(&s),
            "linear  needs_reconnect  login on vps-1  3 tools  turned off"
        );
    }
}
