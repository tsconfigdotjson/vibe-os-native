//! The JSON-RPC 2.0 contract for the `vibed` socket.
//!
//! The CLI, the menu bar app and MCP all speak this one protocol to the same
//! core handlers. `protocol/vibed-rpc.json` and `protocol/examples/` are the
//! shared source of truth: contract tests on the Rust side (here and in
//! `vibed`) and on the Swift side (`swift/VibeOS`) check against them, so the
//! two can't drift.
//!
//! Framing is one JSON value per line (UTF-8, `\n`-terminated), in both
//! directions, on the Unix socket and through `vibe rpc` over SSH alike.

use std::fmt;

use serde::de::{DeserializeOwned, Error as _};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

pub use crate::limits::HostLimits;
pub use crate::model::{
    Activity, AdminNeed, AgentKind, AgentState, Certainty, CheckId, CheckState, Environment,
    EventKind, GhState, HostLink, LoginTool, MessageRole, Mode, Model, PiTarget, Profile,
    ProjectSource, Reasoning, ServiceRole, ServiceState, SessionState, StopReason, ToolboxAction,
    ToolboxState, TunnelState, WakeupKind,
};

/// Bumped on any incompatible change to the contract. Clients send theirs in
/// `daemon.hello`, and the daemon refuses a mismatch.
pub const PROTOCOL_VERSION: u32 = 2;

/// The longest message, newline excluded, either side accepts.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// The `"jsonrpc": "2.0"` member, which must be exactly that string.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct V2;

impl Serialize for V2 {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for V2 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s == "2.0" {
            Ok(V2)
        } else {
            Err(D::Error::invalid_value(
                serde::de::Unexpected::Str(&s),
                &"\"2.0\"",
            ))
        }
    }
}

/// A request id. `Null` is only sent by the daemon, when it can't read the id
/// of a request it's rejecting.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Number(i64),
    String(String),
    Null,
}

/// Deserializes a member that is present, so `null` becomes `Some(null)`
/// rather than `None`, which stays reserved for an absent member.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

/// A request, or a notification when `id` is absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub jsonrpc: V2,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub id: Option<Id>,
    pub method: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub params: Option<Value>,
}

impl Request {
    /// A notification `N` from the daemon, such as an event.
    pub fn notification<N: Notification>(params: &N::Params) -> Self {
        Request {
            jsonrpc: V2,
            id: None,
            method: N::NAME.to_owned(),
            params: Some(serde_json::to_value(params).expect("params serialize to JSON")),
        }
    }

    /// A call to `M` that expects a response.
    pub fn call<M: Method>(id: Id, params: &M::Params) -> Self {
        Request {
            jsonrpc: V2,
            id: Some(id),
            method: M::NAME.to_owned(),
            params: Some(serde_json::to_value(params).expect("params serialize to JSON")),
        }
    }
}

/// A response: exactly one of `result` or `error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawResponse", into = "RawResponse")]
pub struct Response {
    pub jsonrpc: V2,
    pub id: Id,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Result(Value),
    Error(ErrorObject),
}

#[derive(Clone, Serialize, Deserialize)]
struct RawResponse {
    jsonrpc: V2,
    id: Id,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<ErrorObject>,
}

impl TryFrom<RawResponse> for Response {
    type Error = &'static str;

    fn try_from(raw: RawResponse) -> Result<Self, Self::Error> {
        let outcome = match (raw.result, raw.error) {
            (Some(result), None) => Outcome::Result(result),
            (None, Some(error)) => Outcome::Error(error),
            _ => return Err("a response has exactly one of `result` and `error`"),
        };
        Ok(Response {
            jsonrpc: raw.jsonrpc,
            id: raw.id,
            outcome,
        })
    }
}

impl From<Response> for RawResponse {
    fn from(r: Response) -> Self {
        let (result, error) = match r.outcome {
            Outcome::Result(v) => (Some(v), None),
            Outcome::Error(e) => (None, Some(e)),
        };
        RawResponse {
            jsonrpc: r.jsonrpc,
            id: r.id,
            result,
            error,
        }
    }
}

impl Response {
    pub fn result(id: Id, result: Value) -> Self {
        Response {
            jsonrpc: V2,
            id,
            outcome: Outcome::Result(result),
        }
    }

    pub fn error(id: Id, error: ErrorObject) -> Self {
        Response {
            jsonrpc: V2,
            id,
            outcome: Outcome::Error(error),
        }
    }
}

/// The error codes in the contract: the JSON-RPC 2.0 standard ones, then
/// ours, which count down from -32000.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    ParseError,
    InvalidRequest,
    MethodNotFound,
    InvalidParams,
    InternalError,
    /// The client's `protocol` in `daemon.hello` isn't the daemon's. The
    /// error's data is a [`ProtocolMismatchData`].
    ProtocolMismatch,
    /// No host, project, profile or key by that name.
    NotFound,
    /// The name is taken, or the thing is in use.
    Conflict,
    /// `host.toml` can't be read; the message says where and why.
    HostConfigInvalid,
    /// A command `vibed` ran, such as `gh repo clone`, failed; the message
    /// has its error.
    CommandFailed,
    /// The call is for a Linux host whose link is down: its agents are
    /// unavailable until the link is back.
    HostUnavailable,
    /// A call that came through the external address's tunnel carried no
    /// API key this Mac knows: unknown, revoked, or a host's own. Only the
    /// tunnel's socket answers with it.
    Unauthorized,
}

impl ErrorCode {
    pub const ALL: [ErrorCode; 12] = [
        ErrorCode::ParseError,
        ErrorCode::InvalidRequest,
        ErrorCode::MethodNotFound,
        ErrorCode::InvalidParams,
        ErrorCode::InternalError,
        ErrorCode::ProtocolMismatch,
        ErrorCode::NotFound,
        ErrorCode::Conflict,
        ErrorCode::HostConfigInvalid,
        ErrorCode::CommandFailed,
        ErrorCode::HostUnavailable,
        ErrorCode::Unauthorized,
    ];

    pub fn code(self) -> i64 {
        match self {
            ErrorCode::ParseError => -32700,
            ErrorCode::InvalidRequest => -32600,
            ErrorCode::MethodNotFound => -32601,
            ErrorCode::InvalidParams => -32602,
            ErrorCode::InternalError => -32603,
            ErrorCode::ProtocolMismatch => -32000,
            ErrorCode::NotFound => -32001,
            ErrorCode::Conflict => -32002,
            ErrorCode::HostConfigInvalid => -32003,
            ErrorCode::CommandFailed => -32004,
            ErrorCode::HostUnavailable => -32005,
            ErrorCode::Unauthorized => -32006,
        }
    }

    /// The name used for this code in `protocol/vibed-rpc.json`.
    pub fn name(self) -> &'static str {
        match self {
            ErrorCode::ParseError => "parse_error",
            ErrorCode::InvalidRequest => "invalid_request",
            ErrorCode::MethodNotFound => "method_not_found",
            ErrorCode::InvalidParams => "invalid_params",
            ErrorCode::InternalError => "internal_error",
            ErrorCode::ProtocolMismatch => "protocol_mismatch",
            ErrorCode::NotFound => "not_found",
            ErrorCode::Conflict => "conflict",
            ErrorCode::HostConfigInvalid => "host_config_invalid",
            ErrorCode::CommandFailed => "command_failed",
            ErrorCode::HostUnavailable => "host_unavailable",
            ErrorCode::Unauthorized => "unauthorized",
        }
    }

    pub fn from_code(code: i64) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.code() == code)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorObject {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        ErrorObject {
            code: code.code(),
            message: message.into(),
            data: None,
        }
    }

    /// Says this call went out and nobody heard how it ended: what it asked
    /// for may have happened, or may yet.
    pub fn outcome_unknown(self) -> Self {
        self.with_data(serde_json::json!({"outcome": "unknown"}))
    }

    /// Whether [`ErrorObject::outcome_unknown`] said so.
    pub fn is_outcome_unknown(&self) -> bool {
        self.data
            .as_ref()
            .and_then(|d| d.get("outcome"))
            .and_then(Value::as_str)
            == Some("unknown")
    }

    pub fn with_data(mut self, data: impl Serialize) -> Self {
        self.data = Some(serde_json::to_value(data).expect("error data serializes to JSON"));
        self
    }
}

