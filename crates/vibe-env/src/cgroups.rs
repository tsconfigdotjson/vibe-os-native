//! The `linux` backend on a Linux host (PRD: How agents run on a Linux
//! host): native agents, each in a cgroup v2 of its own, made through the
//! owner's systemd user manager, so no root is needed.
//!
//! - **Compartment.** An agent's processes run in a transient scope,
//!   `vibeos-<ref>.scope` in `vibeos.slice`, under the owner's
//!   `user@<uid>.service`. The shim's PTY runs `systemd-run --user --scope …
//!   -- claude …`, which asks systemd for the scope, moves itself into it and
//!   becomes Claude, so Claude keeps its place as the PTY's foreground
//!   process and everything it starts is in the scope from its first
//!   instruction. The shim itself isn't in it: it must keep answering while
//!   the agent is frozen. A `run` (`.vibe/setup.sh`) gets a scope of its own
//!   with the same limits, and whatever it leaves in it is ended with it.
//! - **Membership** is the cgroup itself: no process scanning, and a
//!   `nohup … &` or `setsid` job is still the agent's. A process the agent
//!   moves into another of the owner's cgroups on purpose (the owner may
//!   write them all), or a unit it starts with `systemd-run --user`,
//!   escapes, and that's accepted: agents are trusted, and the limits
//!   protect the server's resources, not its files.
//! - **Limits** are the kernel's, set as systemd properties so they hold
//!   across a `systemctl --user daemon-reload`: `MemoryMax` from the
//!   profile, with `MemoryHigh` at [`PAUSE_PERCENT`] of it, where the kernel
//!   throttles the agent so its watcher can pause it before the OOM killer
//!   picks anything, and `MemorySwapMax` as much again, so an agent's memory
//!   in RAM plus swap is bounded even while `vibed` is down; `TasksMax` from
//!   `max_processes`, at [`TASKS_PER_PROCESS`] tasks each, as the kernel
//!   counts threads and a browser or a JVM has dozens; and `CPUQuota` from
//!   `cpu_cores`. The memory controller's OOM killer acts inside the scope
//!   only, and the scope carries on after it (`OOMPolicy=continue`), so a
//!   memory hog is stopped by its own limit and nobody else's.
//! - **A limit hit pauses it.** While `vibed` holds an agent to its limits
//!   ([`Limiter::apply`]), a watcher reads its cgroup every
//!   [`Intervals::watch`] and freezes it when it's over one: more processes
//!   than `max_processes`, or a fork or thread the kernel refused at
//!   `pids.max`; its memory in RAM and swap at [`PAUSE_PERCENT`] of the
//!   profile's, or an OOM in it. It also measures the worktree and temp dir every
//!   [`Intervals::disk`]: a warning past `disk_soft`, a pause past
//!   `disk_hard`. Memory at 80% is a warning. A pause holds until a wake,
//!   across restarts of `vibed` too (the kernel keeps the cgroup frozen and
//!   [`STATE_FILE`] says why).
//! - **Sleep** is the cgroup freezer (`cgroup.freeze`), which is exact, and
//!   wake thaws it. Frozen memory stays allocated and may swap. After a
//!   reboot the scope is gone, so a wake says `--resume`.
//! - **Logins** are the server's own: its `gh`, and its `claude` logged in
//!   with `/login`, in the owner's one `~/.claude`, as on the Mac's `macos`
//!   agents. Nothing is redirected.
//!
//! It needs cgroups v2 and a systemd user manager that's been given the
//! `memory` and `pids` controllers (and `cpu`, for the CPU cap), which is
//! systemd's default since version 252. [`Cgroups::check`] says what's
//! missing. The backend builds for every Unix, so `vibed` and its tests
//! build everywhere, but it runs agents on Linux only.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use vibe_core::limits::HostLimits;

use vibe_core::model::AgentKind;

use crate::tree::{self, SHIM_NAME, SessionTree};
use crate::{
    AgentHome, AgentSpec, Backend, Capabilities, ClaudeHome, Compartment, EnvError, Handle, Launch,
    Limit, Limiter, Limits, Proc, Result, Run, RunOutput, Sleeper, TerminalEnd, Usage, Woke,
    native,
};

/// Where an agent's mode (running, asleep or paused, and why) and the
/// limits it was last given are kept, in its directory.
pub const STATE_FILE: &str = "cgroup.json";

/// The slice every agent's scope is in, under the owner's user manager.
pub const SLICE: &str = "vibeos.slice";

/// Where cgroups v2 is mounted.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// The controllers the limits need; without the first two an agent doesn't
/// start.
const REQUIRED: [&str; 2] = ["memory", "pids"];

/// Tasks (processes and their threads) the kernel allows per process in
/// `max_processes`.
pub const TASKS_PER_PROCESS: u64 = 8;

/// The share of its memory where the kernel starts throttling an agent
/// (`MemoryHigh`), and its watcher pauses it.
pub const PAUSE_PERCENT: u64 = 95;

/// How long a stopping agent gets after `SIGTERM`.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// How long `systemctl --user` may take to answer.
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a launched agent's scope may take to appear, as the shim's
/// command asks systemd for it.
const APPEAR: Duration = Duration::from_secs(10);

/// How long systemd may take to remove an emptied scope.
const GONE: Duration = Duration::from_secs(10);

/// How long the freezer may take to say a cgroup is frozen or thawed.
const FREEZE_SETTLE: Duration = Duration::from_secs(5);

/// The systemd that first expands `$VARIABLES` in a scope's command line,
/// unless told not to: a prompt must reach Claude as it was written.
const EXPAND_ENVIRONMENT_FROM: u32 = 254;

/// The systemd that first gives scopes an `OOMPolicy`, whose default would
/// stop the whole agent once the OOM killer ended any of its processes.
const SCOPE_OOM_POLICY_FROM: u32 = 253;

/// How often the watcher does each of its jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Intervals {
    /// Reads the agent's cgroup and acts on what it says.
    pub watch: Duration,
    /// Measures the worktree and its temp dir.
    pub disk: Duration,
}

impl Default for Intervals {
    fn default() -> Intervals {
        Intervals {
            watch: Duration::from_millis(250),
            disk: Duration::from_secs(60),
        }
    }
}

/// The owner's systemd user manager, as [`Cgroups::check`] found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Systemd {
    /// Its major version.
    pub version: u32,
    /// Its cgroup, in the cgroup filesystem.
    pub manager: PathBuf,
    /// The controllers it was given.
    pub controllers: Vec<String>,
}

