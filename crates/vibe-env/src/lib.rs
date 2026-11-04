//! Environment backends: `Compartment`, `Limiter`, `Sleeper` and
//! `Capabilities`, plus the conformance suite every backend passes (PRD:
//! Architecture, Environment backends).
//!
//! Every way of running an agent sits behind these three small traits, so
//! policy (admission, sleep, MCP, the menu) never asks which backend it's
//! talking to: it reads the traits and [`Capabilities`], nothing else. A
//! backend is all three plus its capabilities, a [`Backend`].
//!
//! What's here:
//!
//! - the traits and the types they pass;
//! - [`tree`], a process tree by session ID and the one way to freeze it,
//!   which the `macos` sleeper and governor and the test backend use;
//! - [`claude`], Claude Code's state in a compartment: where its
//!   transcripts go, the pre-fill that opens it straight to its prompt, and
//!   forgetting a worktree;
//! - [`pi`], the same for Pi, and how it's started: its args, the allowlist
//!   its environment is built from, and the one login every agent shares;
//!   [`pi_release`] downloads it, checksummed;
//! - [`native`], what backends whose agents run natively share;
//! - [`macos`], the `macos` backend: native agents under a resource
//!   governor, whose decisions are [`governor`]'s;
//! - [`cgroups`], the `linux` backend on a Linux host: native agents, each
//!   in a cgroup v2 of its own under the owner's systemd user manager;
//! - [`testing`], a backend for tests, so the core doesn't wait on the real
//!   ones;
//! - [`conformance`], the suite every backend must pass;
//! - the `linux` backend's plumbing on the Mac: [`vm`] drives `vibe-vm`,
//!   the helper that runs each agent's VM, and [`guest`] drives
//!   `vibe-guest`, PID 1 inside it; [`images`] keeps the disks they boot,
//!   and [`toolbox`] makes new template and Linux home versions.

pub mod cgroups;
pub mod claude;
pub mod conformance;
pub mod ext4;
pub mod governor;
pub mod guest;
pub mod images;
pub mod linux;
pub mod macos;
pub mod native;
pub mod pi;
pub mod pi_logins;
pub mod pi_release;
pub mod sparse;
pub mod testing;
pub mod toolbox;
pub mod tree;
pub mod vm;

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use vibe_core::limits::HostLimits;
use vibe_core::model::AgentKind;

pub use claude::ClaudeHome;
pub use pi::PiHome;

/// Why a backend couldn't do something, in a sentence for the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvError(String);

impl EnvError {
    pub fn new(message: impl Into<String>) -> Self {
        EnvError(message.into())
    }

    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EnvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EnvError {}

impl From<io::Error> for EnvError {
    fn from(e: io::Error) -> Self {
        EnvError(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, EnvError>;

/// What a backend can and can't do. Policy decides from this alone: for
/// example, memory pressure ends frozen agents only where
/// `sleep_frees_memory` is false, as sleeping didn't give their memory
/// back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Sleeping gives the agent's memory back to the host.
    pub sleep_frees_memory: bool,
    /// An agent asleep at a reboot wakes exactly where it was afterwards;
    /// otherwise it resumes with `claude --resume`.
    pub exact_after_reboot: bool,
    /// The kernel enforces the limits, rather than a supervisor.
    pub kernel_limits: bool,
    /// The agent can't touch files outside what's shared with it.
    pub file_containment: bool,
    /// About how long a wake takes.
    pub wake_cost: Duration,
}

/// One agent's limits: its profile's memory and the host limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// The high-water mark, in bytes.
    pub memory: u64,
    pub cpu_cores: u32,
    pub max_processes: u32,
    /// Bytes in the worktree and temp dir: past this, a warning.
    pub disk_soft: u64,
    /// Past this, the agent is paused.
    pub disk_hard: u64,
}

impl Limits {
    pub fn new(memory: u64, host: &HostLimits) -> Limits {
        Limits {
            memory,
            cpu_cores: host.cpu_cores,
            max_processes: host.max_processes,
            disk_soft: host.disk_soft,
            disk_hard: host.disk_hard,
        }
    }
}

/// Which limit an agent hit. Named as `agent.limit` events name them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Limit {
    Memory,
    Processes,
    Disk,
    Cpu,
}

impl Limit {
    pub fn as_str(self) -> &'static str {
        match self {
            Limit::Memory => "memory",
            Limit::Processes => "processes",
            Limit::Disk => "disk",
            Limit::Cpu => "cpu",
        }
    }
}