impl fmt::Display for ErrorObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for ErrorObject {}

/// A method in the contract, with its parameter and result types.
pub trait Method {
    const NAME: &'static str;
    type Params: Serialize + DeserializeOwned;
    type Result: Serialize + DeserializeOwned;
}

/// A notification the daemon sends, with its parameter type.
pub trait Notification {
    const NAME: &'static str;
    type Params: Serialize + DeserializeOwned;
}

/// Declares a method: its marker type, name, and param and result types.
macro_rules! method {
    ($(#[$meta:meta])* $marker:ident = $name:literal, $params:ty => $result:ty) => {
        $(#[$meta])*
        #[derive(Debug)]
        pub enum $marker {}

        impl Method for $marker {
            const NAME: &'static str = $name;
            type Params = $params;
            type Result = $result;
        }
    };
}

/// Every method in the contract.
pub const METHODS: &[&str] = &[
    Hello::NAME,
    HostsList::NAME,
    HostsAdd::NAME,
    HostsUpgrade::NAME,
    HostsAdmin::NAME,
    HostsRemove::NAME,
    HostsExternal::NAME,
    TunnelPrepare::NAME,
    ProjectsList::NAME,
    ProjectsAdd::NAME,
    ProjectsRemove::NAME,
    ProfilesList::NAME,
    ProfilesCreate::NAME,
    ProfilesUpdate::NAME,
    ProfilesDelete::NAME,
    LimitsGet::NAME,
    LimitsSet::NAME,
    McpGet::NAME,
    McpSet::NAME,
    KeysList::NAME,
    KeysCreate::NAME,
    KeysRevoke::NAME,
    KeysVerify::NAME,
    KeysSync::NAME,
    GhStatusGet::NAME,
    EventsSubscribe::NAME,
    SessionsList::NAME,
    SessionsAttach::NAME,
    AgentsStart::NAME,
    AgentsList::NAME,
    AgentsDelete::NAME,
    AgentsInterrupt::NAME,
    AgentsSend::NAME,
    AgentsCheck::NAME,
    AgentsOutput::NAME,
    AgentsMessages::NAME,
    ToolboxStatusGet::NAME,
    ToolboxOpen::NAME,
    ToolboxUpdate::NAME,
    ToolboxRebuild::NAME,
    ToolboxLoginsGet::NAME,
    ToolboxClose::NAME,
    PiStatusGet::NAME,
    PiInstall::NAME,
    PiForget::NAME,
    DoctorCheckGet::NAME,
    DoctorFix::NAME,
    ServicesList::NAME,
    ServicesConnect::NAME,
    ServicesComplete::NAME,
    ServicesToken::NAME,
    ServicesFollow::NAME,
    ServicesOverview::NAME,
    ServicesDisconnect::NAME,
    ServicesSet::NAME,
    ServicesAudit::NAME,
];

/// Every notification the daemon sends.
pub const NOTIFICATIONS: &[&str] = &[EventNotice::NAME];

/// `daemon.hello`: the handshake. It names the client and checks that both
/// sides speak the same protocol version.
#[derive(Debug)]
pub enum Hello {}

impl Method for Hello {
    const NAME: &'static str = "daemon.hello";
    type Params = HelloParams;
    type Result = HelloResult;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloParams {
    /// Who is calling, for the daemon's log, e.g. `vibe-cli` or `VibeOS.app`.
    pub client: String,
    /// The client's [`PROTOCOL_VERSION`].
    pub protocol: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloResult {
    /// Always `vibed`.
    pub daemon: String,
    /// The daemon's version.
    pub version: String,
    /// The daemon's [`PROTOCOL_VERSION`].
    pub protocol: u32,
    /// `std::env::consts::OS`: `macos` or `linux`.
    pub os: String,
    /// `std::env::consts::ARCH`: `aarch64` or `x86_64`.
    pub arch: String,
    pub pid: u32,
}

/// The data of a [`ErrorCode::ProtocolMismatch`] error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolMismatchData {
    /// The daemon's [`PROTOCOL_VERSION`].
    pub protocol: u32,
}

/// The params of a method that takes none: `{}`, or none at all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoParams {}

// Every method that acts on one host takes an optional `host`: absent means
// this one, `local`. A call for a Linux host this one has added goes to that
// host over its link, and its answer comes back with the host's name in it.

method! {
    /// `hosts.list`: every host this daemon knows: itself first, then the
    /// Linux hosts added to it, each with its link.
    HostsList = "hosts.list", NoParams => HostList
}

method! {
    /// `hosts.add`: Add Linux host. Over SSH, installs or upgrades `vibed`
    /// on the server (static binaries and a systemd user service, with
    /// lingering on), checks `git`, `gh` and `claude` there, and starts the
    /// link. The server's logins stay its own.
    HostsAdd = "hosts.add", HostAddParams => HostInstall
}

method! {
    /// `hosts.upgrade`: installs this build's `vibed` on a host added already, if
    /// it runs another, and checks its tools again, now. A host runs behind by
    /// itself for no longer than it takes to reach it, so this is **Retry**.
    HostsUpgrade = "hosts.upgrade", HostNameParams => HostInstall
}

method! {
    /// `hosts.admin`: the one button for what a Linux host needs `sudo` for
    /// (`needs_admin` in its row): the line to run in a Terminal.
    HostsAdmin = "hosts.admin", HostNameParams => HostAdmin
}

method! {
    /// `hosts.remove`: forgets a Linux host and ends its link. Nothing on
    /// the server changes: its `vibed` and agents run on, on their own.
    HostsRemove = "hosts.remove", HostNameParams => HostInfo
}

method! {
    /// `hosts.external`: Settings → Hosts → *Use as external address*. On
    /// the Mac, for a Linux host added to it: records the choice, tells the
    /// host (now, or once its link is back), and keeps an SSH tunnel to it
    /// while it's on. On a Linux host, for `local`: serves MCP publicly at
    /// `https://<domain>/mcp`, with a certificate from Let's Encrypt. A Mac
    /// never serves it itself.
    HostsExternal = "hosts.external", HostExternalParams => HostInfo
}

method! {
    /// `tunnel.prepare`: on a Linux host, where the Mac's reverse tunnel
    /// is to end, a Unix socket in its private `run/`, with any left by an
    /// earlier tunnel cleared. The Mac calls it over the link before each
    /// `ssh -R`.
    TunnelPrepare = "tunnel.prepare", NoParams => TunnelPrepared
}

method! {
    /// `projects.list`: a host's projects, with their profiles.
    ProjectsList = "projects.list", HostParams => ProjectList
}

method! {
    /// `projects.add`: clones a GitHub repo into `~/VibeOS/projects/` with
    /// the host's `gh` login, or registers a local repo where it is. The
    /// project starts with its host's starter profiles.
    ProjectsAdd = "projects.add", ProjectAddParams => Project
}

method! {
    /// `projects.remove`: forgets a project and its profiles. Its files stay
    /// on disk. Refused while it has agents.
    ProjectsRemove = "projects.remove", ProjectRemoveParams => Project
}

method! {
    /// `profiles.list`: a project's profiles, oldest first.
    ProfilesList = "profiles.list", ProjectParams => ProfileList
}

method! {
    /// `profiles.create`: a new profile; unset fields take the defaults
    /// (`linux`, Opus, high, `auto`, 4 GB).
    ProfilesCreate = "profiles.create", ProfileCreateParams => Profile
}

method! {
    /// `profiles.update`: changes the fields given, and renames the profile
    /// when `name` is given.
    ProfilesUpdate = "profiles.update", ProfileUpdateParams => Profile
}

method! {
    /// `profiles.delete`: returns the profile it deleted.
    ProfilesDelete = "profiles.delete", ProfileParams => Profile
}

method! {
    /// `limits.get`: the host's limits, from `host.toml` over the defaults.
    LimitsGet = "limits.get", HostParams => HostLimits
}

method! {
    /// `limits.set`: writes the settings given into `host.toml` and returns
    /// the limits that result.
    LimitsSet = "limits.set", LimitsSetParams => HostLimits
}

method! {
    /// `mcp.get`: where this host serves MCP, for Settings and its Copy
    /// setup command (PRD: MCP interface, Transport).
    McpGet = "mcp.get", HostParams => McpSettings
}

method! {
    /// `mcp.set`: turns MCP on the Tailscale address on or off. It's
    /// written to `host.toml` and applied at once, and the settings that
    /// result are returned.
    McpSet = "mcp.set", McpSetParams => McpSettings
}

method! {
    /// `keys.list`: the API keys, oldest first. Never the keys themselves.
    KeysList = "keys.list", NoParams => KeyList
}

method! {
    /// `keys.create`: a new API key. Its secret is in this one response and
    /// nowhere else; only its hash is kept.
    KeysCreate = "keys.create", KeyCreateParams => KeyCreated
}

method! {
    /// `keys.revoke`: deletes a key, so it stops working at once.
    KeysRevoke = "keys.revoke", KeyParams => ApiKey
}

method! {
    /// `keys.verify`: checks a key and records that it was used.
    KeysVerify = "keys.verify", KeyVerifyParams => ApiKey
}

method! {
    /// `keys.sync`: replaces the keys pushed to this host by the Mac it's
    /// linked to, by their hashes, so one key works on every host. The
    /// host's own keys, made with `vibe key create`, are kept.
    KeysSync = "keys.sync", KeySyncParams => KeyList
}

method! {
    /// `gh.status`: the last `gh auth status`, run at startup and every
    /// hour, or now when `refresh` is set.
    GhStatusGet = "gh.status", GhStatusParams => GhStatus
}

method! {
    /// `events.subscribe`: from now on, this connection also gets an
    /// `events.event` notification for every event, after this response.
    EventsSubscribe = "events.subscribe", NoParams => Subscribed
}

method! {
    /// `sessions.list`: a host's sessions, by ref: those running, and those
    /// whose command has exited but whose record is still there.
    SessionsList = "sessions.list", HostParams => SessionList
}

method! {
    /// `sessions.attach`: what a client needs to attach to a running
    /// session. The client then talks to the session's shim directly, so
    /// restarting `vibed` doesn't interrupt it.
    SessionsAttach = "sessions.attach", SessionParams => SessionAttach
}

method! {
    /// `agents.start`: one call does it all (PRD: Key user flows, Start an
    /// agent). It cuts a worktree on a new branch, named by the agent's
    /// ref, from `origin`'s default branch or `from`; checks the host has
    /// disk to spare and room under `max_awake_agents` (never memory: an
    /// agent's memory is its ceiling, not a reservation); creates the
    /// compartment; runs `.vibe/setup.sh` there if the
    /// worktree has one; pre-fills its coding agent's state; and starts that on
    /// the prompt in a session of its own.
    AgentsStart = "agents.start", AgentStartParams => Agent
}

method! {
    /// `agents.list`: a host's agents, oldest first.
    AgentsList = "agents.list", HostParams => AgentList
}

method! {
    /// `agents.delete`: ends the agent and removes everything VibeOS made
    /// for it; its branch is kept. Refused while its worktree has
    /// uncommitted changes or unpushed commits, unless `force`.
    AgentsDelete = "agents.delete", AgentDeleteParams => Agent
}

method! {
    /// `agents.interrupt`: presses Escape in the agent's terminal, which
    /// stops Claude's turn, as the menu's Interrupt does. Refused unless its
    /// session runs; it never wakes an agent.
    AgentsInterrupt = "agents.interrupt", AgentParams => Agent
}

method! {
    /// `toolbox.status`: the toolbox's state, the template and Linux home
    /// versions new agents boot, the base images, and how the last toolbox
    /// run went.
    ToolboxStatusGet = "toolbox.status", NoParams => ToolboxStatus
}

method! {
    /// `toolbox.open`: opens the toolbox shell, a terminal in a VM where the
    /// template and the Linux home are writable, and says how to attach to
    /// it, as `sessions.attach` does. Exiting the shell saves a new version
    /// of each. There's one toolbox at a time: while it's open, this gives
    /// the same shell.
    ToolboxOpen = "toolbox.open", NoParams => SessionAttach
}

method! {
    /// `toolbox.update`: runs `toolbox.toml`'s `[update]` commands on the
    /// template now, as the weekly update does, and keeps the result as a
    /// new template version.
    ToolboxUpdate = "toolbox.update", NoParams => ToolboxRun
}

method! {
    /// `toolbox.rebuild`: **Rebuild template**. Boots the newest base image,
    /// replays `toolbox.toml`'s recipe into a fresh template, and lists
    /// what the old template has that the recipe doesn't cover. The Linux
    /// home carries over.
    ToolboxRebuild = "toolbox.rebuild", NoParams => ToolboxRun
}

method! {
    /// `toolbox.logins`: **See logins**. What the Linux home new agents
    /// boot holds, as the toolbox found when it saved that home, and
    /// whether linux agents have a Claude login. Reads only what's kept on
    /// the Mac: it never boots a VM.
    ToolboxLoginsGet = "toolbox.logins", NoParams => ToolboxLogins
}

method! {
    /// `toolbox.close`: **Save & Close**. Hangs up the open shell, as
    /// closing its Terminal window does, then saves a new template and
    /// Linux home version as exiting the shell does, and gives the status
    /// after. Attached terminals see the shell exit. A toolbox that's
    /// already closed is left as it is, so calling it twice is harmless; one
    /// busy updating or rebuilding is refused.
    ToolboxClose = "toolbox.close", NoParams => ToolboxStatus
}

method! {
    /// `pi.status`: Pi's install on a host and the providers it's signed in
    /// to, by name: never what they hold (PRD: Who gets which credentials).
    /// Reads only what's on disk, and asks `pi` its version. Never changes
    /// anything.
    PiStatusGet = "pi.status", HostParams => PiStatus
}

method! {
    /// `pi.install`: downloads the newest Pi, checked against its release's
    /// checksums, and gives the status after: `native` for the `pi` this
    /// host's native agents run (linked at `~/.local/bin/pi`, never over one
    /// that isn't VibeOS's), or on a Mac `vm` for the layer `linux` agents
    /// boot. It takes as long as the download does, and one runs at a time. A
    /// `pi.changed` event tells other clients, with `message` if it failed.
    PiInstall = "pi.install", PiInstallParams => PiStatus
}

method! {
    /// `pi.forget`: takes these providers' logins out of the store `linux` agents
    /// get theirs from, and so out of each agent's copy within seconds, and gives
    /// the status after. A way to clear what agents offered, or sign out, without
    /// the toolbox. Offered again as it was, a login stays out; anything new is a
    /// new offer, so an agent offering junk has to be stopped too. On a Mac only.
    PiForget = "pi.forget", PiForgetParams => PiStatus
}

method! {
    /// `doctor.check`: `vibe doctor`, and the first run's checklist (PRD:
    /// Key user flows, Install and setup). Runs every check now, as this
    /// host's `vibed` sees it: git, `gh`'s login and git using it, the
    /// `claude` macos agents run and its login, system sleep, Tailscale,
    /// and on a Mac the Linux images' download. Never changes anything.
    DoctorCheckGet = "doctor.check", NoParams => DoctorReport
}

method! {
    /// `doctor.fix`: does what VibeOS can about one check, then runs every
    /// check again. `gh_git` runs `gh auth setup-git`; `linux` downloads
    /// the Linux images now rather than at the next daily check. Every
    /// other check is the owner's to fix, as its `hint` says, and is
    /// refused.
    DoctorFix = "doctor.fix", DoctorFixParams => DoctorReport
}

method! {
    /// `agents.send`: types text into the agent's terminal and submits it,
    /// or presses keys, as `vibe_send` does. An agent asleep, paused or
    /// exited is woken or resumed first. Text is refused while it's on a
    /// question, unless `force`.
    AgentsSend = "agents.send", AgentSendParams => AgentSent
}

method! {
    /// `agents.check`: where the agent is, and whether it has answered the
    /// last send to it, for `vibe_wait`. Never wakes it.
    AgentsCheck = "agents.check", AgentCheckParams => AgentCheck
}

method! {
    /// `agents.output`: what its terminal shows, and what scrolled off it
    /// since `after`, as `vibe_read_output` gives it. Never wakes it.
    AgentsOutput = "agents.output", AgentOutputParams => AgentOutput
}

method! {
    /// `agents.messages`: the last messages of its conversation, from its
    /// transcript on its host, as `vibe_read_messages` gives them. Never
    /// wakes it.
    AgentsMessages = "agents.messages", AgentMessagesParams => MessageList
}

/// `events.event`: one event, sent to subscribed connections.
#[derive(Debug)]
pub enum EventNotice {}

impl Notification for EventNotice {
    const NAME: &'static str = "events.event";
    type Params = Event;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    /// `local` for this host.
    pub name: String,
    /// `macos` or `linux`.
    pub os: String,
    /// For people: the machine's hostname.
    pub label: String,
    /// How this daemon reaches it: `local` for itself; for another host,
    /// whether its SSH link is up.
    pub link: HostLink,
    /// Whether it's the external address for outside MCP callers (#18).
    pub external: bool,
    /// Whether it serves MCP now: on loopback, at least.
    pub mcp: bool,
    /// A Linux host's SSH destination, such as `me@vps.example.com`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// The `vibed` it runs, when it's known: this one's, or what another
    /// host said when its link last came up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// What a Linux host lacks of `git`, `gh` and `claude`, which its agents
    /// need; empty when it has them all, or isn't known.
    pub missing: Vec<String>,
    /// For a host whose link is down, why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// While it's the external address, where outside MCP callers reach
    /// it: `https://<domain>/mcp`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// While it's the external address, the Mac's tunnel to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<TunnelState>,
    /// While it's the external address and something keeps it from
    /// working, or holds it up, what and how to fix it: such as port 443,
    /// or Let's Encrypt not reaching the domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_detail: Option<String>,
    /// What its memory is doing now, as its kernel measures it: this
    /// host's own, and a Linux host's from its last ping while its link
    /// is up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryUse>,
    /// What this Linux host needs someone with `sudo` there to allow, once;
    /// `hosts.admin` says how. Empty almost always: only what VibeOS couldn't do
    /// itself. A need this build doesn't know, from a newer host, is left out.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "known_admin_needs"
    )]
    pub needs_admin: Vec<AdminNeed>,
    /// Why upgrading this Linux host's `vibed` failed, the last time this Mac tried.
    /// It tries again by itself; `hosts.upgrade` is **Retry**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade_error: Option<String>,
}

/// The needs this build knows of those a host names.
fn known_admin_needs<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<AdminNeed>, D::Error> {
    let names = <Vec<String> as Deserialize>::deserialize(d)?;
    Ok(names.iter().filter_map(|n| n.parse().ok()).collect())
}

/// A host's memory as its kernel measures it (#47). Nothing is admitted or
/// refused by it: each agent's memory is a ceiling it's held to, and the
/// host swaps if its agents use more than it has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryUse {
    /// Its physical memory, in bytes.
    pub total: u64,
    /// What's in use, in bytes: on macOS as Activity Monitor counts it
    /// (app memory, wired and compressed); on Linux, all but
    /// `MemAvailable`.
    pub used: u64,
    /// Its swap, in bytes: 0 with none.
    pub swap_total: u64,
    pub swap_used: u64,
    /// While the host is short of memory, as its kernel says (macOS's
    /// memory pressure at warn or critical; Linux's pressure stall
    /// information past its marks, or under 5% free where it has none),
    /// why. A warning only: no start or wake is refused for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressure: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostExternalParams {
    /// A Linux host this Mac added; or, absent or `local`, the Linux host
    /// this is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub on: bool,
    /// The public DNS name it's reached at, such as `vps.example.com`,
    /// pointing at the host. Needed to turn it on; ignored to turn it off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelPrepared {
    /// The absolute path, on the host, of the socket the tunnel ends at.
    pub socket: String,
}

