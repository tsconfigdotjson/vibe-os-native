//! What's said on the local socket a machine's shims reach: `vibe-guest`'s in
//! a VM, `run/.services.sock` under `vibed` on a host. It's one JSON line
//! each way, then either JSON-RPC, one message per line, or relay frames.

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;

use serde_json::{Value, json};

/// Where a shim in a VM reaches `vibe-guest`.
pub const GUEST_SOCKET: &str = "/run/vibe/services.sock";

/// Where the static `vibe` a VM's agents run the shim with is kept: the
/// guest's own binary, which answers to that name.
pub const GUEST_PROGRAM: &str = "/run/vibe/bin/vibe";

/// The socket's name in `vibed`'s run directory. No ref can start with a dot,
/// so it can't be taken for a session's socket, nor a session's for it.
pub const HOST_SOCKET_NAME: &str = ".services.sock";

/// Overrides the socket, for the shim and `vibe services`.
pub const SOCKET_ENV: &str = "VIBE_SERVICES_SOCKET";

/// Set in every agent: its ref, which the shim passes along for the audit log
/// and the per-profile switch.
pub const AGENT_ENV: &str = "VIBE_AGENT";

/// Overrides where the shim keeps its catalog cache, a directory.
pub const CACHE_ENV: &str = "VIBE_SERVICES_CACHE";

/// What a client says first on the local socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hello {
    /// A shim wants a service.
    Open {
        service: String,
        agent: Option<String>,
    },
    /// `vibe services`, the Mac's end of a Linux host's relay: from here on
    /// the connection carries frames.
    Relay,
}

impl Hello {
    pub fn line(&self) -> String {
        let v = match self {
            Hello::Open { service, agent } => {
                let mut v = json!({"open": service});
                if let Some(agent) = agent {
                    v["agent"] = json!(agent);
                }
                v
            }
            Hello::Relay => json!({"relay": crate::frame::PROTOCOL}),
        };
        format!("{v}\n")
    }

    pub fn parse(line: &[u8]) -> Result<Hello, String> {
        let v: Value = serde_json::from_slice(line).map_err(|_| "that's not a hello")?;
        if v.get("relay").is_some() {
            return Ok(Hello::Relay);
        }
        match v.get("open").and_then(Value::as_str) {
            Some(service) => Ok(Hello::Open {
                service: service.to_owned(),
                agent: v.get("agent").and_then(Value::as_str).map(str::to_owned),
            }),
            None => Err("that's not a hello".into()),
        }
    }
}

/// The answer to a hello.
pub fn answer(outcome: &Result<(), String>) -> String {
    let v = match outcome {
        Ok(()) => json!({"ok": true}),
        Err(why) => json!({"ok": false, "error": why}),
    };
    format!("{v}\n")
}

/// What an answer says.
pub fn parse_answer(line: &[u8]) -> Result<(), String> {
    let v: Value =
        serde_json::from_slice(line).map_err(|_| "an answer that makes no sense".to_owned())?;
    if v.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    Err(v
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("refused without a reason")
        .to_owned())
}

/// What the local socket says when this machine has no way to the Mac.
pub const NO_RELAY: &str = "Mac unreachable: this machine has no connection to it now";

fn set(v: Option<OsString>) -> Option<OsString> {
    v.filter(|v| !v.is_empty())
}

/// The socket for a shim on a host: [`SOCKET_ENV`], or `run/.services.sock`
/// beside `vibed`'s socket. `vibed_socket` is that socket, however this
/// program found it.
pub fn socket_beside(vibed_socket: PathBuf) -> PathBuf {
    if let Some(path) = set(env::var_os(SOCKET_ENV)) {
        return PathBuf::from(path);
    }
    vibed_socket.with_file_name(HOST_SOCKET_NAME)
}

/// The directory the shim keeps its catalogs in: [`CACHE_ENV`],
/// `$XDG_CACHE_HOME/vibeos/services` or `~/.cache/vibeos/services`, if any is
/// set. Only tool descriptions go there, never anything secret.
pub fn cache_dir() -> Option<PathBuf> {
    if let Some(dir) = set(env::var_os(CACHE_ENV)) {
        return Some(PathBuf::from(dir));
    }
    let base = set(env::var_os("XDG_CACHE_HOME"))
        .map(PathBuf::from)
        .or_else(|| set(env::var_os("HOME")).map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("vibeos/services"))
}

/// The agent this process belongs to, if it's in one.
pub fn agent_from_env() -> Option<String> {
    set(env::var_os(AGENT_ENV)).and_then(|v| v.into_string().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hellos_read_back() {
        for hello in [
            Hello::Open {
                service: "linear".into(),
                agent: Some("quiet-amber-otter".into()),
            },
            Hello::Open {
                service: "linear".into(),
                agent: None,
            },
            Hello::Relay,
        ] {
            let line = hello.line();
            assert!(line.ends_with('\n'));
            assert_eq!(Hello::parse(line.trim_end().as_bytes()), Ok(hello));
        }
        assert!(Hello::parse(b"{}").is_err());
        assert!(Hello::parse(b"nonsense").is_err());
        assert!(Hello::parse(br#"{"open":1}"#).is_err());
    }

    #[test]
    fn answers_read_back() {
        assert_eq!(parse_answer(answer(&Ok(())).trim_end().as_bytes()), Ok(()));
        assert_eq!(
            parse_answer(answer(&Err("no".into())).trim_end().as_bytes()),
            Err("no".to_owned())
        );
        assert!(parse_answer(b"{}").is_err());
        assert!(parse_answer(b"junk").is_err());
    }
}