impl Systemd {
    /// From `systemctl --user show --property=Version --property=ControlGroup`.
    fn parse(show: &str, root: &Path) -> Result<Systemd> {
        let field = |name: &str| {
            show.lines()
                .find_map(|l| l.strip_prefix(name)?.strip_prefix('='))
                .map(str::trim)
                .unwrap_or("")
        };
        let version: u32 = field("Version")
            .trim_start_matches('v')
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| {
                EnvError::new(format!(
                    "can't tell which systemd this is from `systemctl --user show`: {show:?}"
                ))
            })?;
        let group = field("ControlGroup");
        if !group.starts_with('/') {
            return Err(EnvError::new(
                "your systemd user manager doesn't say which cgroup it has, so agents can't get \
                 cgroups of their own (systemd 247 or later is needed)",
            ));
        }
        let manager = root.join(group.trim_start_matches('/'));
        let controllers = fs::read_to_string(manager.join("cgroup.controllers"))
            .map_err(|e| {
                EnvError::new(format!(
                    "can't read {}: {e}",
                    manager.join("cgroup.controllers").display()
                ))
            })?
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        Ok(Systemd {
            version,
            manager,
            controllers,
        })
    }

    fn has(&self, controller: &str) -> bool {
        self.controllers.iter().any(|c| c == controller)
    }

    /// The controllers the limits need that it lacks.
    pub fn missing(&self) -> Vec<&'static str> {
        REQUIRED.into_iter().filter(|c| !self.has(c)).collect()
    }

    /// Whether `CPUQuota` works: without the `cpu` controller systemd
    /// accepts it and does nothing.
    pub fn cpu(&self) -> bool {
        self.has("cpu")
    }
}

/// One cgroup, by its directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cgroup {
    pub dir: PathBuf,
}

/// What a cgroup's files say, for the limits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counters {
    /// Processes now, in it and the cgroups under it.
    processes: u64,
    /// Forks and threads refused at `pids.max`.
    refused: u64,
    /// OOMs, and processes the OOM killer ended.
    ooms: u64,
    /// Memory in RAM that reclaim can't just drop, plus swap.
    memory: u64,
    cpu_ms: u64,
}

impl Cgroup {
    /// Whether it exists now.
    pub fn exists(&self) -> bool {
        self.dir.join("cgroup.procs").is_file()
    }

    /// Every process in it and the cgroups under it.
    pub fn pids(&self) -> Vec<u32> {
        let mut out = Vec::new();
        let mut stack = vec![self.dir.clone()];
        while let Some(d) = stack.pop() {
            if let Ok(text) = fs::read_to_string(d.join("cgroup.procs")) {
                out.extend(
                    text.split_whitespace()
                        .filter_map(|p| p.parse::<u32>().ok()),
                );
            }
            if let Ok(entries) = fs::read_dir(&d) {
                stack.extend(
                    entries
                        .filter_map(std::result::Result::ok)
                        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                        .map(|e| e.path()),
                );
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    fn read(&self, file: &str) -> Option<String> {
        fs::read_to_string(self.dir.join(file)).ok()
    }

    /// A file of one number, `max` being none.
    fn number(&self, file: &str) -> Option<u64> {
        self.read(file)?.trim().parse().ok()
    }

    /// A file of `key value` lines.
    fn keyed(&self, file: &str) -> HashMap<String, u64> {
        self.read(file).map(|t| keyed(&t)).unwrap_or_default()
    }

    fn counters(&self) -> Counters {
        let events = self.keyed("memory.events");
        let stat = self.keyed("memory.stat");
        let current = self.number("memory.current").unwrap_or(0);
        let swap = self.number("memory.swap.current").unwrap_or(0);
        Counters {
            processes: self.pids().len() as u64,
            refused: self.keyed("pids.events").get("max").copied().unwrap_or(0),
            ooms: events.get("oom").copied().unwrap_or(0)
                + events.get("oom_kill").copied().unwrap_or(0),
            memory: own_memory(current, &stat).saturating_add(swap),
            cpu_ms: self
                .keyed("cpu.stat")
                .get("usage_usec")
                .copied()
                .unwrap_or(0)
                / 1000,
        }
    }

    /// Whether the freezer has it frozen.
    pub fn frozen(&self) -> bool {
        self.keyed("cgroup.events").get("frozen") == Some(&1)
    }

    /// Freezes or thaws it, and waits until the kernel says it has.
    pub fn freeze(&self, on: bool) -> io::Result<()> {
        fs::write(self.dir.join("cgroup.freeze"), if on { "1" } else { "0" })?;
        let deadline = Instant::now() + FREEZE_SETTLE;
        loop {
            if !self.exists() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "its cgroup has gone",
                ));
            }
            if self.frozen() == on {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(format!(
                    "the cgroup didn't {} within {}s",
                    if on { "freeze" } else { "thaw" },
                    FREEZE_SETTLE.as_secs()
                )));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Kills everything in it: `cgroup.kill` where the kernel has it (5.14
    /// and later), `SIGKILL` to each process otherwise.
    pub fn kill(&self) {
        if fs::write(self.dir.join("cgroup.kill"), "1").is_err() {
            for pid in self.pids() {
                tree::signal(pid, libc::SIGKILL);
            }
        }
    }

    /// Waits up to `timeout` for it to have no processes.
    pub fn wait_empty(&self, timeout: Duration) -> bool {
        wait_until(timeout, || self.pids().is_empty())
    }

    /// Waits up to `timeout` for systemd to remove it.
    pub fn wait_gone(&self, timeout: Duration) -> bool {
        wait_until(timeout, || !self.dir.exists())
    }

    /// Ends everything in it: up to `grace` after `SIGTERM`, then killed.
    /// A frozen one is killed as it is, without a thaw that would let it
    /// run, and page back in, only to end. Returns whether it emptied.
    pub fn end(&self, grace: Duration) -> bool {
        if !self.exists() {
            return true;
        }
        if !grace.is_zero() && !self.frozen() {
            for pid in self.pids() {
                tree::signal(pid, libc::SIGTERM);
            }
            if self.wait_empty(grace) {
                return true;
            }
        }
        self.kill();
        self.wait_empty(Duration::from_secs(5))
    }
}

fn wait_until(timeout: Duration, mut ok: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `key value` lines.
fn keyed(text: &str) -> HashMap<String, u64> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(' ')?;
            Some((k.to_owned(), v.trim().parse().ok()?))
        })
        .collect()
}

/// The memory a cgroup holds that reclaim can't just drop: all it's
/// charged for but its file cache, though tmpfs files (`shmem`, which
/// `file` includes) stay.
fn own_memory(current: u64, stat: &HashMap<String, u64>) -> u64 {
    let file = stat.get("file").copied().unwrap_or(0);
    let shmem = stat.get("shmem").copied().unwrap_or(0);
    current.saturating_sub(file.saturating_sub(shmem))
}

/// Whether the agent runs, or why it's stopped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Mode {
    #[default]
    Running,
    Asleep,
    Paused {
        limit: Limit,
    },
}

/// What [`STATE_FILE`] keeps.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    mode: Mode,
    /// The limits it was last given, which a launch or a run starts with.
    #[serde(default)]
    limits: Option<Limits>,
}