/// Everything a backend needs to make an agent's compartment. The worktree
/// and the agent's directory already exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSpec {
    pub agent_ref: String,
    pub project: String,
    /// The worktree, at the same absolute path in every environment.
    pub worktree: PathBuf,
    /// The project's git directory, which the worktree's `.git` file
    /// points into, and which a VM mounts read-write at the same path.
    pub git_dir: PathBuf,
    /// `~/VibeOS/agents/<ref>`, the agent's own directory on the host.
    pub agent_dir: PathBuf,
    /// The limits it starts with; a VM's are fixed at boot.
    pub limits: Limits,
    /// Which coding agent runs in it, for life.
    pub agent: AgentKind,
}

/// Where the agent's coding agent keeps its state. It's flattened into
/// [`Handle`] under the kind's name, so a handle saved before Pi, with its
/// `claude` key, reads as it always did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentHome {
    Claude(ClaudeHome),
    Pi(PiHome),
}

impl AgentHome {
    pub fn kind(&self) -> AgentKind {
        match self {
            AgentHome::Claude(_) => AgentKind::Claude,
            AgentHome::Pi(_) => AgentKind::Pi,
        }
    }

    /// The directory of the transcripts of sessions run in `worktree`.
    pub fn transcripts_dir(&self, worktree: &std::path::Path) -> PathBuf {
        match self {
            AgentHome::Claude(c) => c.project_dir(worktree),
            AgentHome::Pi(p) => p.sessions.clone(),
        }
    }

    /// Whether session `session_id`, run in `worktree`, has a transcript to
    /// resume from.
    pub fn has_transcript(&self, worktree: &std::path::Path, session_id: &str) -> bool {
        match self {
            AgentHome::Claude(c) => c.transcript(worktree, session_id).is_file(),
            AgentHome::Pi(p) => p.transcript(session_id).is_some(),
        }
    }

    /// Pre-fills its state so it opens straight to its prompt in `worktree`.
    pub fn prefill(&self, worktree: &std::path::Path, bypass: bool) -> io::Result<()> {
        match self {
            AgentHome::Claude(c) => c.prefill(worktree, bypass),
            AgentHome::Pi(p) => p.prefill(),
        }
    }

    /// Forgets `worktree`, and what's the agent's alone.
    pub fn forget(&self, worktree: &std::path::Path) -> io::Result<()> {
        match self {
            AgentHome::Claude(c) => c.forget(worktree),
            AgentHome::Pi(p) => p.forget(),
        }
    }
}

/// An agent's compartment, as its backend made it. It's plain data, kept in
/// the registry, so a restarted `vibed` has it again; a backend keeps
/// anything else it needs in the agent's directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handle {
    pub agent_ref: String,
    pub agent_dir: PathBuf,
    pub worktree: PathBuf,
    /// Where its coding agent keeps its state.
    #[serde(flatten)]
    pub home: AgentHome,
    /// The session the agent's terminal leads: its shim's pid, which is the
    /// session ID of everything a native agent starts. Set once a shim
    /// holds the terminal; it stays after the shim is gone, so stragglers
    /// can still be found. Its pid may be reused once the whole tree has
    /// gone, so a backend acts on it only while
    /// [`tree::SessionTree::is_shims`] says it's still the shim's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<u32>,
}

impl Handle {
    /// Its Claude home, for an agent that runs Claude.
    pub fn claude(&self) -> Option<&ClaudeHome> {
        match &self.home {
            AgentHome::Claude(c) => Some(c),
            AgentHome::Pi(_) => None,
        }
    }
}

