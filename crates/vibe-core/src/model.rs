//! The registry's domain: hosts, projects and profiles, and the rules their
//! names and settings follow (PRD: Core concepts, Environments and
//! profiles).

use std::fmt;
use std::str::FromStr;

/// How much of an alert about Pi's logins is shown, in characters.
const MAX_ALERT_CHARS: usize = 400;

/// An alert about Pi's logins, as it's safe to print or show: part of it can have
/// come from a guest, and it ends up on the owner's terminal. Nothing that moves a
/// cursor, clears a screen or reverses text, and no more than a few lines of it.
pub fn clean_alert(alert: &str) -> String {
    let bidi = |c: char| matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
    let clean: String = alert
        .chars()
        .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control() && !bidi(*c))
        .collect();
    match clean.char_indices().nth(MAX_ALERT_CHARS) {
        Some((at, _)) => format!("{}…", &clean[..at]),
        None => clean,
    }
}

/// How to sign in to Pi for `linux` agents: the app does it, and for whoever's only
/// at a terminal there's the toolbox, which has no browser, and where a browser
/// login's localhost callback can't be reached from the Mac's. Each option named
/// is what Pi 1.5 calls it (`auth/oauth/`, in its source): check them when the pin moves.
pub const PI_TOOLBOX_SIGN_IN: &str = "Sign in from the VibeOS app, in Settings → Agents. At a \
    terminal, in the Linux toolbox (`vibe toolbox`), run `pi`, then /login. \
    Don't choose Browser login: the toolbox has no browser, and that login can't call back to \
    it. Choose an API key, or for a subscription Anthropic's \"Copy code login (headless)\" or \
    OpenAI's \"Device code login (headless)\". Open the link Pi shows in your Mac's browser, and \
    give Pi the code, or paste back the address you end up on if it asks for a redirect URL";

use serde::{Deserialize, Serialize};

use crate::units::{GIB, MIB, TIB};

/// What every `vibed` calls itself. Other hosts get the name they were added
/// under (#16), such as `vps`, which is how `vps:quiet-amber-otter` reads.
pub const LOCAL_HOST: &str = "local";

/// Declares a closed set of names, as serialized on the socket and stored in
/// the registry, with the list `protocol/vibed-rpc.json` checks it against.
macro_rules! names {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident = $s:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $($(#[$vmeta])* #[serde(rename = $s)] $variant),+
        }

        impl $name {
            pub const ALL: &[$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $s),+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, String> {
                $name::ALL
                    .iter()
                    .copied()
                    .find(|v| v.as_str() == s)
                    .ok_or_else(|| format!("unknown {}: `{s}`", stringify!($name)))
            }
        }
    };
}

names! {
    /// Where an agent runs: a micro-VM (or a cgroup on a Linux host), or
    /// natively on the Mac.
    Environment { Linux = "linux", Macos = "macos" }
}

