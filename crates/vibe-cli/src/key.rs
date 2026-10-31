//! `vibe key`: API keys for MCP, on a host without the menu bar, such as a
//! standalone Linux host (PRD: Architecture, `vibe` CLI). The same keys as
//! Settings → API keys: `vk_…`, 256 bits, stored hashed, named, revocable,
//! with when each was last used. A key is equivalent to a shell on this
//! host.

use std::path::Path;
use std::time::Duration;

use vibe_core::rpc::{
    ApiKey, KeyCreateParams, KeyParams, KeysCreate, KeysList, KeysRevoke, NoParams,
};

use crate::client;

const TIMEOUT: Duration = Duration::from_secs(30);

/// Which `vibe key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    List,
    Create(String),
    Revoke(String),
}

impl Command {
    /// From the words after `key`.
    pub(crate) fn parse(words: &[String]) -> Result<Command, String> {
        match words {
            [] => Ok(Command::List),
            [w] if w == "list" || w == "ls" => Ok(Command::List),
            [w, name] if w == "create" => Ok(Command::Create(name.clone())),
            [w, name] if w == "revoke" => Ok(Command::Revoke(name.clone())),
            [w] if w == "create" || w == "revoke" => Err(format!("key {w} needs a key's name")),
            _ => Err("key takes list, create NAME or revoke NAME".into()),
        }
    }
}

/// The line that adds this host's MCP to Claude Code with `secret`.
pub(crate) fn setup_line(url: &str, secret: &str) -> String {
    format!(
        "claude mcp add --transport http vibeos {url} --header \"Authorization: Bearer {secret}\""
    )
}

fn describe(k: &ApiKey) -> String {
    format!(
        "{}  {}…  made {}  {}",
        k.name,
        k.prefix,
        k.created_at,
        match &k.last_used_at {
            Some(at) => format!("last used {at}"),
            None => "never used".into(),
        }
    )
}

pub(crate) fn run(socket: &Path, command: Command, url: &str) -> Result<(), String> {
    match command {
        Command::List => {
            let list = client::call::<KeysList>(socket, &NoParams {}, TIMEOUT)
                .map_err(|e| e.to_string())?;
            if list.keys.is_empty() {
                println!("no keys; make one with vibe key create NAME");
            }
            for k in &list.keys {
                println!("{}", describe(k));
            }
        }
        Command::Create(name) => {
            let made = client::call::<KeysCreate>(socket, &KeyCreateParams { name }, TIMEOUT)
                .map_err(|e| e.to_string())?;
            println!("{}", made.secret);
            eprintln!(
                "\nThat's API key {:?}. It's shown only now, and it's equivalent to a shell on this host: keep it like a password. To use it from Claude Code:\n\n  {}\n\nor over stdio: VIBE_API_KEY=… vibe mcp",
                made.key.name,
                setup_line(url, &made.secret)
            );
        }
        Command::Revoke(name) => {
            let gone = client::call::<KeysRevoke>(socket, &KeyParams { name }, TIMEOUT)
                .map_err(|e| e.to_string())?;
            println!("revoked {}", gone.name);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(w: &[&str]) -> Vec<String> {
        w.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn commands() {
        assert_eq!(Command::parse(&words(&[])), Ok(Command::List));
        assert_eq!(Command::parse(&words(&["list"])), Ok(Command::List));
        assert_eq!(
            Command::parse(&words(&["create", "laptop"])),
            Ok(Command::Create("laptop".into()))
        );
        assert_eq!(
            Command::parse(&words(&["revoke", "laptop"])),
            Ok(Command::Revoke("laptop".into()))
        );
        assert!(Command::parse(&words(&["create"])).is_err());
        assert!(Command::parse(&words(&["delete", "x"])).is_err());
        assert!(Command::parse(&words(&["create", "a", "b"])).is_err());
    }

    #[test]
    fn the_setup_line_adds_the_server() {
        assert_eq!(
            setup_line("http://127.0.0.1:7878/mcp", "vk_x"),
            "claude mcp add --transport http vibeos http://127.0.0.1:7878/mcp --header \"Authorization: Bearer vk_x\""
        );
    }
}