/// What the backend knows of one agent, shared with its watcher.
#[derive(Debug)]
struct Agent {
    file: PathBuf,
    worktree: PathBuf,
    tmp: PathBuf,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    saved: Saved,
    /// Tells the watcher holding it to its limits to stop, while one does.
    watcher: Option<Arc<AtomicBool>>,
    warnings: Vec<Limit>,
    disk: Option<u64>,
    /// Where the watcher counts what the kernel refused from, set by a
    /// launch and a wake; it measures the disk afresh too.
    recount: Option<Recount>,
}

/// Where a watcher counts what the kernel refused an agent from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recount {
    /// From nothing: a launch's scope is new, so all it was refused counts,
    /// even before its watcher started.
    Zero,
    /// From what's been refused by now: a wake lets go of what paused it.
    Now,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Agent {
    fn new(h: &Handle) -> Agent {
        let file = h.agent_dir.join(STATE_FILE);
        let saved = fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Agent {
            file,
            worktree: h.worktree.clone(),
            tmp: h.agent_dir.join(vibe_core::paths::TMP_DIR),
            state: Mutex::new(State {
                saved,
                watcher: None,
                warnings: Vec::new(),
                disk: None,
                recount: None,
            }),
        }
    }

    fn save(&self, st: &State) {
        let r = serde_json::to_vec(&st.saved)
            .map_err(io::Error::other)
            .and_then(|b| {
                let tmp = self.file.with_extension("json.new");
                fs::write(&tmp, b)?;
                fs::rename(&tmp, &self.file)
            });
        if let Err(e) = r {
            eprintln!("vibe-env: can't keep {}: {e}", self.file.display());
        }
    }

    fn set_mode(&self, st: &mut State, mode: Mode) {
        if st.saved.mode != mode {
            st.saved.mode = mode;
            self.save(st);
        }
    }
}

/// The `linux` backend on a Linux host.
#[derive(Debug)]
pub struct Cgroups {
    /// The owner's home, whose `~/.claude` the agents use.
    home: PathBuf,
    /// The `claude` to run; by default the one on the agents' `PATH`.
    claude: Option<PathBuf>,
    /// The `pi` to run; by default the one on the agents' `PATH`.
    pi: Option<PathBuf>,
    /// The agents' `PATH`.
    path: String,
    systemctl: PathBuf,
    systemd_run: PathBuf,
    slice: String,
    /// Where cgroups v2 is mounted.
    root: PathBuf,
    intervals: Intervals,
    /// The user manager, once it's been found.
    systemd: Mutex<Option<Systemd>>,
    agents: Mutex<HashMap<String, Arc<Agent>>>,
    /// Numbers each run's scope.
    runs: AtomicU64,
}

impl Cgroups {
    /// A backend for the owner whose home is `home`, giving agents
    /// `vibed`'s own `PATH` (on a Linux host its systemd unit's, which is
    /// the owner's login shell's) with `~/.local/bin`, where Claude Code's
    /// installer puts it.
    pub fn new(home: impl Into<PathBuf>) -> Cgroups {
        let home = home.into();
        let path = agent_path(&std::env::var("PATH").unwrap_or_default(), &home);
        Cgroups {
            systemctl: find(&path, "systemctl"),
            systemd_run: find(&path, "systemd-run"),
            home,
            claude: None,
            pi: None,
            path,
            slice: SLICE.into(),
            root: PathBuf::from(CGROUP_ROOT),
            intervals: Intervals::default(),
            systemd: Mutex::new(None),
            agents: Mutex::new(HashMap::new()),
            runs: AtomicU64::new(0),
        }
    }

    /// Runs `claude` as Claude Code, rather than the one on `PATH`.
    pub fn with_claude(mut self, claude: impl Into<PathBuf>) -> Cgroups {
        self.claude = Some(claude.into());
        self
    }

    /// Runs `pi` as Pi, rather than the one on `PATH`.
    pub fn with_pi(mut self, pi: impl Into<PathBuf>) -> Cgroups {
        self.pi = Some(pi.into());
        self
    }

    /// Gives agents this `PATH`.
    pub fn with_path(mut self, path: impl Into<String>) -> Cgroups {
        self.path = path.into();
        self.systemctl = find(&self.path, "systemctl");
        self.systemd_run = find(&self.path, "systemd-run");
        self
    }

    /// Puts agents' scopes in `slice`, a top-level slice of the user manager
    /// (no `-` in its name), rather than [`SLICE`], as tests do to keep to
    /// their own.
    pub fn with_slice(mut self, slice: impl Into<String>) -> Cgroups {
        let slice = slice.into();
        assert!(
            slice.ends_with(".slice") && !slice.contains('-') && !slice.contains('/'),
            "{slice} isn't a top-level slice"
        );
        self.slice = slice;
        self
    }

    /// Watches at these intervals, as tests do to see a minute's disk
    /// check sooner.
    pub fn with_intervals(mut self, intervals: Intervals) -> Cgroups {
        self.intervals = intervals;
        self
    }

    /// Looks for cgroups v2 at `root` rather than `/sys/fs/cgroup`, as tests
    /// do to stand in for a server whose systemd lacks a controller.
    pub fn with_cgroup_root(mut self, root: impl Into<PathBuf>) -> Cgroups {
        self.root = root.into();
        self
    }

    /// The `PATH` agents get.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The slice agents' scopes are in.
    pub fn slice(&self) -> &str {
        &self.slice
    }

    /// The user manager, or why agents can't have cgroups here; and, if
    /// they can, what doesn't work, such as the CPU cap without the `cpu`
    /// controller.
    pub fn check(&self) -> Result<(Systemd, Vec<String>)> {
        let sd = self.systemd()?;
        let mut warnings = Vec::new();
        if !sd.cpu() {
            warnings.push(format!(
                "your systemd user manager wasn't given the cpu controller (it has: {}), so \
                 agents here aren't held to cpu_cores. To fix it, as root: `systemctl edit \
                 user@.service`, add `[Service]` and `Delegate=cpu memory pids`, then restart \
                 the server",
                sd.controllers.join(" ")
            ));
        }
        Ok((sd, warnings))
    }

    fn systemd(&self) -> Result<Systemd> {
        let mut cached = lock(&self.systemd);
        if let Some(sd) = &*cached {
            return Ok(sd.clone());
        }
        if !self.root.join("cgroup.controllers").is_file() {
            return Err(EnvError::new(format!(
                "this server doesn't have cgroups v2 at {}, which linux agents need for their \
                 limits",
                self.root.display()
            )));
        }
        let out = self.systemctl(&["show", "--property=Version", "--property=ControlGroup"])?;
        let sd = Systemd::parse(&out, &self.root)?;
        if !sd.manager.join("cgroup.freeze").is_file() {
            return Err(EnvError::new(
                "this server's kernel has no cgroup freezer (Linux 5.2 or later has it), so \
                 linux agents can't sleep or be paused here",
            ));
        }
        let missing = sd.missing();
        if !missing.is_empty() {
            // SAFETY: getuid has no preconditions.
            let uid = unsafe { libc::getuid() };
            return Err(EnvError::new(format!(
                "your systemd user manager can't limit {} (it was given: {}), so linux agents \
                 can't start here. To fix it, as root: `systemctl edit user@.service`, add \
                 `[Service]` and `Delegate=cpu memory pids`, then restart the server (or \
                 `systemctl restart user@{uid}.service`)",
                missing.join(" and "),
                if sd.controllers.is_empty() {
                    "none".to_owned()
                } else {
                    sd.controllers.join(" ")
                }
            )));
        }
        *cached = Some(sd.clone());
        Ok(sd)
    }