/// `tunnel.call`, which only the Mac's tunnel socket takes (PRD: External
/// access, keys are checked where the agent lives): one of the agent calls
/// MCP makes, on this Mac, with the API key the outside caller sent, which
/// the Mac checks itself. Its result is the call's.
#[derive(Debug)]
pub enum TunnelCall {}

impl Method for TunnelCall {
    const NAME: &'static str = "tunnel.call";
    type Params = TunnelCallParams;
    type Result = Value;
}

/// The notification a peer sends as soon as it has a call it will run: `{id}`,
/// the call's id. What's listening knows within seconds that the far side is there,
/// however long the call itself goes on to take, so it needn't wait out a long
/// timeout on a Mac that's asleep. A client that isn't looking for it passes over it.
pub const CALL_ACCEPTED: &str = "call.accepted";

/// In a `daemon.hello` answer on the tunnel's socket or a link: this peer sends
/// [`CALL_ACCEPTED`], and on the tunnel's socket gives its memory with `agents.list`.
/// One without it is older, and isn't held to the accept deadline.
pub const ACCEPTS_CALLS: &str = "accepts_calls";

/// The [`CALL_ACCEPTED`] line for a message, if it's a call to a `method` that `wanted`
/// says is to get one.
pub fn accepted_frame(line: &[u8], wanted: impl FnOnce(&str) -> bool) -> Option<String> {
    let message: Value = serde_json::from_slice(line).ok()?;
    let id = message.get("id").filter(|id| !id.is_null())?;
    wanted(message.get("method")?.as_str()?).then(|| {
        serde_json::json!({"jsonrpc": "2.0", "method": CALL_ACCEPTED, "params": {"id": id}})
            .to_string()
    })
}