names! {
    /// Which coding agent a profile runs (PRD: Profiles): Claude Code, or
    /// Pi. Missing from a profile or an agent that predates Pi, which is
    /// then Claude's.
    #[derive(Default)]
    AgentKind { #[default] Claude = "claude", Pi = "pi" }
}

impl AgentKind {
    /// Whether it's the default, which the socket leaves out.
    pub fn is_claude(&self) -> bool {
        *self == AgentKind::Claude
    }
}

names! {
    /// Which Pi `pi.install` installs: the `pi` this host's native agents run,
    /// or the layer `linux` agents boot in their VMs on a Mac.
    PiTarget { Native = "native", Vm = "vm" }
}

names! {
    /// How a Pi sign-in goes: a provider's own account, through a browser or a
    /// code, or an API key.
    PiLoginMethod { Oauth = "oauth", ApiKey = "api_key" }
}

names! {
    /// Where a Pi sign-in has got to.
    PiLoginState { Running = "running", Done = "done", Failed = "failed", Cancelled = "cancelled" }
}

names! {
    /// What a question in a Pi sign-in wants: something that can be shown as it's
    /// typed, something that can't, one of a list, or what a browser ended on.
    PiPromptKind { Text = "text", Secret = "secret", Select = "select", ManualCode = "manual_code" }
}

names! {
    /// Claude's `--model`. A Pi profile names its model in `pi_model`.
    Model { Opus = "opus", Sonnet = "sonnet" }
}

names! {
    /// Claude's `--effort`, and Pi's `--thinking`, which takes the same
    /// names.
    Reasoning { Low = "low", Medium = "medium", High = "high", Xhigh = "xhigh", Max = "max" }
}

names! {
    /// Claude's permission mode: `auto`, or `bypass`
    /// (`--dangerously-skip-permissions`). Pi never asks before a tool
    /// runs, so a Pi profile is always `bypass`.
    Mode { Auto = "auto", Bypass = "bypass" }
}

names! {
    /// How a project got onto its host.
    ProjectSource {
        /// Cloned by `owner/name` with the host's `gh` login.
        Github = "github",
        /// A local repo, registered where it is.
        Path = "path",
    }
}

names! {
    /// The result of the last `gh auth status`.
    GhState {
        /// Not checked yet.
        Unknown = "unknown",
        LoggedIn = "logged_in",
        LoggedOut = "logged_out",
        NotInstalled = "not_installed",
        /// The check itself failed, e.g. it timed out.
        Error = "error",
    }
}

names! {
    /// What an event on the stream is about.
    EventKind {
        ProjectAdded = "project.added",
        ProjectRemoved = "project.removed",
        /// A profile of `project` was created, changed or deleted.
        ProfilesChanged = "profiles.changed",
        LimitsChanged = "limits.changed",
        KeysChanged = "keys.changed",
        /// `gh auth status` changed; `state` is the new [`GhState`].
        GhStatus = "gh.status",
        /// An agent was started; `state` is its first [`AgentState`].
        AgentAdded = "agent.added",
        /// An agent was deleted, with everything VibeOS made for it.
        AgentDeleted = "agent.deleted",
        /// An agent changed state; `state` is the new [`AgentState`].
        AgentState = "agent.state",
        /// What an agent's transcript says changed: its activity, title,
        /// latest activity line, wakeups or stop; `status` is the new
        /// [`AgentStatus`](crate::rpc::AgentStatus).
        AgentStatus = "agent.status",
        /// An agent hit a limit; `limit` says which (#6, #11, #17).
        AgentLimit = "agent.limit",
        /// The subscriber fell behind and missed events: re-read everything.
        Lagged = "lagged",
        /// The toolbox opened, or saved, updated or rebuilt the template,
        /// or failed to; `message` says which.
        ToolboxChanged = "toolbox.changed",
        /// Where MCP is served changed: `mcp.set` turned the Tailscale
        /// address on or off, or MCP started or stopped being served on
        /// loopback (`HostInfo.mcp`).
        McpChanged = "mcp.changed",
        /// A Linux host was added under `host`'s name, and its link starts.
        HostAdded = "host.added",
        /// A Linux host was forgotten; it runs on there, on its own.
        HostRemoved = "host.removed",
        /// A Linux host's link came up or went down, or what it reports
        /// changed; `state` is the new [`HostLink`], `message` why it's
        /// down.
        HostLink = "host.link",
        /// A shared service changed: its state, settings or tools.
        ServiceChanged = "service.changed",
        /// Pi's install on this host changed, or an install ended; `message`
        /// says why one failed.
        PiChanged = "pi.changed",
    }
}

names! {
    /// Where a shared service stands, on the host that holds its login.
    ServiceState {
        /// Never connected, or disconnected on purpose.
        Disconnected = "disconnected",
        Connected = "connected",
        /// A refresh is under way.
        Refreshing = "refreshing",
        /// This host's access token has run out or been refused, and the host
        /// that owns the login can't be reached to give it another.
        OwnerUnreachable = "owner_unreachable",
        /// A refresh was refused (`invalid_grant`, `invalid_client`): only
        /// connecting again helps.
        NeedsReconnect = "needs_reconnect",
        /// The service turned down a freshly refreshed token, or a key: the
        /// owner revoked it there.
        Revoked = "revoked",
    }
}

names! {
    /// A host's part in a service's one login.
    ServiceRole {
        /// It holds the refresh token, and it alone ever refreshes.
        Owner = "owner",
        /// It holds only an access token, given to it by way of the Mac.
        Follower = "follower",
    }
}

names! {
    /// How a host's `vibed` is reached from this one.
    HostLink {
        /// It's this host.
        Local = "local",
        /// Another host, whose link is up.
        Connected = "connected",
        /// Another host, whose link is down: its agents are unavailable.
        Down = "down",
    }
}

names! {
    /// Who said a message of an agent's conversation.
    MessageRole {
        User = "user",
        Assistant = "assistant",
        /// A tool call, as one short line.
        Tool = "tool",
    }
}

names! {
    /// Whether a session's shim is still holding its terminal.
    SessionState {
        /// Its shim is up, and clients can attach.
        Running = "running",
        /// Its command ended; the shim left a record of how.
        Exited = "exited",
    }
}

names! {
    /// What an agent is doing, as clients see it (PRD: MCP interface,
    /// States). The status engine tells `busy`, `idle` and `question` apart
    /// from the agent's transcript; `asleep`, `paused` and `exited` are
    /// `vibed`'s own.
    AgentState {
        Busy = "busy",
        Idle = "idle",
        /// Waiting on an answer: a menu, a yes/no, a permission prompt.
        Question = "question",
        /// Frozen, or saved to disk; a send wakes it.
        Asleep = "asleep",
        /// Stopped by a limit.
        Paused = "paused",
        /// Its processes are gone; a send resumes it with `claude --resume`.
        Exited = "exited",
    }
}

names! {
    /// What an agent's transcript says it's doing: agent-tail's finer
    /// `activity`, which [`AgentState`] folds (PRD: MCP interface, States).
    Activity {
        /// It asked a question, or for its plan to be approved.
        AwaitingAnswer = "awaiting_answer",
        /// A tool call may be waiting on a permission prompt.
        AwaitingApproval = "awaiting_approval",
        RunningTool = "running_tool",
        Thinking = "thinking",
        /// Its turn ended, but a subagent or background task still runs.
        BackgroundWork = "background_work",
        Done = "done",
        Interrupted = "interrupted",
        /// Its turn ended with an error, such as a usage limit.
        Errored = "errored",
        /// No entry that moves the state has been read yet.
        Unknown = "unknown",
    }
}

names! {
    /// Whether the transcript says so, or it's a guess from timing.
    Certainty { Firm = "firm", Inferred = "inferred" }
}

names! {
    /// What armed a scheduled wakeup: a dynamic `/loop` wakeup, or a cron
    /// job (`/loop 5m` makes one).
    WakeupKind { Loop = "loop", Cron = "cron" }
}

names! {
    /// Why a turn stopped, when an error alone doesn't say.
    StopReason {
        /// A Claude usage limit; the next send starts a new turn.
        UsageLimit = "usage_limit",
    }
}

names! {
    /// What the toolbox is doing (PRD: The toolbox shell). `unavailable` on
    /// a host with none: a Linux host, or a Mac without `vibe-vm`.
    ToolboxState {
        Unavailable = "unavailable",
        Closed = "closed",
        Open = "open",
        /// The shell exited, and the new versions are being kept.
        Saving = "saving",
        Updating = "updating",
        Rebuilding = "rebuilding",
    }
}

names! {
    /// What a toolbox run was: a shell session, the weekly `[update]`, or
    /// **Rebuild template**.
    ToolboxAction { Shell = "shell", Update = "update", Rebuild = "rebuild" }
}

names! {
    /// What a login kept in the Linux home is for (PRD: Core concepts,
    /// Linux home): `gh`, `gcloud`, git's identity, or an SSH key.
    LoginTool { Gh = "gh", Gcloud = "gcloud", Git = "git", Ssh = "ssh" }
}

names! {
    /// What `vibe doctor` checks (PRD: Key user flows, Install and setup),
    /// in the order it reports them: git; `gh`'s login, and git using `gh`
    /// for GitHub (`gh auth setup-git`); the `claude` macos agents run, and
    /// its login; system sleep off, on a Mac; on a Linux host, `vibed` staying
    /// up while the owner's logged out (lingering); Tailscale, which is
    /// optional; on a Mac, the Linux base image and Claude Code for linux
    /// agents; and Pi, which is optional too.
    CheckId {
        Git = "git",
        Gh = "gh",
        GhGit = "gh_git",
        Claude = "claude",
        ClaudeLogin = "claude_login",
        Power = "power",
        Linger = "linger",
        Tailscale = "tailscale",
        Linux = "linux",
        Pi = "pi",
    }
}

names! {
    /// What can be signed in to from the app: `github` and `claude`, on the host that's
    /// to have the login; `toolbox_github` and `toolbox_claude`, for a Mac's linux
    /// agents.
    SignInWhat {
        Github = "github",
        Claude = "claude",
        ToolboxGithub = "toolbox_github",
        ToolboxClaude = "toolbox_claude",
    }
}

names! {
    /// How a sign-in stands.
    SignInState { Running = "running", Done = "done", Failed = "failed", Cancelled = "cancelled" }
}

names! {
    /// What a Linux host needs someone with `sudo` there to allow, once, because
    /// VibeOS tried every way it has without a password and none worked:
    /// `linger`, `vibed` staying up while the owner's logged out; `git`, which agents
    /// can't do without and only a package manager can install.
    AdminNeed { Linger = "linger", Git = "git" }
}

names! {
    /// How a doctor check went: `pass`; `fail`, which needs the owner;
    /// `optional`, missing but not needed; or `working`, still under way
    /// (a download).
    CheckState { Pass = "pass", Fail = "fail", Optional = "optional", Working = "working" }
}

/// The default memory of a profile.
pub const DEFAULT_PROFILE_MEMORY: u64 = 4 * GIB;
pub const MIN_PROFILE_MEMORY: u64 = GIB;
pub const MAX_PROFILE_MEMORY: u64 = TIB;

/// A per-project preset for starting agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub environment: Environment,
    pub model: Model,
    pub reasoning: Reasoning,
    pub mode: Mode,
    /// The agent's high-water mark in bytes: a `linux` VM's RAM, or a
    /// `macos` agent's hard limit.
    pub memory: u64,
    /// Whether its agents get the shared MCP services (PRD: Shared MCP
    /// services). On for every profile unless it's switched off, so only
    /// `false` is ever written, and a host that doesn't know of them reads
    /// as on.
    #[serde(default = "shared_services_on", skip_serializing_if = "is_on")]
    pub shared_services: bool,
    /// Which coding agent it runs. Claude's unless it says otherwise, so
    /// what was saved before Pi still reads the same.
    #[serde(default, skip_serializing_if = "AgentKind::is_claude")]
    pub agent: AgentKind,
    /// A Pi profile's model, as Pi is to be given it: `provider/id` or a bare id.
    /// Without one, Pi uses what its own settings and logins give: VibeOS doesn't
    /// pick a provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pi_model: Option<String>,
}