    /// Runs `systemctl --user` with `args`, and returns what it printed.
    fn systemctl(&self, args: &[&str]) -> Result<String> {
        let mut argv = vec![
            self.systemctl.to_string_lossy().into_owned(),
            "--user".to_owned(),
        ];
        argv.extend(args.iter().map(|a| (*a).to_owned()));
        let out = native::run(
            &Run {
                argv,
                env: Vec::new(),
                cwd: self.home.clone(),
                timeout: SYSTEMCTL_TIMEOUT,
            },
            &[],
        )
        .map_err(|e| {
            EnvError::new(format!(
                "can't ask your systemd user manager (systemctl --user): {e}"
            ))
        })?;
        let text = String::from_utf8_lossy(&out.output).trim().to_owned();
        if !out.success() {
            return Err(EnvError::new(format!(
                "systemctl --user {} failed{}{}",
                args.first().copied().unwrap_or(""),
                if out.timed_out { " (it timed out)" } else { "" },
                if text.is_empty() {
                    String::new()
                } else {
                    format!(": {text}")
                }
            )));
        }
        Ok(text)
    }

    /// The scope `agent_ref`'s processes run in.
    pub fn unit(&self, agent_ref: &str) -> String {
        format!("{}-{agent_ref}.scope", self.stem())
    }

    fn stem(&self) -> &str {
        self.slice.strip_suffix(".slice").unwrap_or(&self.slice)
    }

    /// Where `unit`'s cgroup is.
    fn cgroup_of(&self, sd: &Systemd, unit: &str) -> Cgroup {
        Cgroup {
            dir: sd.manager.join(&self.slice).join(unit),
        }
    }

    /// The cgroup `agent_ref`'s processes run in, once it's launched.
    pub fn cgroup(&self, agent_ref: &str) -> Result<Cgroup> {
        let sd = self.systemd()?;
        Ok(self.cgroup_of(&sd, &self.unit(agent_ref)))
    }

    /// The limits as systemd properties.
    fn properties(sd: &Systemd, limits: &Limits) -> Vec<String> {
        let mut p = vec![
            format!("MemoryMax={}", limits.memory),
            format!("MemoryHigh={}", pause_at(limits)),
            format!("MemorySwapMax={}", limits.memory),
            format!(
                "TasksMax={}",
                u64::from(limits.max_processes.max(1)) * TASKS_PER_PROCESS
            ),
        ];
        if sd.cpu() {
            p.push(format!(
                "CPUQuota={}%",
                u64::from(limits.cpu_cores.max(1)) * 100
            ));
        }
        p
    }

    /// `systemd-run` making `unit` with `limits`, then becoming `command`.
    fn scope_command(
        &self,
        sd: &Systemd,
        unit: &str,
        limits: &Limits,
        command: &[String],
    ) -> Vec<String> {
        let mut argv: Vec<String> = [
            self.systemd_run.to_string_lossy().as_ref(),
            "--user",
            "--scope",
            "--quiet",
            "--collect",
        ]
        .map(str::to_owned)
        .to_vec();
        if sd.version >= EXPAND_ENVIRONMENT_FROM {
            argv.push("--expand-environment=no".into());
        }
        argv.push(format!("--unit={unit}"));
        argv.push(format!("--slice={}", self.slice));
        let mut props = Cgroups::properties(sd, limits);
        if sd.version >= SCOPE_OOM_POLICY_FROM {
            props.push("OOMPolicy=continue".into());
        }
        for p in props {
            argv.push("-p".into());
            argv.push(p);
        }
        argv.push("--".into());
        argv.extend(command.iter().cloned());
        argv
    }

    /// The `claude` agents run.
    pub fn claude(&self) -> Result<PathBuf> {
        if let Some(c) = &self.claude {
            return Ok(c.clone());
        }
        std::env::split_paths(&self.path)
            .map(|d| d.join("claude"))
            .find(|c| is_executable(c))
            .ok_or_else(|| {
                EnvError::new(format!(
                    "can't find claude on this server's PATH ({}). VibeOS installs it by itself: try again in \
                     a moment, and Settings → Hosts signs in with one click",
                    self.path
                ))
            })
    }

    /// The `pi` agents run: the server's own, like `claude`.
    pub fn pi(&self) -> Result<PathBuf> {
        if let Some(p) = &self.pi {
            return Ok(p.clone());
        }
        std::env::split_paths(&self.path)
            .map(|d| d.join("pi"))
            .find(|c| is_executable(c))
            .ok_or_else(|| {
                EnvError::new(format!(
                    "can't find pi on this server's PATH ({}). VibeOS installs it by itself when \
                     a profile here runs Pi",
                    self.path
                ))
            })
    }

    fn env(&self) -> Vec<(String, String)> {
        vec![
            ("HOME".into(), self.home.to_string_lossy().into_owned()),
            ("PATH".into(), self.path.clone()),
        ]
    }

    fn agent(&self, h: &Handle) -> Arc<Agent> {
        Arc::clone(
            lock(&self.agents)
                .entry(h.agent_ref.clone())
                .or_insert_with(|| Arc::new(Agent::new(h))),
        )
    }

    /// The limits `st` starts a launch or a run with: those it was last
    /// given, or the defaults until it's given any.
    fn limits_of(st: &State) -> Limits {
        st.saved.limits.unwrap_or_else(|| {
            Limits::new(
                vibe_core::model::DEFAULT_PROFILE_MEMORY,
                &HostLimits::default(),
            )
        })
    }

    /// Stops every watcher, as `vibed` does before it exits. The kernel
    /// keeps holding the agents to their limits, and a paused one frozen.
    pub fn release(&self) {
        for agent in lock(&self.agents).values() {
            if let Some(stop) = lock(&agent.state).watcher.take() {
                stop.store(true, Ordering::SeqCst);
            }
        }
    }
}

impl Drop for Cgroups {
    fn drop(&mut self) {
        self.release();
    }
}