/// The calls the tunnel takes, each for this Mac's own agents only; and `hosts.list`,
/// answered with this Mac's own row alone, for a host older than its memory coming with
/// `agents.list`. On a link, these are the calls that get [`CALL_ACCEPTED`].
pub const TUNNEL_METHODS: &[&str] = &[
    "hosts.list",
    "projects.list",
    "agents.list",
    "agents.start",
    "agents.delete",
    "agents.send",
    "agents.check",
    "agents.output",
    "agents.messages",
];

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TunnelCallParams {
    /// The outside caller's API key, as it sent it.
    pub key: String,
    /// One of [`TUNNEL_METHODS`].
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// When the host sent it, in Unix seconds. A send, start or delete that arrives
    /// long after, as one held in the tunnel while a Mac slept does, doesn't run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<i64>,
}

impl fmt::Debug for TunnelCallParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TunnelCallParams")
            .field("key", &crate::keys::shown_prefix(&self.key))
            .field("method", &self.method)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostAddParams {
    /// What the host goes by, such as `vps`: it's how its agents' refs read
    /// (`vps:quiet-amber-otter`).
    pub name: String,
    /// Its SSH destination, as after `ssh`: `me@vps.example.com`, a
    /// Tailscale name, or a name from `~/.ssh/config`.
    pub address: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostNameParams {
    pub host: String,
}

/// How an install over SSH went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInstall {
    pub host: HostInfo,
    /// Whether this install changed what the server runs: new binaries, or
    /// a new service file. False when it had this build already.
    pub changed: bool,
    /// What the owner should know, in sentences: such as that lingering
    /// couldn't be turned on, and how to.
    pub notes: Vec<String>,
}