fn shared_services_on() -> bool {
    true
}

fn is_on(on: &bool) -> bool {
    *on
}

impl Profile {
    /// A profile with the defaults: Opus, high reasoning, `auto`, 4 GB.
    pub fn with_defaults(name: impl Into<String>, environment: Environment) -> Self {
        Profile {
            name: name.into(),
            environment,
            model: Model::Opus,
            reasoning: Reasoning::High,
            mode: Mode::Auto,
            memory: DEFAULT_PROFILE_MEMORY,
            shared_services: true,
            agent: AgentKind::Claude,
            pi_model: None,
        }
    }

    /// Checks the profile can be used on a host running `host_os`.
    pub fn validate(&self, host_os: &str) -> Result<(), String> {
        validate_name("profile", &self.name)?;
        if self.environment == Environment::Macos && host_os != "macos" {
            return Err(format!(
                "`{}` uses the macos environment, which only a Mac host has",
                self.name
            ));
        }
        match self.agent {
            AgentKind::Claude => {
                if self.pi_model.is_some() {
                    return Err(format!(
                        "`{}` runs Claude, so it can't have a Pi model",
                        self.name
                    ));
                }
            }
            AgentKind::Pi => {
                if let Some(model) = &self.pi_model {
                    validate_pi_model(model)?;
                }
                // The same opt-in as Claude's bypass on the Mac: Pi never
                // asks, and `macos` has no file sandbox.
                if self.environment == Environment::Macos && self.mode != Mode::Bypass {
                    return Err(format!(
                        "`{}` would run Pi on this Mac, unsandboxed, and Pi never asks before \
                         running a tool. Use the linux environment, or set its mode to bypass \
                         to say that's what you want",
                        self.name
                    ));
                }
                if self.environment == Environment::Linux && self.mode != Mode::Bypass {
                    return Err(format!(
                        "`{}` runs Pi, which has no permission modes: leave its mode out",
                        self.name
                    ));
                }
            }
        }
        validate_memory(self.memory)
    }
}