/// `path` with `~/.local/bin` in it, and the usual places if it's empty.
fn agent_path(path: &str, home: &Path) -> String {
    let mut dirs: Vec<PathBuf> = std::env::split_paths(path)
        .filter(|d| !d.as_os_str().is_empty())
        .collect();
    if dirs.is_empty() {
        dirs = ["/usr/local/bin", "/usr/bin", "/bin"]
            .map(PathBuf::from)
            .to_vec();
    }
    let local = home.join(".local/bin");
    if !dirs.contains(&local) {
        dirs.insert(0, local);
    }
    std::env::join_paths(&dirs)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_owned())
}

/// `name` on `path`, or else in `/usr/bin` and `/bin`, or just its name.
fn find(path: &str, name: &str) -> PathBuf {
    std::env::split_paths(path)
        .chain([PathBuf::from("/usr/bin"), PathBuf::from("/bin")])
        .map(|d| d.join(name))
        .find(|p| is_executable(p))
        .unwrap_or_else(|| PathBuf::from(name))
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The agent's shim, while it's still the one that holds its terminal: the
/// session's leader is a `vibe-shim` for this ref. Checking the ref keeps a
/// pid reused after a reboot by another agent's shim from being taken for
/// it.
fn shim_of(h: &Handle) -> Option<u32> {
    let sid = h.session?;
    let (p, session) = tree::process(sid)?;
    let ours = p.name == SHIM_NAME
        && session == sid
        && p.args
            .windows(2)
            .any(|w| w[0] == "--ref" && w[1] == h.agent_ref);
    ours.then_some(sid)
}

/// Ends the shim `sid`, and whatever is left in its session.
fn end_shim(sid: u32) {
    SessionTree::new(sid).end(STOP_GRACE);
}

fn not_running(h: &Handle) -> EnvError {
    EnvError::new(format!("{} isn't running", h.agent_ref))
}

impl Compartment for Cgroups {
    /// Its temp dir and its state; the cgroup comes with its launch.
    /// Claude's state is the owner's own `~/.claude`.
    fn create(&self, spec: &AgentSpec) -> Result<Handle> {
        // Said now, before anything runs, if agents can't have cgroups here.
        self.systemd()?;
        let tmp = spec.agent_dir.join(vibe_core::paths::TMP_DIR);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&tmp)
            .map_err(|e| EnvError::new(format!("can't create {}: {e}", tmp.display())))?;
        let h = Handle {
            agent_ref: spec.agent_ref.clone(),
            agent_dir: spec.agent_dir.clone(),
            worktree: spec.worktree.clone(),
            home: match spec.agent {
                AgentKind::Claude => AgentHome::Claude(ClaudeHome::user(&self.home)),
                AgentKind::Pi => AgentHome::Pi(crate::PiHome::user(
                    crate::pi::own_dir(&self.home),
                    &spec.worktree,
                )),
            },
            session: None,
        };
        let agent = self.agent(&h);
        let mut st = lock(&agent.state);
        st.saved = Saved {
            mode: Mode::Running,
            limits: Some(spec.limits),
        };
        agent.save(&st);
        Ok(h)
    }

    /// In a scope of its own, with the agent's limits. Whatever it leaves
    /// running ends with it.
    fn run(&self, h: &Handle, r: &Run) -> Result<RunOutput> {
        let sd = self.systemd()?;
        let tmp = h.agent_dir.join(vibe_core::paths::TMP_DIR);
        fs::create_dir_all(&tmp)?;
        let mut env = self.env();
        env.push(("TMPDIR".into(), tmp.to_string_lossy().into_owned()));
        let limits = Cgroups::limits_of(&lock(&self.agent(h).state));
        // `:` can't be in a ref, so no agent's scope has this name.
        let unit = format!(
            "{}-{}:run-{}-{}.scope",
            self.stem(),
            h.agent_ref,
            std::process::id(),
            self.runs.fetch_add(1, Ordering::SeqCst)
        );
        let argv = self.scope_command(&sd, &unit, &limits, &r.argv);
        let out = native::run_for(
            h,
            &Run {
                argv,
                env: r.env.clone(),
                cwd: r.cwd.clone(),
                timeout: r.timeout,
            },
            &env,
        )?;
        let cg = self.cgroup_of(&sd, &unit);
        cg.end(Duration::ZERO);
        cg.wait_gone(GONE);
        Ok(out)
    }

    fn pi_auth_check(
        &self,
        h: &Handle,
        env: &[(String, String)],
        model: &str,
    ) -> Option<crate::pi::AuthCheck> {
        native::pi_auth_check(&self.pi().ok()?, h, env, self.env(), model)
    }

    fn native_pi(&self) -> Option<std::result::Result<PathBuf, String>> {
        Some(self.pi().map_err(|e| e.to_string()))
    }

    fn launch(&self, h: &Handle, launch: &Launch) -> Result<TerminalEnd> {
        let sd = self.systemd()?;
        let program = match launch.agent {
            AgentKind::Claude => self.claude()?,
            AgentKind::Pi => self.pi()?,
        };
        let unit = self.unit(&h.agent_ref);
        let cg = self.cgroup_of(&sd, &unit);
        let agent = self.agent(h);
        let mut st = lock(&agent.state);
        // What's left of an earlier life (stragglers its shim hung up)
        // ends, so the scope's name is free.
        if cg.exists() {
            if shim_of(h).is_some() && !cg.pids().is_empty() {
                return Err(EnvError::new(format!("{} is running already", h.agent_ref)));
            }
            cg.end(Duration::ZERO);
            if !cg.wait_gone(GONE) {
                let _ = self.systemctl(&["stop", &unit]);
            }
        }
        let limits = Cgroups::limits_of(&st);
        let native::AgentCommand {
            command,
            env,
            clean_env,
        } = native::agent_command(&program, h, launch, self.env())?;
        let command = self.scope_command(&sd, &unit, &limits, &command);
        // A new launch runs: nothing of an old pause or sleep holds.
        agent.set_mode(&mut st, Mode::Running);
        st.warnings.clear();
        st.disk = None;
        st.recount = Some(Recount::Zero);
        Ok(TerminalEnd::Pty {
            command,
            cwd: launch.cwd.clone(),
            env,
            clean_env,
        })
    }

    fn processes(&self, h: &Handle) -> Result<Vec<Proc>> {
        let cg = self.cgroup(&h.agent_ref)?;
        let frozen = cg.frozen();
        Ok(cg
            .pids()
            .into_iter()
            .filter(|pid| Some(*pid) != h.session)
            .filter_map(|pid| tree::process(pid).map(|(p, _)| p))
            .map(|p| Proc {
                stopped: p.stopped || frozen,
                ..p
            })
            .collect())
    }

    fn stop(&self, h: &Handle) -> Result<()> {
        let agent = self.agent(h);
        let mut st = lock(&agent.state);
        if let Some(stop) = st.watcher.take() {
            stop.store(true, Ordering::SeqCst);
        }
        let shim = shim_of(h);
        let sd = self.systemd()?;
        let unit = self.unit(&h.agent_ref);
        let cg = self.cgroup_of(&sd, &unit);
        if !cg.end(STOP_GRACE) {
            return Err(EnvError::new(format!(
                "{}'s processes didn't end",
                h.agent_ref
            )));
        }
        if !cg.wait_gone(GONE) {
            let _ = self.systemctl(&["stop", &unit]);
        }
        if let Some(sid) = shim {
            end_shim(sid);
        }
        agent.set_mode(&mut st, Mode::Running);
        st.warnings.clear();
        st.disk = None;
        Ok(())
    }

    /// Ends it, and removes its temp dir, its state and Claude's records
    /// of its worktree.
    fn delete(&self, h: &Handle) -> Result<()> {
        self.stop(h)?;
        h.home
            .forget(&h.worktree)
            .map_err(|e| EnvError::new(format!("can't remove its agent's state: {e}")))?;
        let gone = |r: io::Result<()>, what: &Path| match r {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(EnvError::new(format!(
                "can't remove {}: {e}",
                what.display()
            ))),
            _ => Ok(()),
        };
        let tmp = h.agent_dir.join(vibe_core::paths::TMP_DIR);
        gone(remove_all_writable(&tmp), &tmp)?;
        let file = h.agent_dir.join(STATE_FILE);
        gone(fs::remove_file(&file), &file)?;
        lock(&self.agents).remove(&h.agent_ref);
        Ok(())
    }
}