/// `hosts.admin`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostAdmin {
    /// The line a Terminal here runs: `ssh -t` to the host, `sudo` there asks
    /// for its password on that terminal. Built on this side from what's known
    /// to be needed, never from anything the host said. VibeOS never sees the
    /// password.
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostList {
    pub hosts: Vec<HostInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub host: String,
    pub name: String,
    pub source: ProjectSource,
    /// `owner/name`, for a project cloned from GitHub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// The repo's absolute path on its host.
    pub path: String,
    pub added_at: String,
    pub profiles: Vec<Profile>,
    /// Only in `projects.remove`'s answer: its clone was deleted from disk
    /// too, as on a Linux host with `delete`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectList {
    pub projects: Vec<Project>,
}

/// Exactly one of `repo` and `path`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectAddParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// A GitHub repo, `owner/name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// An absolute path to the top of a local git checkout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The project's name; by default the repo's or the directory's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub project: String,
}

/// What `projects.remove` takes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectRemoveParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub project: String,
    /// On a Linux host, delete the clone VibeOS made for it as well. A
    /// removal there that would delete a clone is refused without it, and
    /// says what it would delete; with it, one that can't safely delete
    /// the clone is refused, and without it that one is only forgotten.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileList {
    pub profiles: Vec<Profile>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileCreateParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub project: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<Environment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Model>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    /// Bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<u64>,
    /// Whether its agents get the shared MCP services; on unless it's
    /// `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_services: Option<bool>,
    /// Which coding agent it runs; Claude unless it says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentKind>,
    /// A Pi profile's model, as Pi is to be given it: `provider/id` or a bare id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pi_model: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileUpdateParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub project: String,
    /// The profile to change.
    pub profile: String,
    /// A new name for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<Environment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Model>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    /// Bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<u64>,
    /// Switches the shared MCP services on or off for agents started from
    /// here on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_services: Option<bool>,
    /// Another coding agent for it. Pi's mode is always `bypass`; going back
    /// to Claude goes back to `auto` unless `mode` says otherwise, and forgets
    /// its Pi model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentKind>,
    /// A Pi profile's model, as Pi is to be given it: `provider/id` or a bare id;
    /// empty clears it, for Pi's own default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pi_model: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub project: String,
    pub profile: String,
}

/// The settings to change; the others keep their values. Sizes are bytes,
/// `idle_debounce` is seconds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsSetParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reserve_cores: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_awake_agents: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_cores: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_processes: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_soft: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_hard: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vm_disk: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_debounce: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_disk: Option<u64>,
}

/// Where a host serves MCP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpSettings {
    /// The port, on loopback and Tailscale alike: `mcp_port` in
    /// `host.toml`, which is set there only.
    pub port: u16,
    /// Whether it's served on this host's Tailscale address too.
    pub tailscale: bool,
    /// Its loopback URL, `http://127.0.0.1:<port>/mcp`.
    pub url: String,
    /// Its URL on each of this host's Tailscale addresses while `tailscale`
    /// is on; none while it's off, or the host has no Tailscale address.
    pub tailscale_urls: Vec<String>,
}

/// The MCP settings to change; the others keep their values.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpSetParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tailscale: Option<bool>,
}

impl LimitsSetParams {
    /// The settings given, as (name, value).
    pub fn changes(&self) -> Vec<(&'static str, u64)> {
        let wide = |key, v: Option<u64>| v.map(|v| (key, v));
        let narrow = |key, v: Option<u32>| v.map(|v| (key, u64::from(v)));
        [
            narrow("reserve_cores", self.reserve_cores),
            narrow("max_awake_agents", self.max_awake_agents),
            narrow("cpu_cores", self.cpu_cores),
            narrow("max_processes", self.max_processes),
            wide("disk_soft", self.disk_soft),
            wide("disk_hard", self.disk_hard),
            wide("vm_disk", self.vm_disk),
            wide("idle_debounce", self.idle_debounce),
            wide("snapshot_disk", self.snapshot_disk),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

/// An API key, as it's shown: never the key itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKey {
    pub name: String,
    /// The start of the key, e.g. `vk_abcdefgh`, to tell keys apart.
    pub prefix: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
    /// It was made on the Mac this host is linked to, and pushed here by its
    /// hash. It's revoked there for everywhere, or here for this host alone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pushed: bool,
    /// On a Linux host: it was pushed here, and the Mac hasn't been heard from in a
    /// while, so a revoke made there may not have arrived.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
}

/// A key as the Mac pushes it to its hosts: its hash, never the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyHash {
    pub name: String,
    pub prefix: String,
    /// SHA-256 of the key, in lowercase hex.
    pub hash: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeySyncParams {
    /// Every key the Mac has now; any pushed before and not here is gone.
    pub keys: Vec<KeyHash>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyList {
    pub keys: Vec<ApiKey>,
    /// On a Mac: the Linux hosts that haven't had its latest keys, so a revoke isn't
    /// in force there yet. They get them when they're next reachable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_on: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyCreateParams {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyCreated {
    pub key: ApiKey,
    /// The key itself, `vk_…`. It can't be shown again.
    pub secret: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyParams {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyVerifyParams {
    pub secret: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GhStatusParams {
    /// Run `gh auth status` now instead of returning the last result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GhStatus {
    pub state: GhState,
    /// The GitHub account, when logged in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// What `gh` said, in a line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscribed {
    /// The `seq` of the last event before this subscription; the first one
    /// this connection gets is the next.
    pub seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(rename = "ref")]
    pub agent: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionList {
    pub sessions: Vec<SessionInfo>,
}

/// A session: the terminal an agent runs in, held by its shim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub host: String,
    #[serde(rename = "ref")]
    pub agent: String,
    pub state: SessionState,
    /// What `vibe attach` names the window: `<ref> · <profile>`, or the ref
    /// alone for a session with no agent.
    pub title: String,
    /// How many `vibe attach` clients are attached.
    pub clients: u32,
    /// The terminals those clients run in, such as `/dev/ttys003`, for
    /// those that have one.
    pub ttys: Vec<String>,
    /// How the command ended, once it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<SessionExit>,
}

/// How a session's command ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionExit {
    /// Its exit code, if it exited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    /// The signal that ended it, if one did, such as `SIGKILL`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    pub at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAttach {
    pub host: String,
    #[serde(rename = "ref")]
    pub agent: String,
    pub title: String,
    /// The shim's socket, to connect to.
    pub socket: String,
    /// The terminals of clients already attached, so the menu bar can raise
    /// one of those instead of opening another.
    pub ttys: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentStartParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub project: String,
    /// The project's profile to start it from.
    pub profile: String,
    /// What the agent starts on, given as an argument.
    pub prompt: String,
    /// The branch (or any commit) to cut the agent's branch from; by
    /// default `origin`'s default branch, or the project's `HEAD` when it
    /// has no `origin`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The ref, which names the worktree and the branch too; by default
    /// three random words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDeleteParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(rename = "ref")]
    pub agent: String,
    /// Delete even with uncommitted changes or unpushed commits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force: Option<bool>,
}

/// Names one agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(rename = "ref")]
    pub agent: String,
}

/// An agent: one Claude Code or Pi in its own worktree and compartment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    pub host: String,
    #[serde(rename = "ref")]
    pub agent: String,
    pub project: String,
    /// The profile it was started from. Its settings were copied at the
    /// start, so editing the profile changes only later agents.
    pub profile: String,
    /// Which coding agent it runs; left out for Claude.
    #[serde(
        rename = "agent",
        default,
        skip_serializing_if = "AgentKind::is_claude"
    )]
    pub kind: AgentKind,
    pub environment: Environment,
    pub state: AgentState,
    /// The worktree's absolute path, the same in every environment.
    pub worktree: String,
    pub branch: String,
    /// The `--session-id` it was started with, which names its transcript.
    pub session_id: String,
    pub created_at: String,
    /// The command that attaches a terminal to it.
    pub attach: String,
    /// Its memory limit, in bytes: its profile's memory when it started.
    pub memory: u64,
    /// What it uses now, in bytes, while its session runs and its backend
    /// can tell. Only `agents.list` and `agents.interrupt` measure it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_used: Option<u64>,
    /// What its transcript says it's doing.
    pub status: AgentStatus,
}

