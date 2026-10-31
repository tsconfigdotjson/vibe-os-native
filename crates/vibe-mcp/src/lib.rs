//! The MCP server: the `vibe_*` tools over Streamable HTTP, built with
//! `rmcp` on the same core handlers as the socket API (PRD: MCP interface).
//! `vibe mcp` bridges stdio to it, for clients that can only start a
//! command.
//!
//! It keeps VibeOS's round, folded to one agent per worktree: start an
//! agent with a prompt, `vibe_wait`, read what it said, answer its
//! questions with `vibe_send`, delete it. Eight tools, across every host
//! the [`Daemon`] is connected to; every call after a start names its agent
//! by a ref that says which host it's on.
//!
//! Every call needs an API key, sent as `Authorization: Bearer`; there's no
//! unauthenticated mode, not even on loopback.
//!
//! This crate is the façade only. `vibed` implements [`Daemon`] on its
//! core, so the tools can't drift from the socket API's handlers.

use std::future::Future;

use vibe_core::model::{Activity, AgentState};
use vibe_core::rpc::{Agent, AgentDeleteParams, AgentStartParams, ErrorObject, Project};

pub mod http;
pub mod keys;
pub mod net;
mod tools;

pub use tools::{MAX_RESULT_CHARS, MAX_WAIT_SECS, Server, clip};

/// The path MCP is served at.
pub const PATH: &str = "/mcp";

/// An agent on a host: what every call after a start names.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Target {
    pub host: String,
    pub agent: String,
}

/// What `vibe_send` types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Text, pasted, then submitted with Enter unless `submit` is false.
    /// Refused while the agent is on a question, unless `force`.
    Text {
        text: String,
        submit: bool,
        force: bool,
    },
    /// Keys, each the bytes a terminal sends for it ([`keys::bytes`]),
    /// pressed one at a time.
    Keys(Vec<Vec<u8>>),
}

/// What a send did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    /// It was asleep, paused or exited, and was woken or resumed first.
    pub woke: bool,
    pub state: AgentState,
}

/// What an agent's terminal shows (PRD: MCP interface, `vibe_read_output`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Screen {
    /// The screen, one line per row.
    pub screen: String,
    /// Lines that scrolled off the screen since the cursor asked for.
    pub history: Vec<String>,
    /// Pass as `after` next time for only what's new.
    pub cursor: u64,
    /// Lines after the cursor asked for that are no longer kept.
    pub dropped: u64,
    /// More history is waiting from `cursor`.
    pub more: bool,
    /// Whether it's the live terminal, or the last one saved: its session
    /// has ended, or the agent is asleep.
    pub live: bool,
}

/// Where an agent is, for `vibe_wait`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub state: AgentState,
    pub activity: Activity,
    /// The latest thing it did, as a line.
    pub line: Option<String>,
    /// It has answered the last send to it (or its start's prompt): its
    /// transcript moved on since, and settled. Unless `background` was
    /// asked for, a turn that ended with commands left running in the
    /// background counts.
    pub answered: bool,
}

/// An agent as `vibe_list_agents` shows it. Its memory is the agent's
/// own: `memory` (the profile's limit) and `memory_used` (what it uses now,
/// while its compartment runs and its backend can say).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentView {
    pub agent: Agent,
    /// The line that attaches a terminal from another machine.
    pub attach_remote: String,
}

/// A message of an agent's conversation.
pub use vibe_status::{Message, Role};

/// An error a core handler gave: its JSON-RPC code says what kind.
pub type Error = ErrorObject;

/// What the MCP server needs from `vibed`: the core handlers, by host.
/// Hosts other than `local` come with Linux hosts (#16); until then, a
/// `Daemon` knows only its own.
pub trait Daemon: Send + Sync + 'static {
    /// Writes a line to the daemon's log.
    fn log(&self, line: &str) {
        eprintln!("{}", vibe_core::keys::redact(line));
    }

    /// Whether MCP is served on loopback now, after each check of the
    /// listeners, so the daemon can say so (`HostInfo.mcp`).
    fn serving(&self, _on: bool) {}

    /// The name of the API key `secret` is, if it's a key; checking it
    /// records that it was used.
    fn verify_key(&self, secret: &str) -> Option<String>;

    /// Every host this one can reach now, itself (`local`) first.
    fn hosts(&self) -> impl Future<Output = Result<Vec<String>, Error>> + Send;

    fn projects(&self, host: &str) -> impl Future<Output = Result<Vec<Project>, Error>> + Send;

    fn agents(&self, host: &str) -> impl Future<Output = Result<Vec<AgentView>, Error>> + Send;

    fn start_agent(&self, p: AgentStartParams)
    -> impl Future<Output = Result<Agent, Error>> + Send;

    fn delete_agent(
        &self,
        p: AgentDeleteParams,
    ) -> impl Future<Output = Result<Agent, Error>> + Send;

    /// Types `input` into the agent's terminal, waking it first if it's
    /// asleep, paused or exited. Whatever answers afterwards is what
    /// [`Daemon::check`] waits for.
    fn send(&self, t: &Target, input: Input) -> impl Future<Output = Result<Sent, Error>> + Send;

    /// Never wakes it.
    fn check(
        &self,
        t: &Target,
        background: bool,
    ) -> impl Future<Output = Result<Check, Error>> + Send;

    /// The screen, and what scrolled off it since `after`. Never wakes it.
    fn read_output(
        &self,
        t: &Target,
        after: Option<u64>,
    ) -> impl Future<Output = Result<Screen, Error>> + Send;

    /// The last `last` messages of its conversation, from its transcript.
    /// Never wakes it.
    fn read_messages(
        &self,
        t: &Target,
        last: usize,
    ) -> impl Future<Output = Result<Vec<Message>, Error>> + Send;
}

/// How a result names an agent: its bare ref on this host, `host:ref`
/// elsewhere (PRD: Core concepts).
pub fn display_ref(host: &str, agent: &str) -> String {
    if host == vibe_core::model::LOCAL_HOST {
        agent.to_owned()
    } else {
        format!("{host}:{agent}")
    }
}

/// A ref as a client writes it: `quiet-amber-otter`, or `vps:quiet-amber-otter`
/// for one on another host. Refs never hold a colon.
pub fn split_ref(name: &str) -> (Option<&str>, &str) {
    match name.trim().split_once(':') {
        Some((host, agent)) => (Some(host), agent),
        None => (None, name.trim()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_say_their_host() {
        assert_eq!(
            display_ref("local", "quiet-amber-otter"),
            "quiet-amber-otter"
        );
        assert_eq!(
            display_ref("vps", "quiet-amber-otter"),
            "vps:quiet-amber-otter"
        );
        assert_eq!(split_ref("quiet-amber-otter"), (None, "quiet-amber-otter"));
        assert_eq!(
            split_ref(" vps:quiet-amber-otter "),
            (Some("vps"), "quiet-amber-otter")
        );
        assert_eq!(split_ref("local:x"), (Some("local"), "x"));
    }
}