/// Removes `dir` whole, even where the agent left a directory read-only.
fn remove_all_writable(dir: &Path) -> io::Result<()> {
    match fs::remove_dir_all(dir) {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            let mut stack = vec![dir.to_owned()];
            while let Some(d) = stack.pop() {
                let _ = fs::set_permissions(&d, fs::Permissions::from_mode(0o700));
                if let Ok(entries) = fs::read_dir(&d) {
                    for e in entries.filter_map(std::result::Result::ok) {
                        if e.file_type().is_ok_and(|t| t.is_dir()) {
                            stack.push(e.path());
                        }
                    }
                }
            }
            fs::remove_dir_all(dir)
        }
        r => r,
    }
}

impl Limiter for Cgroups {
    /// Sets its scope's limits, and starts its watcher, or gives the one it
    /// has the new limits.
    fn apply(&self, h: &Handle, limits: &Limits) -> Result<()> {
        h.session.ok_or_else(|| not_running(h))?;
        let sd = self.systemd()?;
        let unit = self.unit(&h.agent_ref);
        let cg = self.cgroup_of(&sd, &unit);
        // Just launched, its scope appears once systemd has made it.
        let there = wait_until(APPEAR, || {
            (cg.exists() && !cg.pids().is_empty()) || shim_of(h).is_none()
        });
        if !there || !cg.exists() || cg.pids().is_empty() {
            return Err(not_running(h));
        }
        let agent = self.agent(h);
        let mut st = lock(&agent.state);
        st.saved.limits = Some(*limits);
        agent.save(&st);
        // Watched first, so a systemd that fails to answer leaves it held to
        // the limits it launched with, and still paused when it hits one.
        let watched = st
            .watcher
            .as_ref()
            .is_some_and(|w| !w.load(Ordering::SeqCst));
        if !watched {
            let stop = Arc::new(AtomicBool::new(false));
            {
                let (agent, stop, intervals) =
                    (Arc::clone(&agent), Arc::clone(&stop), self.intervals);
                let cg = cg.clone();
                std::thread::Builder::new()
                    .name(format!("cgroup {}", h.agent_ref))
                    .spawn(move || watch(&agent, &cg, &stop, intervals))?;
            }
            st.watcher = Some(stop);
        }
        let mut args = vec!["set-property", "--runtime", unit.as_str()];
        let props = Cgroups::properties(&sd, limits);
        args.extend(props.iter().map(String::as_str));
        self.systemctl(&args).map_err(|e| {
            EnvError::new(format!(
                "can't set {}'s limits: {}",
                h.agent_ref,
                e.message()
            ))
        })?;
        Ok(())
    }

    fn usage(&self, h: &Handle) -> Result<Usage> {
        let cg = self.cgroup(&h.agent_ref)?;
        let c = cg.counters();
        let agent = self.agent(h);
        let st = lock(&agent.state);
        Ok(Usage {
            memory: c.memory,
            cpu_ms: c.cpu_ms,
            processes: u32::try_from(c.processes).unwrap_or(u32::MAX),
            paused: match st.saved.mode {
                Mode::Paused { limit } => Some(limit),
                _ => None,
            },
            warnings: st.warnings.clone(),
            disk: st.disk,
        })
    }
}

impl Sleeper for Cgroups {
    /// Freezes its cgroup. A paused agent is frozen already, and stays
    /// paused.
    fn sleep(&self, h: &Handle) -> Result<()> {
        let cg = self.cgroup(&h.agent_ref)?;
        if shim_of(h).is_none() || cg.pids().is_empty() {
            return Err(not_running(h));
        }
        let agent = self.agent(h);
        let mut st = lock(&agent.state);
        let was = st.saved.mode;
        if was == Mode::Running {
            agent.set_mode(&mut st, Mode::Asleep);
        }
        if let Err(e) = cg.freeze(true) {
            // Half asleep isn't asleep: it runs on, and says why.
            if was == Mode::Running {
                agent.set_mode(&mut st, Mode::Running);
                let _ = cg.freeze(false);
            }
            return Err(EnvError::new(format!("can't freeze {}: {e}", h.agent_ref)));
        }
        Ok(())
    }

    /// Thaws its cgroup, asleep or paused; `--resume` if its processes or
    /// its shim are gone, as after a reboot.
    fn wake(&self, h: &Handle) -> Result<Woke> {
        let sd = self.systemd()?;
        let unit = self.unit(&h.agent_ref);
        let cg = self.cgroup_of(&sd, &unit);
        let agent = self.agent(h);
        let mut st = lock(&agent.state);
        let shim = shim_of(h);
        if shim.is_none() || cg.pids().is_empty() {
            // Without its shim it has no terminal, and without its
            // processes nothing to wake: what's left ends.
            cg.end(STOP_GRACE);
            cg.wait_gone(GONE);
            if let Some(sid) = shim {
                end_shim(sid);
            }
            agent.set_mode(&mut st, Mode::Running);
            return Ok(Woke::Resume);
        }
        cg.freeze(false)
            .map_err(|e| EnvError::new(format!("can't thaw {}: {e}", h.agent_ref)))?;
        agent.set_mode(&mut st, Mode::Running);
        st.recount = Some(Recount::Now);
        Ok(Woke::Exact)
    }
}

impl Backend for Cgroups {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            sleep_frees_memory: false,
            exact_after_reboot: false,
            kernel_limits: true,
            file_containment: false,
            wake_cost: Duration::from_millis(10),
        }
    }
}

/// What the kernel had refused an agent when its watcher last started
/// counting.
#[derive(Debug, Clone, Copy, Default)]
struct Baseline {
    refused: u64,
    ooms: u64,
}