/// How to start the agent's coding agent in a compartment. The backend knows
/// which `claude` or `pi` to run there; this is what to run it with.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Launch {
    /// Which to run.
    pub agent: AgentKind,
    /// Its arguments, the prompt included.
    pub args: Vec<String>,
    /// Set on top of the compartment's own environment. For Pi, on top of
    /// only what [`pi::allowed_env`] keeps of it; `VIBE_SERVICES` here also
    /// has its backend add the `-e` for the services' extension
    /// ([`pi::service_args`]), as only the backend knows where that is.
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

/// A command to run to completion in a compartment, such as
/// `.vibe/setup.sh`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    /// Past this it's killed, with everything it started.
    pub timeout: Duration,
}

/// How a [`Run`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    /// Its exit code, if it exited.
    pub code: Option<i32>,
    /// The signal that ended it, if one did.
    pub signal: Option<i32>,
    /// Whether it ran out of time and was killed.
    pub timed_out: bool,
    /// What it wrote to stdout and stderr, interleaved.
    pub output: Vec<u8>,
}

impl RunOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// The agent's end of its terminal, for a shim to hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEnd {
    /// The shim runs this on a PTY of its own, leading its session.
    Pty {
        command: Vec<String>,
        cwd: PathBuf,
        env: Vec<(String, String)>,
        /// Start from an empty environment rather than `vibed`'s, so `env`
        /// is everything it gets: Pi's way ([`pi::allowed_env`]).
        clean_env: bool,
    },
    /// The command already runs in a VM: the shim attaches to
    /// `vibe-guest`'s terminal through the `vibe-vm` serving this socket.
    Vm { socket: PathBuf },
}

/// One of an agent's processes, for the idle rule and the menu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    /// The kernel's short name for it.
    pub name: String,
    /// Its command line, where the OS shows it.
    pub args: Vec<String>,
    /// Stopped, by a freeze or a pause.
    pub stopped: bool,
    /// When it started, in milliseconds since the epoch.
    pub started_ms: u64,
    /// CPU time used so far, user and system.
    pub cpu_ms: u64,
    /// Its memory, in bytes: resident on Linux, the physical footprint on
    /// macOS.
    pub memory: u64,
}

/// What an agent is using now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub memory: u64,
    pub cpu_ms: u64,
    pub processes: u32,
    /// The limit it hit, while that has it paused.
    pub paused: Option<Limit>,
    /// The soft limits it's past, which it's warned about: memory at 80%,
    /// `disk_soft`. Only a governor that measures them says.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Limit>,
    /// Bytes its worktree and temp dir took when last measured, where the
    /// backend measures them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<u64>,
}

/// How a wake went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Woke {
    /// Exactly where it was, on the terminal it had.
    Exact,
    /// Exactly where it was, but its terminal needs a new shim: a VM's
    /// stream dropped when it went to sleep.
    Reattach(TerminalEnd),
    /// Its processes are gone, as after a reboot: launch Claude again with
    /// `--resume`.
    Resume,
}

/// Lifecycle, for every backend.
pub trait Compartment {
    /// Makes the compartment: a VM's disks and configuration, a cgroup, or
    /// the directories a native agent needs. Nothing runs yet.
    fn create(&self, spec: &AgentSpec) -> Result<Handle>;

    /// Runs a command to completion inside the compartment.
    fn run(&self, h: &Handle, run: &Run) -> Result<RunOutput>;

    /// What Pi itself says of `model` for this agent, given the environment it
    /// would start with, where Pi can be run before the agent is: not in a VM,
    /// whose Pi only the guest can run. None there, or if Pi doesn't say.
    fn pi_auth_check(
        &self,
        _h: &Handle,
        _env: &[(String, String)],
        _model: &str,
    ) -> Option<pi::AuthCheck> {
        None
    }

    /// Starts its coding agent, and returns the end of its terminal for the shim:
    /// a command for the shim's PTY, or a VM's stream.
    fn launch(&self, h: &Handle, launch: &Launch) -> Result<TerminalEnd>;

    /// Every process of the agent but its shim, for the idle rule.
    fn processes(&self, h: &Handle) -> Result<Vec<Proc>>;

    /// The program the agent runs the shared services' shim with, if this
    /// compartment is a machine of its own, whose `vibe` isn't the host's.
    fn services_program(&self) -> Option<String> {
        None
    }