impl Profile {
    /// Keeps the mode true to the agent, after a change of agent or
    /// environment from `was`, or on a new profile, with `asked` the mode this
    /// call named. Pi has no modes, so in `linux` it's simply `bypass`. On the
    /// Mac that's the owner's opt-in, which has to be said in the call that
    /// puts Pi there: a `bypass` left from `linux` doesn't count, nor does it
    /// carry over to Claude, which goes back to `auto`.
    pub fn settle_mode(&mut self, was: Option<(AgentKind, Environment)>, asked: Option<Mode>) {
        self.mode = match (self.agent, self.environment) {
            (AgentKind::Pi, Environment::Linux) => Mode::Bypass,
            (AgentKind::Pi, Environment::Macos) => {
                if was == Some((AgentKind::Pi, Environment::Macos)) {
                    asked.unwrap_or(self.mode)
                } else {
                    asked.unwrap_or(Mode::Auto)
                }
            }
            (AgentKind::Claude, _) => match was {
                Some((AgentKind::Pi, _)) => asked.unwrap_or(Mode::Auto),
                _ => asked.unwrap_or(self.mode),
            },
        };
    }
}

/// The longest Pi model name there can be, in bytes.
pub const MAX_PI_MODEL_BYTES: usize = 200;

/// Whether `model` is safe to give Pi as one argument, which is all that's asked
/// of it: `provider/id`, a bare id, whatever Pi may know now or later. Pi says if
/// it doesn't know it, once it runs. Never read as an option, split by a shell or
/// shown with what moves a cursor. `isValidPiModel`, in Swift, is the same rule, and
/// `protocol/pi-models.json` holds both to it.
pub fn validate_pi_model(model: &str) -> Result<(), String> {
    let ok = !model.is_empty()
        && model.len() <= MAX_PI_MODEL_BYTES
        && !model.starts_with('-')
        && model.chars().all(|c| {
            !c.is_whitespace() && !c.is_control() && !matches!(c, '\'' | '"' | '`' | '$' | '\\')
        });
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{}` isn't a Pi model: it can't be empty, start with -, have spaces or any of \
             ' \" ` $ \\ in it, or run past {MAX_PI_MODEL_BYTES} bytes",
            clean_alert(model)
        ))
    }
}

/// The profiles a new project starts with: one per environment the host
/// has (PRD: Profiles).
pub fn starter_profiles(host_os: &str) -> Vec<Profile> {
    let mut profiles = vec![Profile::with_defaults(
        "software-eng-linux",
        Environment::Linux,
    )];
    if host_os == "macos" {
        profiles.push(Profile::with_defaults(
            "software-eng-mac",
            Environment::Macos,
        ));
    }
    profiles
}

pub fn validate_memory(bytes: u64) -> Result<(), String> {
    if !bytes.is_multiple_of(MIB) {
        return Err(format!(
            "memory must be a whole number of MiB, not {bytes} bytes"
        ));
    }
    if !(MIN_PROFILE_MEMORY..=MAX_PROFILE_MEMORY).contains(&bytes) {
        return Err(format!(
            "memory must be between 1G and 1T, not {} MiB",
            bytes / MIB
        ));
    }
    Ok(())
}