/// A `pi` a host's agents can run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiBinary {
    pub path: String,
    pub version: String,
    /// Whether it's the one `pi.install` put there, which it can update. One
    /// that isn't is the owner's own, which it leaves alone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub managed: bool,
}

/// The Pi layer `linux` agents boot on a Mac, and the login they share.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiVms {
    /// Pi's version in the newest layer, once there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The providers `linux` agents are signed in to, by name: the store each
    /// VM's private copy is kept in step with.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logins: Vec<String>,
    /// Why the layer couldn't be had when it was last tried in the background.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// What needs the owner's eye about those logins: what's wrong with an
    /// agent's copy, and what agents have done with a login.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alerts: Vec<String>,
    /// The logins here that came from an agent and not from the owner.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub offered: Vec<PiOffered>,
}

/// A login an agent signed in to, and every agent now uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiOffered {
    pub provider: String,
    /// The agent's ref.
    pub agent: String,
}

/// Pi on one host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiStatus {
    pub host: String,
    /// The `pi` this host's native agents run: `macos` agents on a Mac, every
    /// agent on a Linux host. None if it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<PiBinary>,
    /// Why VibeOS couldn't install or update that, the last time it tried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The providers the owner's own `~/.pi/agent` is signed in to, by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logins: Vec<String>,
    /// On a Mac: what `linux` agents, in their VMs, have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vms: Option<PiVms>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PiInstallParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub target: PiTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PiForgetParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The providers, by name.
    pub providers: Vec<String>,
}

/// What an agent's transcript says it's doing, from agent-tail (PRD:
/// Matching an agent to its transcript). Until a transcript is read, its
/// activity is `unknown`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatus {
    pub activity: Activity,
    pub certainty: Certainty,
    /// When the activity began.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// The session's title, once Claude has written one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The latest thing it did, as a line: `› fix the build`, `$ cargo
    /// test`, `Edit routing.ts`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    /// Wakeups it has armed and that haven't fired.
    pub pending_wakeups: Vec<Wakeup>,
    /// Why its turn stopped, beyond an error: `usage_limit` is shown as
    /// "limited until …".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    /// When a usage limit resets, if Claude recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    /// The error its turn ended with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A wakeup an agent has armed: a `/loop` or a cron job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wakeup {
    pub kind: WakeupKind,
    /// When it's due, if the transcript recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<String>,
    /// `firm` when a tool result recorded it, `inferred` from the call
    /// alone.
    pub certainty: Certainty,
    /// Whether it fires again after this time: a recurring cron job stays
    /// pending until it's deleted.
    pub recurring: bool,
    /// What it's for, if the model said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentList {
    pub agents: Vec<Agent>,
}

/// Something that happened on a host. Fields other than these four are set
/// when they apply to the kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Counts up from 1 for the daemon's life.
    pub seq: u64,
    pub at: String,
    pub host: String,
    pub kind: EventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The agent's ref.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The new state: an agent's for `agent.state`, a [`GhState`] for
    /// `gh.status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Which limit an `agent.limit` is about: `memory`, `processes`, `disk`
    /// or `cpu`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// An agent's new status, for `agent.status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<AgentStatus>,
}

/// The toolbox, for `toolbox.status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolboxStatus {
    pub state: ToolboxState,
    /// The newest template version, which new agents boot; absent until
    /// the toolbox makes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<u32>,
    /// The newest Linux home version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<u32>,
    /// The base release new agents boot: the one the template was built
    /// on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<u32>,
    /// The newest base release installed. Newer than `base`, the template
    /// wants a rebuild.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_base: Option<u32>,
    /// The last shell, update or rebuild to finish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<ToolboxRun>,
    /// How many terminals are attached to the open shell, as its shim says:
    /// 0 when it's open in the background, as after a detach. Absent when
    /// that isn't known: it isn't open, its shim isn't up yet or didn't
    /// answer, or a `vibed` from before this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clients: Option<u32>,
}

/// How a toolbox run went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolboxRun {
    pub action: ToolboxAction,
    /// When it finished.
    pub at: String,
    /// The template version it saved; absent if it saved none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<u32>,
    /// After a rebuild: what the old template has that the recipe doesn't
    /// cover, such as `apt: htop` or `/opt/google-cloud-sdk`.
    pub uncovered: Vec<String>,
    /// Why it failed, in which case nothing was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The logins linux agents start with, for `toolbox.logins`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolboxLogins {
    /// The Linux home version new agents boot; absent before the toolbox
    /// has saved one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<u32>,
    /// The gh, gcloud, git and SSH logins that home holds, found as the
    /// toolbox saved it; absent if it couldn't tell (the look failed, or a
    /// VibeOS before `toolbox.logins` saved it). Without a home, none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logins: Option<Vec<Login>>,
    /// Whether linux agents have a Claude login: the token `vibe
    /// setup-token` keeps, which lives on the Mac, not in the home.
    pub claude: bool,
    /// Without one, why, as a sentence: none kept yet, or it can't be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_problem: Option<String>,
}