impl Baseline {
    fn of(c: &Counters) -> Baseline {
        Baseline {
            refused: c.refused,
            ooms: c.ooms,
        }
    }
}

/// The memory an agent is paused at: [`PAUSE_PERCENT`] of its limit.
fn pause_at(limits: &Limits) -> u64 {
    u64::try_from(u128::from(limits.memory) * u128::from(PAUSE_PERCENT) / 100).unwrap_or(u64::MAX)
}

/// The limit `c` says the agent hit, if any, since `base`.
fn hit(c: &Counters, base: &Baseline, limits: &Limits, disk: Option<u64>) -> Option<Limit> {
    if c.processes > u64::from(limits.max_processes) || c.refused > base.refused {
        Some(Limit::Processes)
    } else if c.ooms > base.ooms || c.memory >= pause_at(limits) {
        Some(Limit::Memory)
    } else if disk.is_some_and(|d| d >= limits.disk_hard) {
        Some(Limit::Disk)
    } else {
        None
    }
}

/// The warnings `c` and `disk` call for, given those there are: memory at
/// [`crate::governor::WARN_PERCENT`] of the limit, until it's back under
/// [`crate::governor::CLEAR_PERCENT`]; the disk past `disk_soft`.
fn warnings(c: &Counters, limits: &Limits, disk: Option<u64>, had: &[Limit]) -> Vec<Limit> {
    let share = |pct: u64| u128::from(limits.memory) * u128::from(pct) / 100;
    let memory = u128::from(c.memory);
    let mut out = Vec::new();
    let warn_memory = if had.contains(&Limit::Memory) {
        memory >= share(crate::governor::CLEAR_PERCENT)
    } else {
        memory >= share(crate::governor::WARN_PERCENT)
    };
    if warn_memory {
        out.push(Limit::Memory);
    }
    if disk.is_some_and(|d| d >= limits.disk_soft) {
        out.push(Limit::Disk);
    }
    out
}

