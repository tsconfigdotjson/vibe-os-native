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
    Activity, AgentState, Certainty, Environment, EventKind, GhState, Mode, Model, Profile,
    ProjectSource, Reasoning, SessionState, StopReason, ToolboxAction, ToolboxState, WakeupKind,
};

/// Bumped on any incompatible change to the contract. Clients send theirs in
/// `daemon.hello`, and the daemon refuses a mismatch.
pub const PROTOCOL_VERSION: u32 = 1;

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
}

impl ErrorCode {
    pub const ALL: [ErrorCode; 10] = [
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
    ProjectsList::NAME,
    ProjectsAdd::NAME,
    ProjectsRemove::NAME,
    ProfilesList::NAME,
    ProfilesCreate::NAME,
    ProfilesUpdate::NAME,
    ProfilesDelete::NAME,
    LimitsGet::NAME,
    LimitsSet::NAME,
    KeysList::NAME,
    KeysCreate::NAME,
    KeysRevoke::NAME,
    KeysVerify::NAME,
    GhStatusGet::NAME,
    EventsSubscribe::NAME,
    SessionsList::NAME,
    SessionsAttach::NAME,
    AgentsStart::NAME,
    AgentsList::NAME,
    AgentsDelete::NAME,
    ToolboxStatusGet::NAME,
    ToolboxOpen::NAME,
    ToolboxUpdate::NAME,
    ToolboxRebuild::NAME,
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
// this one, `local`. Other hosts come with #16.

method! {
    /// `hosts.list`: every host this daemon knows; for now, itself.
    HostsList = "hosts.list", NoParams => HostList
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
    ProjectsRemove = "projects.remove", ProjectParams => Project
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
    /// ref, from `origin`'s default branch or `from`; checks admission;
    /// creates the compartment; runs `.vibe/setup.sh` there if the
    /// worktree has one; pre-fills Claude's state; and starts Claude on the
    /// prompt in a session of its own.
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
    pub reserve_memory: Option<u64>,
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

impl LimitsSetParams {
    /// The settings given, as (name, value).
    pub fn changes(&self) -> Vec<(&'static str, u64)> {
        let wide = |key, v: Option<u64>| v.map(|v| (key, v));
        let narrow = |key, v: Option<u32>| v.map(|v| (key, u64::from(v)));
        [
            wide("reserve_memory", self.reserve_memory),
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyList {
    pub keys: Vec<ApiKey>,
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
    /// What Claude starts on, given as an argument.
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

/// An agent: one Claude Code in its own worktree and compartment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    pub host: String,
    #[serde(rename = "ref")]
    pub agent: String,
    pub project: String,
    /// The profile it was started from. Its settings were copied at the
    /// start, so editing the profile changes only later agents.
    pub profile: String,
    pub environment: Environment,
    pub state: AgentState,
    /// The worktree's absolute path, the same in every environment.
    pub worktree: String,
    pub branch: String,
    /// Claude Code's `--session-id`, which names its transcript.
    pub session_id: String,
    pub created_at: String,
    /// The command that attaches a terminal to it.
    pub attach: String,
    /// What its transcript says it's doing.
    pub status: AgentStatus,
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
            reserve_memory: Some(1),
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