/// What `doctor.check` found, check by check, in [`CheckId`]'s order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
}

/// One doctor check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub id: CheckId,
    pub state: CheckState,
    /// What it found, as a sentence: `git version 2.50.1`, `logged in to
    /// github.com as octocat`.
    pub detail: String,
    /// Unless it passed, what to do about it, as a sentence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Whether `doctor.fix` can do that.
    pub fixable: bool,
    /// While it's `working`, how far along, in percent, when that's known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoctorFixParams {
    pub check: CheckId,
}

/// One login the Linux home holds. Never its secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Login {
    pub tool: LoginTool,
    /// Whose it is, for people: `octocat on github.com` for `gh`, an
    /// account for `gcloud`, `Name <email>` for git, the key's file and
    /// comment for SSH.
    pub account: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSendParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(rename = "ref")]
    pub agent: String,
    /// Text to paste. Give this or `keys`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Whether Enter follows the text; true unless false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submit: Option<bool>,
    /// Paste even while the agent is on a question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force: Option<bool>,
    /// Keys to press, one at a time, by name: `"2"`, `"Enter"`, `"Down"`,
    /// `"Ctrl+C"`, as `vibe_send` takes them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keys: Option<Vec<String>>,
    /// Types it at most once: the host that types answers a repeat of this id
    /// for this agent with how the first went, for about 10 minutes. 1 to 128
    /// printable ASCII characters, no spaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSent {
    /// It was asleep, paused or exited, and was woken or resumed first.
    pub woke: bool,
    pub state: AgentState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCheckParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(rename = "ref")]
    pub agent: String,
    /// Count a turn as answered only once the commands it left running in
    /// the background are done too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCheck {
    pub state: AgentState,
    pub activity: Activity,
    /// The latest thing it did, as a line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    /// It has answered the last send to it, or its start's prompt.
    pub answered: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOutputParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(rename = "ref")]
    pub agent: String,
    /// A cursor from an earlier read: only what scrolled off since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentOutput {
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
    /// The live terminal, or the last one saved: its session has ended, or
    /// the agent is asleep.
    pub live: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMessagesParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(rename = "ref")]
    pub agent: String,
    /// How many of the last messages; 10 if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<u32>,
}

/// One message of an agent's conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub text: String,
    /// When it was written, as the transcript has it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageList {
    /// Oldest first.
    pub messages: Vec<Message>,
}

// Shared MCP services (PRD: Shared MCP services). Each host holds its own logins,
// so these go to the host they name, over its link, and only ever say how it
// stands. No token crosses a link: `services.connect` and `services.complete`
// have the host do everything but the browser, and a personal key is entered on
// the host it's for.

method! {
    /// `services.list`: the supported services, and how each stands.
    ServicesList = "services.list", HostParams => ServiceList
}

method! {
    /// `services.connect`: the first half of connecting or reconnecting a host
    /// with OAuth. That host registers its client and makes the PKCE verifier,
    /// which it keeps, and answers with the page for the app to open in a web
    /// authentication session. What's connected stays as it is until
    /// `services.complete`, so a sign-in given up on loses nothing.
    ServicesConnect = "services.connect", ServiceParams => ServiceAuthorization
}

method! {
    /// `services.complete`: the second half: the URL that session ended on. The
    /// code in it is useless without the host's verifier; the host makes the
    /// exchange and keeps the tokens.
    ServicesComplete = "services.complete", ServiceCompleteParams => ServiceInfo
}

method! {
    /// `services.disconnect`: forgets the login, here and, where it can, at
    /// the service.
    ServicesDisconnect = "services.disconnect", ServiceParams => ServiceInfo
}

method! {
    /// `services.set`: changes the settings given. If they can't be kept,
    /// nothing changes and it says so.
    ServicesSet = "services.set", ServiceSetParams => ServiceInfo
}

method! {
    /// `services.token`: the owner's current access token, refreshed first if
    /// `rejected` is what's current. The Mac asks it, for the other hosts.
    ServicesToken = "services.token", ServiceTokenParams => ServiceAccess
}

method! {
    /// `services.follow`: makes this host a follower of a login, with the access
    /// token it's to use. The Mac sends it. There's no way to send a refresh
    /// token this way.
    ServicesFollow = "services.follow", ServiceFollowParams => ServiceInfo
}

method! {
    /// `services.overview`: each service's one connection: who owns it, and how
    /// every host stands with it, those that don't answer in a few seconds
    /// marked so.
    ServicesOverview = "services.overview", NoParams => ServiceOverviewList
}

method! {
    /// `services.audit`: the last calls agents on that host made to its services,
    /// oldest first. Never arguments or tokens.
    ServicesAudit = "services.audit", HostParams => ServiceAuditList
}

/// One shared service, on one host. Nothing here is secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceInfo {
    /// Its name: `linear`. Its MCP server in an agent is `vibe-<name>`.
    pub name: String,
    /// Its name for people: `Linear`.
    pub title: String,
    pub state: ServiceState,
    /// This host's part in the login.
    pub role: ServiceRole,
    /// Who owns the login, for people, if it's another host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// When this host's access token runs out, if it has one and that's known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_until: Option<String>,
    /// A follower whose token was refused or ran out, waiting to be given another.
    #[serde(default)]
    pub wants_token: bool,
    /// Off, every agent's calls are refused: the kill switch.
    pub enabled: bool,
    /// Agents reach its read-only endpoint.
    pub read_only: bool,
    /// Tools no agent may list or call, by their names at the service.
    pub denied_tools: Vec<String>,
    /// Claude Code runs its tools without asking, for agents started from here
    /// on. Off, its own permission prompts govern them.
    pub auto_allow: bool,
    /// How many tools agents see.
    pub tools: u32,
    /// The last thing that went wrong, for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// When it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceList {
    pub services: Vec<ServiceInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub service: String,
}

/// What the app needs for a web authentication session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceAuthorization {
    pub service: String,
    /// The page to open.
    pub url: String,
    /// The scheme of the URL it ends on: `vibeos`.
    pub callback_scheme: String,
}

// By hand, below: it holds a sign-in's code.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceCompleteParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub service: String,
    /// The URL the session ended on, as it was.
    pub callback_url: String,
}

impl fmt::Debug for ServiceCompleteParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceCompleteParams")
            .field("host", &self.host)
            .field("service", &self.service)
            .finish_non_exhaustive()
    }
}

// By hand, below: it can hold an access token.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceTokenParams {
    pub service: String,
    /// The access token a follower had turned down, so that the owner refreshes
    /// if it's still the current one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected: Option<String>,
}

impl fmt::Debug for ServiceTokenParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceTokenParams")
            .field("service", &self.service)
            .finish_non_exhaustive()
    }
}

/// What the owner of a login gives out: an access token and how the login stands.
/// Never its refresh token.
// By hand, below: it holds an access token.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceAccess {
    pub service: String,
    pub state: ServiceState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<String>,
    /// When it runs out, in seconds since the epoch, if the service said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// What's wrong with the login, for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl fmt::Debug for ServiceAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceAccess")
            .field("service", &self.service)
            .field("state", &self.state)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// What the Mac tells a host that doesn't own a login. Any login of its own that
/// it held is forgotten, as another host has it now.
// By hand, below: it holds an access token.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceFollowParams {
    pub service: String,
    /// Who owns it, for people: a host's name, or `the Mac`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// How the login stands at its owner.
    pub state: ServiceState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl fmt::Debug for ServiceFollowParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceFollowParams")
            .field("service", &self.service)
            .field("owner", &self.owner)
            .field("state", &self.state)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// How one host stands with a service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceHost {
    /// The host's name; `local` is the one asked.
    pub host: String,
    /// Whether it answered just now.
    pub reachable: bool,
    /// What it says, if it answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<ServiceInfo>,
}