/// Holds one agent to its limits until `stop`, or until its cgroup has
/// gone (it was stopped, or Claude exited and took everything with it).
fn watch(agent: &Arc<Agent>, cg: &Cgroup, stop: &Arc<AtomicBool>, intervals: Intervals) {
    // Counted from what's been refused when it first looks, unless a launch
    // or a wake says otherwise.
    let mut base: Option<Baseline> = None;
    // The latest measurement, and which: one begun before a wake is stale.
    let disk: Arc<Mutex<(u64, Option<u64>)>> = Arc::new(Mutex::new((0, None)));
    let mut generation = 0u64;
    let measuring = Arc::new(AtomicBool::new(false));
    let mut next_disk = Instant::now();
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let now = Instant::now();
        // `apply` saw the scope before starting this, so once it's gone the
        // agent's processes are, and the next launch's `apply` starts the
        // next watcher. Asked again under the lock, as `apply` holds it: a
        // relaunch that made the scope again meanwhile is this one's to
        // watch.
        if !cg.exists() {
            let mut st = lock(&agent.state);
            if !cg.exists() {
                if st.watcher.as_ref().is_some_and(|w| Arc::ptr_eq(w, stop)) {
                    st.watcher = None;
                }
                return;
            }
        }
        {
            let mut st = lock(&agent.state);
            if stop.load(Ordering::SeqCst) {
                return;
            }
            if let Some(how) = st.recount.take() {
                base = Some(match how {
                    Recount::Zero => Baseline::default(),
                    Recount::Now => Baseline::of(&cg.counters()),
                });
                generation += 1;
                *lock(&disk) = (generation, None);
                st.disk = None;
                next_disk = now;
            }
            let base = *base.get_or_insert_with(|| Baseline::of(&cg.counters()));
            if next_disk <= now && !measuring.swap(true, Ordering::SeqCst) {
                next_disk = now + intervals.disk;
                let paths = vec![agent.worktree.clone(), agent.tmp.clone()];
                let (into, done) = (Arc::clone(&disk), Arc::clone(&measuring));
                let spawned = std::thread::Builder::new()
                    .name("cgroup disk".into())
                    .spawn(move || {
                        let bytes = crate::macos::disk_usage(&paths);
                        let mut d = lock(&into);
                        if d.0 == generation {
                            d.1 = Some(bytes);
                        }
                        drop(d);
                        done.store(false, Ordering::SeqCst);
                    });
                if spawned.is_err() {
                    measuring.store(false, Ordering::SeqCst);
                }
            }
            if let (g, Some(bytes)) = *lock(&disk)
                && g == generation
            {
                st.disk = Some(bytes);
            }
            if let (Mode::Running, Some(limits)) = (st.saved.mode, st.saved.limits) {
                let c = cg.counters();
                let now_warned = warnings(&c, &limits, st.disk, &st.warnings);
                st.warnings = now_warned;
                if let Some(limit) = hit(&c, &base, &limits, st.disk) {
                    match cg.freeze(true) {
                        Ok(()) => agent.set_mode(&mut st, Mode::Paused { limit }),
                        Err(e) => eprintln!(
                            "vibe-env: can't pause {} for {}: {e}",
                            cg.dir.display(),
                            limit.as_str()
                        ),
                    }
                }
            }
        }
        std::thread::sleep(intervals.watch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn systemd(version: u32, controllers: &[&str]) -> Systemd {
        Systemd {
            version,
            manager: PathBuf::from("/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service"),
            controllers: controllers.iter().map(|c| (*c).to_owned()).collect(),
        }
    }

    fn limits() -> Limits {
        Limits {
            memory: 64 << 20,
            cpu_cores: 2,
            max_processes: 100,
            disk_soft: 1000,
            disk_hard: 2000,
        }
    }

    #[test]
    fn systemctl_show_says_the_version_and_the_cgroup() {
        let root = tempfile::tempdir().unwrap();
        let manager = root.path().join("user.slice/user@1000.service");
        fs::create_dir_all(&manager).unwrap();
        fs::write(manager.join("cgroup.controllers"), "cpu memory pids\n").unwrap();
        let sd = Systemd::parse(
            "Version=259.5-0ubuntu3.4\nControlGroup=/user.slice/user@1000.service\n",
            root.path(),
        )
        .unwrap();
        assert_eq!(sd.version, 259);
        assert_eq!(sd.manager, manager);
        assert!(sd.missing().is_empty() && sd.cpu());
        let old = Systemd::parse(
            "Version=v245.4\nControlGroup=/user.slice/user@1000.service",
            root.path(),
        )
        .unwrap();
        assert_eq!(old.version, 245);
        fs::write(manager.join("cgroup.controllers"), "memory\n").unwrap();
        let some = Systemd::parse(
            "Version=250\nControlGroup=/user.slice/user@1000.service",
            root.path(),
        )
        .unwrap();
        assert_eq!(some.missing(), vec!["pids"]);
        assert!(!some.cpu());
        let err = Systemd::parse("Version=250\nControlGroup=\n", root.path()).unwrap_err();
        assert!(err.message().contains("cgroup"), "{err}");
        assert!(Systemd::parse("ControlGroup=/x\n", root.path()).is_err());
    }

    #[test]
    fn a_scope_command_keeps_the_prompt_and_carries_the_limits() {
        let b = Cgroups::new("/home/me").with_path("/usr/bin:/bin");
        let cmd = |sd: &Systemd| {
            b.scope_command(
                sd,
                "vibeos-quiet-amber-otter.scope",
                &limits(),
                &["claude".into(), "--".into(), "fix $HOME's `x`".into()],
            )
        };
        let new = cmd(&systemd(259, &["cpu", "memory", "pids"]));
        let tail = &new[new.iter().position(|a| a == "--").unwrap()..];
        assert_eq!(tail, ["--", "claude", "--", "fix $HOME's `x`"]);
        for want in [
            "--user",
            "--scope",
            "--collect",
            "--expand-environment=no",
            "--unit=vibeos-quiet-amber-otter.scope",
            "--slice=vibeos.slice",
            "MemoryMax=67108864",
            "MemoryHigh=63753420",
            "MemorySwapMax=67108864",
            "TasksMax=800",
            "CPUQuota=200%",
            "OOMPolicy=continue",
        ] {
            assert!(new.iter().any(|a| a == want), "{want} in {new:?}");
        }
        // An older systemd expands nothing and has no OOMPolicy for scopes;
        // one without the cpu controller gets no CPUQuota it would ignore.
        let old = cmd(&systemd(250, &["memory", "pids"]));
        for gone in [
            "--expand-environment=no",
            "OOMPolicy=continue",
            "CPUQuota=200%",
        ] {
            assert!(!old.iter().any(|a| a == gone), "{gone} in {old:?}");
        }
    }

    #[test]
    fn units_and_their_cgroups() {
        let b = Cgroups::new("/home/me").with_slice("vibetest.slice");
        assert_eq!(
            b.unit("quiet-amber-otter"),
            "vibetest-quiet-amber-otter.scope"
        );
        let sd = systemd(259, &["memory", "pids"]);
        assert_eq!(
            b.cgroup_of(&sd, &b.unit("a.b_c")).dir,
            sd.manager.join("vibetest.slice/vibetest-a.b_c.scope")
        );
    }

    #[test]
    #[should_panic(expected = "isn't a top-level slice")]
    fn a_nested_slice_is_refused() {
        let _ = Cgroups::new("/h").with_slice("vibe-test.slice");
    }

    #[test]
    fn agents_find_local_bin() {
        let home = Path::new("/home/me");
        assert_eq!(
            agent_path("/usr/bin:/bin", home),
            "/home/me/.local/bin:/usr/bin:/bin"
        );
        assert_eq!(
            agent_path("/home/me/.local/bin:/usr/bin", home),
            "/home/me/.local/bin:/usr/bin"
        );
        assert_eq!(
            agent_path("", home),
            "/home/me/.local/bin:/usr/local/bin:/usr/bin:/bin"
        );
    }

    #[test]
    fn memory_is_what_reclaim_cant_drop_plus_swap() {
        let stat = keyed("anon 100\nfile 300\nshmem 50\nkernel 7\n");
        assert_eq!(stat.get("anon"), Some(&100));
        // 500 charged, 250 of it page cache that can go.
        assert_eq!(own_memory(500, &stat), 250);
        assert_eq!(own_memory(10, &stat), 0);
        assert_eq!(own_memory(500, &HashMap::new()), 500);
    }

    #[test]
    fn what_the_kernel_refused_is_a_hit() {
        let l = limits();
        let base = Baseline {
            refused: 3,
            ooms: 1,
        };
        let ok = Counters {
            processes: 10,
            refused: 3,
            ooms: 1,
            memory: 1 << 20,
            cpu_ms: 0,
        };
        assert_eq!(hit(&ok, &base, &l, Some(10)), None);
        let forks = Counters { refused: 4, ..ok };
        assert_eq!(hit(&forks, &base, &l, None), Some(Limit::Processes));
        let over = Counters {
            processes: 101,
            ..ok
        };
        assert_eq!(hit(&over, &base, &l, None), Some(Limit::Processes));
        let at = Counters {
            processes: 100,
            ..ok
        };
        assert_eq!(hit(&at, &base, &l, None), None);
        let oom = Counters { ooms: 2, ..ok };
        assert_eq!(hit(&oom, &base, &l, None), Some(Limit::Memory));
        // Paused at 95%, where the kernel throttles it, before the OOM
        // killer could pick anything at 100%.
        let edge = Counters {
            memory: (64 << 20) * 95 / 100,
            ..ok
        };
        assert_eq!(hit(&edge, &base, &l, None), Some(Limit::Memory));
        let under = Counters {
            memory: (64 << 20) * 94 / 100,
            ..ok
        };
        assert_eq!(hit(&under, &base, &l, None), None);
        assert_eq!(hit(&ok, &base, &l, Some(2000)), Some(Limit::Disk));
        assert_eq!(hit(&ok, &base, &l, Some(1999)), None);
    }

    #[test]
    fn warnings_come_at_80_percent_and_go_under_75() {
        let l = limits();
        let at = |pct: u64| Counters {
            memory: (64 << 20) * pct / 100,
            ..Counters::default()
        };
        assert!(warnings(&at(79), &l, None, &[]).is_empty());
        assert_eq!(warnings(&at(80), &l, None, &[]), vec![Limit::Memory]);
        assert_eq!(
            warnings(&at(76), &l, None, &[Limit::Memory]),
            vec![Limit::Memory]
        );
        assert!(warnings(&at(74), &l, None, &[Limit::Memory]).is_empty());
        assert_eq!(warnings(&at(0), &l, Some(1000), &[]), vec![Limit::Disk]);
    }

    #[test]
    fn modes_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let h = Handle {
            agent_ref: "a".into(),
            agent_dir: dir.path().to_owned(),
            worktree: dir.path().to_owned(),
            home: AgentHome::Claude(ClaudeHome::user(dir.path())),
            session: None,
        };
        let agent = Agent::new(&h);
        {
            let mut st = lock(&agent.state);
            st.saved.limits = Some(limits());
            agent.set_mode(
                &mut st,
                Mode::Paused {
                    limit: Limit::Memory,
                },
            );
        }
        let again = Agent::new(&h);
        let st = lock(&again.state);
        assert_eq!(
            st.saved,
            Saved {
                mode: Mode::Paused {
                    limit: Limit::Memory
                },
                limits: Some(limits()),
            }
        );
        // A file it can't read is a running agent with no limits yet.
        fs::write(dir.path().join(STATE_FILE), "nonsense").unwrap();
        assert_eq!(lock(&Agent::new(&h).state).saved, Saved::default());
    }
}