/// Names of projects, profiles and API keys: up to 100 letters, digits, `.`,
/// `_` and `-`, starting with a letter or digit. Project names become
/// directory names, so nothing else is allowed.
pub fn validate_name(what: &str, name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 100
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{name}` isn't a valid {what} name: use up to 100 letters, digits, `.`, `_` and `-`, starting with a letter or digit"
        ))
    }
}

/// The longest host name: it goes in front of refs (`vps:quiet-amber-otter`)
/// and in menus, so it stays short.
pub const MAX_HOST_NAME_LEN: usize = 32;

/// A Linux host's name, such as `vps`: like a project's (see
/// [`validate_name`]) but at most [`MAX_HOST_NAME_LEN`] characters, and never
/// `local`, which every `vibed` calls itself.
pub fn validate_host_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= MAX_HOST_NAME_LEN
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !ok {
        return Err(format!(
            "`{name}` isn't a valid host name: use up to {MAX_HOST_NAME_LEN} letters, digits, `.`, `_` and `-`, starting with a letter or digit"
        ));
    }
    if name == LOCAL_HOST {
        return Err(format!(
            "`{LOCAL_HOST}` is what every vibed calls itself; pick another name"
        ));
    }
    Ok(())
}

/// An SSH destination as `ssh` takes it: `host`, `user@host`,
/// `ssh://user@host:port` or a name from `~/.ssh/config`. It may not start
/// with `-`, so it can't be read as an
/// option, and holds no spaces, quotes or control characters, so it's one
/// word in every shell.
pub fn validate_ssh_destination(dest: &str) -> Result<(), String> {
    let ok = !dest.is_empty()
        && dest.len() <= 255
        && !dest.starts_with('-')
        && dest.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '.' | '_' | '-' | '@' | ':' | '/' | '[' | ']' | '%')
        });
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{dest}` isn't an SSH destination: write it as you would after `ssh`, such as me@vps.example.com"
        ))
    }
}

/// The longest ref. A ref names a socket, `run/<ref>.sock`, whose path must
/// fit in 103 bytes on macOS. That leaves 26 for the home directory, such as
/// `/Users/` and a name of up to 19 letters.
pub const MAX_REF_LEN: usize = 32;

/// The names a session's socket can't have in `run/`: `vibed.sock`, the daemon's,
/// and `mac.sock` and `tunnel.sock`, which a `vibed` from before External Address
/// was retired (#143) may have left there, the first with an old Mac's `ssh -R`
/// still holding it. A new agent may not take one, in any case, as the Mac's file
/// system ignores case: its session would be that socket (issue #45). Only `vibed`
/// was reserved before 0.1.3, so an agent may already be called `mac` or `tunnel`;
/// it's still a ref, and can be used and deleted ([`validate_ref`]).
pub const RESERVED_REFS: [&str; 3] = ["vibed", "mac", "tunnel"];

/// Whether `r` is one of [`RESERVED_REFS`], in any case.
pub fn is_reserved_ref(r: &str) -> bool {
    RESERVED_REFS.iter().any(|n| n.eq_ignore_ascii_case(r))
}

fn kept_for_vibeos(r: &str) -> String {
    format!("`{r}` is a name VibeOS keeps for itself; pick another name")
}

/// An agent's ref, which also names its worktree, branch and session: like
/// a name (see [`validate_name`]) but at most [`MAX_REF_LEN`] characters,
/// and not `vibed`, whose socket is in the same directory. Every ref an
/// agent may have; a new one is held to [`validate_new_ref`].
pub fn validate_ref(r: &str) -> Result<(), String> {
    let ok = !r.is_empty()
        && r.len() <= MAX_REF_LEN
        && r.starts_with(|c: char| c.is_ascii_alphanumeric())
        && r.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !ok {
        return Err(format!(
            "`{r}` isn't a valid ref: use up to {MAX_REF_LEN} letters, digits, `.`, `_` and `-`, starting with a letter or digit"
        ));
    }
    if r == "vibed" {
        return Err(kept_for_vibeos(r));
    }
    Ok(())
}

/// The ref of an agent about to be made: a valid ref, and none of
/// [`RESERVED_REFS`] in any case.
pub fn validate_new_ref(r: &str) -> Result<(), String> {
    validate_ref(r)?;
    if is_reserved_ref(r) {
        return Err(kept_for_vibeos(r));
    }
    Ok(())
}

/// The words of a new agent's ref: one of each list, such as
/// `quiet-amber-otter`. 64 × 64 × 64 is 262,144 refs per host, and none is
/// longer than [`MAX_REF_LEN`].
const REF_WORDS: [[&str; 64]; 3] = [
    [
        "able", "agile", "alert", "ample", "avid", "bold", "brave", "brisk", "calm", "candid",
        "civil", "clear", "clever", "cosy", "crisp", "curious", "daring", "deft", "eager", "early",
        "easy", "fair", "fancy", "fond", "frank", "free", "fresh", "gentle", "glad", "grand",
        "happy", "hardy", "honest", "jolly", "keen", "kind", "lively", "loyal", "lucky", "merry",
        "mild", "modest", "neat", "nimble", "noble", "patient", "plucky", "polite", "proud",
        "quick", "quiet", "rapid", "ready", "sharp", "shy", "sleek", "smart", "steady", "sunny",
        "swift", "tidy", "warm", "wise", "witty",
    ],
    [
        "amber", "aqua", "ashen", "azure", "beige", "black", "bronze", "brown", "cedar", "cerise",
        "chalk", "cobalt", "copper", "coral", "cream", "crimson", "cyan", "denim", "dusky",
        "ebony", "fawn", "flint", "golden", "granite", "green", "grey", "hazel", "honey", "indigo",
        "ivory", "jade", "khaki", "lemon", "lilac", "lime", "linen", "maple", "maroon", "mauve",
        "mint", "navy", "ochre", "olive", "onyx", "opal", "orange", "pearl", "pewter", "pink",
        "plum", "red", "rose", "ruby", "russet", "rust", "sable", "sage", "sandy", "scarlet",
        "silver", "slate", "teal", "violet", "white",
    ],
    [
        "badger", "beaver", "bison", "bobcat", "crane", "cougar", "coyote", "deer", "dingo",
        "dove", "eagle", "egret", "falcon", "ferret", "finch", "fox", "gecko", "gibbon", "goose",
        "hare", "hawk", "heron", "ibex", "ibis", "impala", "jackal", "jaguar", "koala", "lemur",
        "lion", "llama", "lynx", "magpie", "marmot", "marten", "mink", "moose", "newt", "ocelot",
        "orca", "osprey", "otter", "owl", "panda", "panther", "parrot", "pelican", "puffin",
        "quail", "raven", "robin", "salmon", "seal", "shrike", "sparrow", "stoat", "swan", "tapir",
        "tiger", "toucan", "trout", "walrus", "weasel", "wren",
    ],
];

/// A ref of three words, chosen by `bytes`: each word by one of them.
pub fn ref_from_bytes(bytes: [u8; 3]) -> String {
    let pick = |list: usize| REF_WORDS[list][usize::from(bytes[list]) % REF_WORDS[list].len()];
    format!("{}-{}-{}", pick(0), pick(1), pick(2))
}

/// API key names are only shown, never used as paths: anything printable up
/// to 100 characters, such as "laptop Claude Code".
pub fn validate_key_name(name: &str) -> Result<(), String> {
    let ok = !name.trim().is_empty()
        && name.trim() == name
        && name.chars().count() <= 100
        && !name.chars().any(char::is_control);
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{name}` isn't a valid key name: use up to 100 printable characters, not starting or ending with a space"
        ))
    }
}