    /// Why an agent starting now can't have the shared services here, if it
    /// can't, for a compartment whose machine may be too old to serve them. It's
    /// asked at a launch, so a VM may be booted by it.
    fn services_unsupported(&self, _h: &Handle) -> Result<Option<String>> {
        Ok(None)
    }

    /// Dials the shared services' relay into a compartment that's a machine of
    /// its own, while it runs: the host's end of a connection the guest's hub
    /// takes. `None` if it isn't running, or its guest doesn't serve them.
    fn services_relay(&self, _h: &Handle) -> Result<Option<std::os::unix::net::UnixStream>> {
        Ok(None)
    }

    /// Ends every process of the agent, its shim's too. The compartment
    /// stays; a later launch starts it again.
    fn stop(&self, h: &Handle) -> Result<()>;

    /// Stops the agent and removes everything the backend made for it,
    /// its coding agent's state included. Leaves nothing behind.
    fn delete(&self, h: &Handle) -> Result<()>;
}

/// Limits: a governor, cgroups, or fixed at a VM's boot.
pub trait Limiter {
    /// Applies `limits` to the running agent. Hitting one pauses it.
    fn apply(&self, h: &Handle, limits: &Limits) -> Result<()>;

    fn usage(&self, h: &Handle) -> Result<Usage>;
}

/// What a sleeping agent keeps on disk: a snapshot, on a backend whose
/// sleep saves the agent there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Saved {
    pub bytes: u64,
    /// When it was saved.
    pub at: std::time::SystemTime,
}

/// Sleep: `SIGSTOP`, the cgroup freezer, or a snapshot.
pub trait Sleeper {
    fn sleep(&self, h: &Handle) -> Result<()>;

    /// Wakes a sleeping or paused agent.
    fn wake(&self, h: &Handle) -> Result<Woke>;

    /// About how much disk a sleep now would write, on a backend whose
    /// sleep saves the agent to disk; none on one whose sleep writes
    /// nothing. All snapshots share the `snapshot_disk` budget.
    fn sleep_size(&self, _h: &Handle) -> Result<Option<u64>> {
        Ok(None)
    }

    /// What the agent's sleep saved, while it's asleep that way.
    fn saved(&self, _h: &Handle) -> Option<Saved> {
        None
    }

    /// Drops what the agent's sleep saved, to make room for another's.
    /// Its processes go with it: its next wake is [`Woke::Resume`].
    fn drop_saved(&self, _h: &Handle) -> Result<()> {
        Ok(())
    }
}

/// A way of running agents: the three traits and what it can do.
pub trait Backend: Compartment + Limiter + Sleeper + Send + Sync + fmt::Debug {
    fn capabilities(&self) -> Capabilities;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_saved_before_pi_still_read() {
        let old = serde_json::json!({
            "agent_ref": "q",
            "agent_dir": "/a/q",
            "worktree": "/w/q",
            "claude": {"dir": "/h/.claude", "state": "/h/.claude.json", "own": false},
            "session": 7,
        });
        let h: Handle = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(h.home.kind(), AgentKind::Claude);
        assert_eq!(h.session, Some(7));
        assert_eq!(h.claude().unwrap().dir, std::path::Path::new("/h/.claude"));
        // And what's written reads the same to an older vibed.
        assert_eq!(serde_json::to_value(&h).unwrap(), old);
    }

    #[test]
    fn a_pi_handle_round_trips_under_its_own_key() {
        let h = Handle {
            agent_ref: "q".into(),
            agent_dir: "/a/q".into(),
            worktree: "/w/q".into(),
            home: AgentHome::Pi(PiHome::at("/h/agent".into(), "/a/q/pi-sessions".into())),
            session: None,
        };
        let v = serde_json::to_value(&h).unwrap();
        assert_eq!(v["pi"]["sessions"], "/a/q/pi-sessions");
        assert!(v.get("claude").is_none());
        assert_eq!(serde_json::from_value::<Handle>(v).unwrap(), h);
        assert_eq!(h.claude(), None);
    }
}
