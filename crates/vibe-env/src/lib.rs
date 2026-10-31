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
//! - [`native`], what backends whose agents run natively share;
//! - [`macos`], the `macos` backend: native agents under a resource
//!   governor, whose decisions are [`governor`]'s;
//! - [`testing`], a backend for tests, so the core doesn't wait on the real
//!   ones (#9, #17);
//! - [`conformance`], the suite every backend must pass;
//! - the `linux` backend's plumbing on the Mac: [`vm`] drives `vibe-vm`,
//!   the helper that runs each agent's VM, and [`guest`] drives
//!   `vibe-guest`, PID 1 inside it; [`images`] keeps the disks they boot,
//!   and [`toolbox`] makes new template and Linux home versions.

pub mod claude;
pub mod conformance;
pub mod ext4;
pub mod governor;
pub mod guest;
pub mod images;
pub mod linux;
pub mod macos;
pub mod native;
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

pub use claude::ClaudeHome;

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
/// example, admission sleeps an agent early to free memory only when
/// `sleep_frees_memory` is true.
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
}

/// An agent's compartment, as its backend made it. It's plain data, kept in
/// the registry, so a restarted `vibed` has it again; a backend keeps
/// anything else it needs in the agent's directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handle {
    pub agent_ref: String,
    pub agent_dir: PathBuf,
    pub worktree: PathBuf,
    /// Where Claude keeps its state for this agent.
    pub claude: ClaudeHome,
    /// The session the agent's terminal leads: its shim's pid, which is the
    /// session ID of everything a native agent starts. Set once a shim
    /// holds the terminal; it stays after the shim is gone, so stragglers
    /// can still be found. Its pid may be reused once the whole tree has
    /// gone, so a backend acts on it only while
    /// [`tree::SessionTree::is_shims`] says it's still the shim's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<u32>,
}

/// How to start Claude Code in a compartment. The backend knows which
/// `claude` to run there; this is what to run it with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// Claude's arguments, the prompt included.
    pub args: Vec<String>,
    /// Set on top of the compartment's own environment.
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

    /// Starts Claude, and returns the end of its terminal for the shim:
    /// a command for the shim's PTY, or a VM's stream.
    fn launch(&self, h: &Handle, launch: &Launch) -> Result<TerminalEnd>;

    /// Every process of the agent but its shim, for the idle rule.
    fn processes(&self, h: &Handle) -> Result<Vec<Proc>>;

    /// Ends every process of the agent, its shim's too. The compartment
    /// stays; a later launch starts it again.
    fn stop(&self, h: &Handle) -> Result<()>;

    /// Stops the agent and removes everything the backend made for it,
    /// Claude's state included. Leaves nothing behind.
    fn delete(&self, h: &Handle) -> Result<()>;
}

/// Limits: a governor, cgroups, or fixed at a VM's boot.
pub trait Limiter {
    /// Applies `limits` to the running agent. Hitting one pauses it.
    fn apply(&self, h: &Handle, limits: &Limits) -> Result<()>;

    fn usage(&self, h: &Handle) -> Result<Usage>;
}

/// Sleep: `SIGSTOP`, the cgroup freezer, or a snapshot.
pub trait Sleeper {
    fn sleep(&self, h: &Handle) -> Result<()>;

    /// Wakes a sleeping or paused agent.
    fn wake(&self, h: &Handle) -> Result<Woke>;
}

/// A way of running agents: the three traits and what it can do.
pub trait Backend: Compartment + Limiter + Sleeper + Send + Sync + fmt::Debug {
    fn capabilities(&self) -> Capabilities;
}