/// A service's one connection, as this install sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceOverview {
    pub name: String,
    pub title: String,
    /// How the login stands, as its owner last said.
    pub state: ServiceState,
    /// The host that holds the login, or would at the next sign-in.
    pub owner: String,
    /// Whether it holds it yet.
    pub signed_in: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail_at: Option<String>,
    pub hosts: Vec<ServiceHost>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceOverviewList {
    pub services: Vec<ServiceOverview>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSetParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denied_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_allow: Option<bool>,
}

/// One call an agent made to a service. Never its arguments or a token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceAuditEntry {
    pub at: String,
    /// The agent's ref, as far as its host could tell.
    pub agent: String,
    pub service: String,
    /// The tool, by its name at the service.
    pub tool: String,
    /// How it ended: `ok`, `tool_error`, `denied`, `off`, `invalid`,
    /// `not_connected`, `needs_reconnect`, `unavailable` or `cancelled`.
    pub outcome: String,
    /// How long it took, in milliseconds.
    pub ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceAuditList {
    pub entries: Vec<ServiceAuditEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn version_marker_must_be_2_0() {
        assert!(serde_json::from_value::<V2>(json!("2.0")).is_ok());
        assert!(serde_json::from_value::<V2>(json!("1.0")).is_err());
        assert!(serde_json::from_value::<V2>(json!(2.0)).is_err());
        assert_eq!(serde_json::to_value(V2).unwrap(), json!("2.0"));
    }

    #[test]
    fn absent_id_is_a_notification_but_null_id_is_not() {
        let note: Request =
            serde_json::from_value(json!({"jsonrpc": "2.0", "method": "m"})).unwrap();
        assert_eq!(note.id, None);
        assert_eq!(note.params, None);
        let null: Request =
            serde_json::from_value(json!({"jsonrpc": "2.0", "id": null, "method": "m"})).unwrap();
        assert_eq!(null.id, Some(Id::Null));
        assert_eq!(
            serde_json::to_value(&note).unwrap(),
            json!({"jsonrpc": "2.0", "method": "m"})
        );
        assert_eq!(
            serde_json::to_value(&null).unwrap(),
            json!({"jsonrpc": "2.0", "id": null, "method": "m"})
        );
    }

    #[test]
    fn null_params_are_kept_so_they_can_be_rejected() {
        let r: Request = serde_json::from_value(
            json!({"jsonrpc": "2.0", "id": 1, "method": "m", "params": null}),
        )
        .unwrap();
        assert_eq!(r.params, Some(Value::Null));
    }

    #[test]
    fn ids() {
        for (v, id) in [
            (json!(7), Id::Number(7)),
            (json!(-1), Id::Number(-1)),
            (json!("abc"), Id::String("abc".into())),
            (json!(null), Id::Null),
        ] {
            assert_eq!(serde_json::from_value::<Id>(v.clone()).unwrap(), id);
            assert_eq!(serde_json::to_value(&id).unwrap(), v);
        }
        assert!(serde_json::from_value::<Id>(json!(1.5)).is_err());
        assert!(serde_json::from_value::<Id>(json!([1])).is_err());
    }

    #[test]
    fn request_rejects_unknown_members() {
        let r = serde_json::from_value::<Request>(
            json!({"jsonrpc": "2.0", "id": 1, "method": "m", "param": {}}),
        );
        assert!(r.is_err());
    }

    #[test]
    fn response_has_exactly_one_outcome() {
        let ok: Response =
            serde_json::from_value(json!({"jsonrpc": "2.0", "id": 1, "result": null})).unwrap();
        assert_eq!(ok.outcome, Outcome::Result(Value::Null));
        assert_eq!(
            serde_json::to_value(&ok).unwrap(),
            json!({"jsonrpc": "2.0", "id": 1, "result": null})
        );
        let err: Response = serde_json::from_value(
            json!({"jsonrpc": "2.0", "id": "x", "error": {"code": -32601, "message": "no"}}),
        )
        .unwrap();
        assert_eq!(
            err.outcome,
            Outcome::Error(ErrorObject::new(ErrorCode::MethodNotFound, "no"))
        );
        for bad in [
            json!({"jsonrpc": "2.0", "id": 1}),
            json!({"jsonrpc": "2.0", "id": 1, "result": 1, "error": {"code": 1, "message": ""}}),
            json!({"jsonrpc": "1.0", "id": 1, "result": 1}),
        ] {
            assert!(serde_json::from_value::<Response>(bad).is_err());
        }
    }

    #[test]
    fn error_codes_are_unique_and_round_trip() {
        for c in ErrorCode::ALL {
            assert_eq!(ErrorCode::from_code(c.code()), Some(c));
            assert_eq!(
                ErrorCode::ALL
                    .iter()
                    .filter(|o| o.name() == c.name())
                    .count(),
                1
            );
        }
        assert_eq!(ErrorCode::from_code(1), None);
    }

    #[test]
    fn call_builds_a_typed_request() {
        let r = Request::call::<Hello>(
            Id::Number(3),
            &HelloParams {
                client: "t".into(),
                protocol: PROTOCOL_VERSION,
            },
        );
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({"jsonrpc": "2.0", "id": 3, "method": "daemon.hello",
                   "params": {"client": "t", "protocol": PROTOCOL_VERSION}})
        );
    }

    #[test]
    fn limits_set_lists_only_the_changes() {
        let p = LimitsSetParams {
            cpu_cores: Some(8),
            vm_disk: Some(1 << 30),
            ..Default::default()
        };
        assert_eq!(p.changes(), [("cpu_cores", 8), ("vm_disk", 1 << 30)]);
        let every = LimitsSetParams {
            host: None,
            reserve_cores: Some(1),
            max_awake_agents: Some(1),
            cpu_cores: Some(1),
            max_processes: Some(1),
            disk_soft: Some(1),
            disk_hard: Some(1),
            vm_disk: Some(1),
            idle_debounce: Some(1),
            snapshot_disk: Some(1),
        };
        let keys: Vec<&str> = every.changes().iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, HostLimits::keys().collect::<Vec<_>>());
    }

    #[test]
    fn notifications_have_no_id() {
        let e = Event {
            seq: 1,
            at: "t".into(),
            host: "local".into(),
            kind: EventKind::LimitsChanged,
            project: None,
            agent: Some("quiet-amber-otter".into()),
            state: None,
            limit: None,
            message: None,
            status: None,
        };
        assert_eq!(
            serde_json::to_value(Request::notification::<EventNotice>(&e)).unwrap(),
            json!({"jsonrpc": "2.0", "method": "events.event", "params": {
                "seq": 1, "at": "t", "host": "local", "kind": "limits.changed",
                "ref": "quiet-amber-otter"}})
        );
    }

    #[test]
    fn no_params_is_an_empty_object() {
        assert_eq!(serde_json::to_value(NoParams {}).unwrap(), json!({}));
        assert!(serde_json::from_value::<NoParams>(json!({"x": 1})).is_err());
    }

    #[test]
    fn hello_params_reject_unknown_fields() {
        let r = serde_json::from_value::<HelloParams>(
            json!({"client": "t", "protocol": 1, "extra": true}),
        );
        assert!(r.is_err());
    }
}