/// A GitHub repo as `owner/name`. A trailing `.git` is dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubRepo {
    pub owner: String,
    pub name: String,
}

impl GithubRepo {
    pub fn parse(s: &str) -> Result<Self, String> {
        let bad = || format!("`{s}` isn't a GitHub repo; write it as owner/name");
        let (owner, name) = s.trim().split_once('/').ok_or_else(bad)?;
        let name = name.strip_suffix(".git").unwrap_or(name);
        let owner_ok = !owner.is_empty()
            && owner.len() <= 39
            && !owner.starts_with('-')
            && owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        // GitHub's rule for repo names, which unlike project names may start
        // with a dot, as in `owner/.github`.
        let name_ok = !name.is_empty()
            && name.len() <= 100
            && name != "."
            && name != ".."
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !owner_ok || !name_ok {
            return Err(bad());
        }
        Ok(GithubRepo {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }

    /// Whether a git remote URL points at this repo on github.com, over
    /// HTTPS or SSH, with or without `.git`. GitHub names are
    /// case-insensitive.
    pub fn matches_remote(&self, url: &str) -> bool {
        let url = url.trim().trim_end_matches('/');
        let url = url.strip_suffix(".git").unwrap_or(url);
        let path = [
            "https://github.com/",
            "git@github.com:",
            "ssh://git@github.com/",
        ]
        .iter()
        .find_map(|prefix| url.strip_prefix(prefix));
        path.is_some_and(|p| p.eq_ignore_ascii_case(&self.to_string()))
    }
}

impl fmt::Display for GithubRepo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_alert_cant_reach_a_terminal_as_anything_but_text() {
        assert_eq!(clean_alert("plain"), "plain");
        assert_eq!(clean_alert("a\x1b[2Jb\x07c\u{9b}d\ne"), "a[2Jbcd e");
        assert_eq!(clean_alert("a\u{202e}b\u{2066}c"), "abc");
        let long = "é".repeat(MAX_ALERT_CHARS * 2);
        let cut = clean_alert(&long);
        assert_eq!(cut.chars().count(), MAX_ALERT_CHARS + 1);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn refs() {
        assert_eq!(validate_ref("quiet-amber-otter"), Ok(()));
        assert_eq!(validate_ref(&"a".repeat(MAX_REF_LEN)), Ok(()));
        for bad in ["", "-x", "a/b", "a b", "vibed", "..", ".hidden"] {
            assert!(validate_ref(bad).is_err(), "{bad:?}");
        }
        // An agent may already be called `mac` or `tunnel`, but no new one.
        for reserved in ["mac", "tunnel", "Mac", "TUNNEL", "Vibed"] {
            assert_eq!(validate_ref(reserved), Ok(()), "{reserved}");
            assert_eq!(
                validate_new_ref(reserved).unwrap_err(),
                format!("`{reserved}` is a name VibeOS keeps for itself; pick another name")
            );
            assert!(is_reserved_ref(reserved));
        }
        assert_eq!(
            validate_ref("vibed").unwrap_err(),
            "`vibed` is a name VibeOS keeps for itself; pick another name"
        );
        assert!(validate_new_ref("vibed").is_err());
        assert!(
            validate_new_ref("a b")
                .unwrap_err()
                .contains("isn't a valid ref")
        );
        for near in ["macs", "mac-mini", "tunnels", "vibed2", "quiet-amber-otter"] {
            assert_eq!(validate_new_ref(near), Ok(()), "{near}");
            assert!(!is_reserved_ref(near));
        }
        let long = validate_ref(&"a".repeat(MAX_REF_LEN + 1)).unwrap_err();
        assert!(long.contains("up to 32"), "{long}");
        assert_eq!(SessionState::Exited.as_str(), "exited");
    }

    #[test]
    fn three_word_refs() {
        assert_eq!(ref_from_bytes([0, 0, 0]), "able-amber-badger");
        // Every byte picks a word, so every ref is one of 64³.
        let mut seen = std::collections::HashSet::new();
        for list in &REF_WORDS {
            let unique: std::collections::HashSet<_> = list.iter().collect();
            assert_eq!(unique.len(), list.len(), "a word appears twice");
            for w in list {
                assert!(w.chars().all(|c| c.is_ascii_lowercase()), "{w}");
            }
        }
        // Each byte picks its word on its own.
        for position in 0..3 {
            for b in 0..=255u8 {
                let mut bytes = [9, 9, 9];
                bytes[position] = b;
                let r = ref_from_bytes(bytes);
                assert_eq!(validate_ref(&r), Ok(()), "{r}");
                seen.insert(r);
            }
        }
        assert_eq!(seen.len(), 3 * 64 - 2);
        let longest = REF_WORDS
            .iter()
            .map(|l| l.iter().map(|w| w.len()).max().unwrap())
            .sum::<usize>()
            + 2;
        assert!(longest <= MAX_REF_LEN, "{longest}");
    }

    #[test]
    fn names_round_trip() {
        for e in Reasoning::ALL {
            assert_eq!(e.as_str().parse::<Reasoning>(), Ok(*e));
            assert_eq!(serde_json::to_value(e).unwrap(), e.as_str());
        }
        assert_eq!(EventKind::AgentState.to_string(), "agent.state");
        assert!("Linux".parse::<Environment>().is_err());
        assert!(serde_json::from_str::<Mode>("\"plan\"").is_err());
    }

    #[test]
    fn host_names_and_destinations() {
        for good in ["vps", "build-1", "a.b_c"] {
            assert_eq!(validate_host_name(good), Ok(()), "{good}");
        }
        for bad in ["", "local", "-x", "a:b", "a b", &"x".repeat(33)] {
            assert!(validate_host_name(bad).is_err(), "{bad}");
        }
        for good in [
            "vps",
            "me@vps.example.com",
            "ssh://me@vps:2222",
            "me@[::1]",
            "box.tail1234.ts.net",
        ] {
            assert_eq!(validate_ssh_destination(good), Ok(()), "{good}");
        }
        for bad in ["", "-oProxyCommand=x", "a b", "a;b", "a'b", "$(x)", "a\nb"] {
            assert!(validate_ssh_destination(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn starters_per_host() {
        let mac = starter_profiles("macos");
        assert_eq!(
            mac.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["software-eng-linux", "software-eng-mac"]
        );
        assert_eq!(mac[1].environment, Environment::Macos);
        for p in &mac {
            assert_eq!(
                (p.model, p.reasoning, p.mode, p.memory),
                (Model::Opus, Reasoning::High, Mode::Auto, 4 * GIB)
            );
            assert_eq!(p.validate("macos"), Ok(()));
        }
        let linux = starter_profiles("linux");
        assert_eq!(linux.len(), 1);
        assert_eq!(linux[0].environment, Environment::Linux);
    }

    #[test]
    fn profiles_from_before_pi_are_claudes() {
        let old = r#"{"name":"p","environment":"linux","model":"opus","reasoning":"high","mode":"auto","memory":4294967296}"#;
        let p: Profile = serde_json::from_str(old).unwrap();
        assert_eq!(p.agent, AgentKind::Claude);
        assert_eq!(p.pi_model, None);
        // And they say what they always said, so nothing older misreads them.
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            serde_json::from_str::<serde_json::Value>(old).unwrap()
        );
    }

    #[test]
    fn pi_profiles() {
        let pi = |environment, mode, model: Option<&str>| Profile {
            agent: AgentKind::Pi,
            mode,
            pi_model: model.map(str::to_owned),
            ..Profile::with_defaults("pi-linux", environment)
        };
        let on_linux = pi(Environment::Linux, Mode::Bypass, Some("openai/gpt-5"));
        assert_eq!(on_linux.validate("macos"), Ok(()));
        assert_eq!(on_linux.validate("linux"), Ok(()));
        let wire = serde_json::to_value(&on_linux).unwrap();
        assert_eq!(wire["agent"], "pi");
        assert_eq!(wire["pi_model"], "openai/gpt-5");
        // No model is fine: VibeOS doesn't pick one.
        assert_eq!(
            pi(Environment::Linux, Mode::Bypass, None).validate("linux"),
            Ok(())
        );
        // On the Mac it takes the owner's opt-in.
        let refused = pi(Environment::Macos, Mode::Auto, None)
            .validate("macos")
            .unwrap_err();
        assert!(refused.contains("never asks"), "{refused}");
        assert_eq!(
            pi(Environment::Macos, Mode::Bypass, None).validate("macos"),
            Ok(())
        );
        assert!(
            pi(Environment::Linux, Mode::Auto, None)
                .validate("linux")
                .is_err()
        );
        // A Pi model on a Claude profile means nothing, so it's refused.
        let claude = Profile {
            pi_model: Some("openai/gpt-5".into()),
            ..Profile::with_defaults("c", Environment::Linux)
        };
        assert!(claude.validate("linux").unwrap_err().contains("Pi model"));
    }

    #[test]
    fn the_mode_stays_true_to_the_agent() {
        use AgentKind::{Claude, Pi};
        use Environment::{Linux, Macos};
        let settled = |agent, environment, mode, was, asked| {
            let mut p = Profile {
                agent,
                mode,
                ..Profile::with_defaults("p", environment)
            };
            p.settle_mode(was, asked);
            p.mode
        };
        // A new Pi profile in linux needs no mode.
        assert_eq!(settled(Pi, Linux, Mode::Auto, None, None), Mode::Bypass);
        // A bypass left from linux isn't the owner's opt-in for the Mac.
        assert_eq!(
            settled(Pi, Macos, Mode::Bypass, Some((Pi, Linux)), None),
            Mode::Auto
        );
        assert_eq!(
            settled(
                Pi,
                Macos,
                Mode::Bypass,
                Some((Pi, Linux)),
                Some(Mode::Bypass)
            ),
            Mode::Bypass
        );
        assert_eq!(
            settled(Pi, Macos, Mode::Auto, Some((Claude, Macos)), None),
            Mode::Auto
        );
        // Once opted in it stays, through an edit of something else.
        assert_eq!(
            settled(Pi, Macos, Mode::Bypass, Some((Pi, Macos)), None),
            Mode::Bypass
        );
        // Pi's bypass doesn't follow it back to Claude.
        assert_eq!(
            settled(Claude, Linux, Mode::Bypass, Some((Pi, Linux)), None),
            Mode::Auto
        );
        assert_eq!(
            settled(
                Claude,
                Linux,
                Mode::Bypass,
                Some((Pi, Linux)),
                Some(Mode::Bypass)
            ),
            Mode::Bypass
        );
        // Claude's own bypass is left as it is.
        assert_eq!(
            settled(Claude, Macos, Mode::Bypass, Some((Claude, Macos)), None),
            Mode::Bypass
        );
    }

    /// The list Swift's `isValidPiModel` is held to as well.
    #[test]
    fn pi_models() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../../../protocol/pi-models.json")).unwrap();
        let list = |name: &str| -> Vec<String> {
            let list = cases[name].as_array().unwrap();
            assert!(!list.is_empty(), "{name}");
            list.iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect()
        };
        for good in list("accepted") {
            assert_eq!(validate_pi_model(&good), Ok(()), "{good:?}");
        }
        for bad in list("refused") {
            assert!(validate_pi_model(&bad).is_err(), "{bad:?}");
        }
        // The cap, in bytes.
        let longest = "x".repeat(MAX_PI_MODEL_BYTES);
        assert_eq!(validate_pi_model(&longest), Ok(()));
        assert!(validate_pi_model(&format!("{longest}x")).is_err());
        assert!(validate_pi_model(&format!("{}é", &longest[1..])).is_err());
    }

    #[test]
    fn a_refused_model_is_said_without_its_control_characters() {
        let said = validate_pi_model("a\u{1b}[2Jb").unwrap_err();
        assert!(said.contains("a[2Jb"), "{said}");
        assert!(!said.contains('\u{1b}'), "{said}");
    }

    #[test]
    fn macos_profiles_need_a_mac() {
        let p = Profile::with_defaults("m", Environment::Macos);
        assert!(p.validate("linux").unwrap_err().contains("only a Mac"));
    }

    #[test]
    fn memory_bounds() {
        assert_eq!(validate_memory(GIB), Ok(()));
        assert_eq!(validate_memory(TIB), Ok(()));
        assert_eq!(validate_memory(1536 * MIB), Ok(()));
        assert!(validate_memory(GIB - MIB).is_err());
        assert!(validate_memory(TIB + MIB).is_err());
        assert!(validate_memory(GIB + 1).is_err());
    }

    #[test]
    fn names() {
        for ok in ["a", "web", "Vibe.OS_2-x", &"a".repeat(100)] {
            assert_eq!(validate_name("project", ok), Ok(()), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "-x",
            "a/b",
            "a b",
            "é",
            &"a".repeat(101),
        ] {
            assert!(validate_name("project", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn key_names() {
        for ok in [
            "laptop",
            "laptop Claude Code",
            "CI (staging) ✓",
            &"k".repeat(100),
        ] {
            assert_eq!(validate_key_name(ok), Ok(()), "{ok}");
        }
        for bad in [
            "",
            " ",
            " laptop",
            "laptop ",
            "a\nb",
            "a\tb",
            &"k".repeat(101),
        ] {
            assert!(validate_key_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn github_repos() {
        let r = GithubRepo::parse("tsconfigdotjson/vibe-os-native.git").unwrap();
        assert_eq!(r.to_string(), "tsconfigdotjson/vibe-os-native");
        assert_eq!(GithubRepo::parse("me/.github").unwrap().name, ".github");
        for bad in [
            "", "x", "/x", "x/", "a/b/c", "a b/c", "-a/b", "a/.git", "a/..", "a/.", "a/b c",
        ] {
            assert!(GithubRepo::parse(bad).is_err(), "{bad}");
        }
        for url in [
            "https://github.com/Me/Web.git",
            "https://github.com/me/web",
            "git@github.com:me/web.git",
            "ssh://git@github.com/me/web/",
        ] {
            assert!(
                GithubRepo::parse("me/web").unwrap().matches_remote(url),
                "{url}"
            );
        }
        for url in [
            "https://github.com/me/web2",
            "https://gitlab.com/me/web",
            "/tmp/web",
        ] {
            assert!(
                !GithubRepo::parse("me/web").unwrap().matches_remote(url),
                "{url}"
            );
        }
    }
}
