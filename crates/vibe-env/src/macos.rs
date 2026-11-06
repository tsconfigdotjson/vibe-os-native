//! The `macos` backend (PRD: How macos is governed): native agents for
//! Xcode and other Mac toolchains, resource-limited but not sandboxed.
//!
//! - **Environment.** The agent runs under its shim's PTY, in the shim's own
//!   session, with `TMPDIR` at `~/VibeOS/agents/<ref>/tmp` (the shim sets
//!   it). It uses the Mac's own `claude`, `gh` and logins, and the Mac's own
//!   `~/.claude`: nothing is redirected. As `vibed` runs as a LaunchAgent
//!   with launchd's short `PATH`, the agent gets the `PATH` of the owner's
//!   login shell, started interactive as Terminal starts it, so it finds
//!   what Terminal would.
//! - **Membership.** A governor thread per agent walks its tree every
//!   [`Intervals::walk`] (`proc_listchildpids`, from the shim and from what
//!   it found before), and every [`Intervals::scan`] also matches every
//!   process by session ID (`getsid`), which catches jobs reparented to
//!   launchd (`nohup … &`). A member found once stays the agent's even
//!   after it leaves both tree and session. One that does both before it's
//!   found (a true daemon) escapes, and that's accepted. Every count and
//!   every signal goes by the same membership.
//! - **Limits**, decided by [`crate::governor`]: CPU in cores
//!   (`PRIO_DARWIN_BG`, then `SIGSTOP`/`SIGCONT` duty cycling), memory by
//!   `phys_footprint` (the background and a warning at 80%, a pause at
//!   100%), `max_processes` (a pause), and disk, the worktree and its temp
//!   dir measured every [`Intervals::disk`] (a warning past `disk_soft`, a
//!   pause past `disk_hard`). A pause is `SIGSTOP` on the tree; it holds
//!   until a wake, and survives a restart of `vibed` (`governor.json` in the
//!   agent's directory).
//! - **Sleep** is `SIGSTOP` on the tree and wake is `SIGCONT`. After a
//!   reboot the processes are gone, so a wake says `--resume`.
//!
//! The backend builds for every Unix, so its tests and the conformance
//! suite run on Linux too; there `PRIO_DARWIN_BG` has no counterpart, so the
//! background step does nothing. `vibed` uses it on a Mac only.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use vibe_core::model::AgentKind;

use crate::governor::Governor;
use crate::tree::{self, SessionTree};
use crate::{
    AgentHome, AgentSpec, Backend, Capabilities, ClaudeHome, Compartment, EnvError, Handle, Launch,
    Limit, Limiter, Limits, Proc, Result, Run, RunOutput, Sleeper, TerminalEnd, Usage, Woke,
    native,
};

/// What keeps a paused or sleeping agent's state across a restart of
/// `vibed`, in its directory.
pub const STATE_FILE: &str = "governor.json";

/// How long a stopping agent gets after `SIGTERM`.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// A duty cycle's period: the agent runs its duty of each, and is stopped
/// for the rest.
const PERIOD: Duration = Duration::from_millis(500);

/// How often the governor looks in between its walks, when it isn't duty
/// cycling.
const TICK: Duration = Duration::from_millis(100);

/// How long the owner's login shell may take to say its `PATH`.
const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(10);

/// Where `claude`, `gh` and the like usually are (relative ones are in the
/// home folder), added after the login shell's `PATH` in case it doesn't
/// set them or doesn't answer.
pub const USUAL_BINS: [&str; 3] = [".local/bin", "/opt/homebrew/bin", "/usr/local/bin"];

/// How often the governor does each of its jobs (PRD: How macos is
/// governed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Intervals {
    /// Walks the tree and acts on what it uses.
    pub walk: Duration,
    /// Also matches every process by session ID.
    pub scan: Duration,
    /// Measures the worktree and its temp dir.
    pub disk: Duration,
}

impl Default for Intervals {
    fn default() -> Intervals {
        Intervals {
            walk: Duration::from_secs(1),
            scan: Duration::from_secs(5),
            disk: Duration::from_secs(60),
        }
    }
}

/// The `macos` backend.
#[derive(Debug)]
pub struct Macos {
    /// The owner's home, whose `~/.claude` the agents use.
    home: PathBuf,
    /// The `claude` to run; by default the one on the agent's `PATH`.
    claude: Option<PathBuf>,
    /// The `pi` to run; by default the one on the agent's `PATH`.
    pi: Option<PathBuf>,
    /// The agent's `PATH`, once the owner's login shell has said it. One it
    /// couldn't say isn't kept, so the next launch asks again.
    path: Mutex<Option<String>>,
    intervals: Intervals,
    agents: Mutex<HashMap<String, Arc<Agent>>>,
}

/// What the backend knows of one agent, shared with its governor thread.
#[derive(Debug)]
struct Agent {
    /// Its [`STATE_FILE`].
    file: PathBuf,
    worktree: PathBuf,
    tmp: PathBuf,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    mode: Mode,
    limits: Option<Limits>,
    /// The thread holding it to its limits, while one does.
    governor: Option<Running>,
    /// Stopped by [`Compartment::stop`]: nothing holds it to limits again
    /// until it's launched again.
    stopped: bool,
    /// Its members as last followed, by pid with when each started: what
    /// left both the tree and the session after it was found is still the
    /// agent's.
    known: HashMap<u32, u64>,
    warnings: Vec<Limit>,
    disk: Option<u64>,
    /// What a duty cycle stopped, which it continues.
    duty_stopped: Vec<u32>,
    /// The share of each period it runs, as of the governor's last walk.
    duty: f64,
    /// Whether it's in the background. Children inherit that, so letting
    /// go brings back every member, not just those put there.
    background: bool,
    /// Set by a wake, so the governor measures afresh.
    woke: bool,
}

#[derive(Debug)]
struct Running {
    shim: u32,
    stop: Arc<AtomicBool>,
}

/// Whether the agent runs, or why it's stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum Mode {
    Running,
    Asleep,
    Paused { limit: Limit },
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Agent {
    fn new(h: &Handle) -> Agent {
        let file = h.agent_dir.join(STATE_FILE);
        let mode = fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(Mode::Running);
        Agent {
            file,
            worktree: h.worktree.clone(),
            tmp: h.agent_dir.join(vibe_core::paths::TMP_DIR),
            state: Mutex::new(State {
                mode,
                limits: None,
                governor: None,
                stopped: false,
                known: HashMap::new(),
                warnings: Vec::new(),
                disk: None,
                duty_stopped: Vec::new(),
                duty: 1.0,
                background: false,
                woke: false,
            }),
        }
    }

    /// Sets the mode, and keeps it in [`STATE_FILE`] unless it's running.
    fn set_mode(&self, st: &mut State, mode: Mode) {
        st.mode = mode;
        let r = match mode {
            Mode::Running => match fs::remove_file(&self.file) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                r => r,
            },
            _ => serde_json::to_vec(&mode)
                .map_err(io::Error::other)
                .and_then(|b| fs::write(&self.file, b)),
        };
        if let Err(e) = r {
            eprintln!("vibe-env: can't keep {}: {e}", self.file.display());
        }
    }
}

impl Macos {
    /// A backend for the owner whose home is `home`.
    pub fn new(home: impl Into<PathBuf>) -> Macos {
        Macos {
            home: home.into(),
            claude: None,
            pi: None,
            path: Mutex::new(None),
            intervals: Intervals::default(),
            agents: Mutex::new(HashMap::new()),
        }
    }

    /// Runs `claude` as Claude Code, rather than the one on `PATH`.
    pub fn with_claude(mut self, claude: impl Into<PathBuf>) -> Macos {
        self.claude = Some(claude.into());
        self
    }

    /// Runs `pi` as Pi, rather than the one on `PATH`.
    pub fn with_pi(mut self, pi: impl Into<PathBuf>) -> Macos {
        self.pi = Some(pi.into());
        self
    }

    /// Gives agents this `PATH`, rather than the login shell's.
    pub fn with_path(mut self, path: impl Into<String>) -> Macos {
        self.path = Mutex::new(Some(path.into()));
        self
    }

    /// Governs at these intervals, as tests do to see a minute's disk
    /// check sooner.
    pub fn with_intervals(mut self, intervals: Intervals) -> Macos {
        self.intervals = intervals;
        self
    }

    /// The `PATH` agents get.
    pub fn path(&self) -> String {
        let mut path = lock(&self.path);
        if let Some(p) = &*path {
            return p.clone();
        }
        match login_path(&self.home) {
            Ok(p) => {
                *path = Some(p.clone());
                p
            }
            Err(fallback) => fallback,
        }
    }

    /// The `claude` agents run.
    pub fn claude(&self) -> Result<PathBuf> {
        if let Some(c) = &self.claude {
            return Ok(c.clone());
        }
        let path = self.path();
        let legacy = self.home.join(".claude/local");
        std::env::split_paths(&path)
            .chain([self.home.join(".local/bin"), legacy])
            .map(|d| d.join("claude"))
            .find(|c| is_executable(c))
            .ok_or_else(|| {
                EnvError::new(format!(
                    "can't find Claude Code for macOS agents (looked in {path}). VibeOS installs it by itself: try again in a moment"
                ))
            })
    }

    /// The `pi` agents run: the Mac's own, like `claude`.
    pub fn pi(&self) -> Result<PathBuf> {
        if let Some(p) = &self.pi {
            return Ok(p.clone());
        }
        let path = self.path();
        // Where VibeOS's own install links it, whether or not the login shell's
        // PATH has that, as the doctor looks too.
        std::env::split_paths(&path)
            .chain([self.home.join(".local/bin")])
            .map(|d| d.join("pi"))
            .find(|c| is_executable(c))
            .ok_or_else(|| {
                EnvError::new(format!(
                    "can't find Pi for macOS agents (looked in {path}). VibeOS installs it by itself when a profile here runs Pi"
                ))
            })
    }

    fn agent(&self, h: &Handle) -> Arc<Agent> {
        Arc::clone(
            lock(&self.agents)
                .entry(h.agent_ref.clone())
                .or_insert_with(|| Arc::new(Agent::new(h))),
        )
    }

    /// The agent's tree as its governor last followed it, unless its
    /// session's pid now names someone else's.
    fn members(h: &Handle, st: &State) -> Option<Members> {
        let sid = h.session?;
        SessionTree::new(sid)
            .is_shims()
            .then(|| Members::new(sid).knowing(st.known.clone()))
    }

    /// What every command in the compartment runs with: the owner's own
    /// `HOME` and their login shell's `PATH`.
    fn env(&self) -> Vec<(String, String)> {
        vec![
            ("HOME".into(), self.home.to_string_lossy().into_owned()),
            ("PATH".into(), self.path()),
        ]
    }

    /// The share of each period its governor lets it run, as of its last walk:
    /// 1 unless it's duty cycling.
    pub fn duty(&self, h: &Handle) -> f64 {
        lock(&self.agent(h).state).duty
    }

    /// Stops every governor at once, bringing back what each put in the
    /// background or stopped for a duty cycle, as `vibed` must before it
    /// exits: its threads die with it, mid cycle or not.
    pub fn release(&self) {
        let agents: Vec<Arc<Agent>> = lock(&self.agents).values().cloned().collect();
        for agent in agents {
            let mut st = lock(&agent.state);
            if let Some(g) = st.governor.take() {
                g.stop.store(true, Ordering::SeqCst);
                let procs = Members::new(g.shim).knowing(st.known.clone()).refresh(true);
                let_go(&mut st, &procs);
            }
        }
    }
}

impl Drop for Macos {
    fn drop(&mut self) {
        self.release();
    }
}

/// Lets go of the levers: the background, and a duty cycle's stop, unless
/// the agent is meant to be stopped.
fn let_go(st: &mut State, procs: &[Proc]) {
    if std::mem::take(&mut st.background) {
        for p in procs {
            let _ = tree::set_background(p.pid, false);
        }
    }
    st.duty = 1.0;
    let stopped = std::mem::take(&mut st.duty_stopped);
    if st.mode == Mode::Running {
        for pid in stopped {
            tree::signal(pid, libc::SIGCONT);
        }
    }
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The owner's login shell: their account's, as launchd's environment may
/// not say.
fn login_shell() -> PathBuf {
    // SAFETY: getpwuid returns null or a record that stays valid until the
    // next call; it's copied out at once.
    let from_account = unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() || (*pw).pw_shell.is_null() {
            None
        } else {
            Some(
                std::ffi::CStr::from_ptr((*pw).pw_shell)
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    };
    from_account
        .or_else(|| std::env::var("SHELL").ok())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/bin/zsh"))
}

/// The owner's login shell's `PATH`, as an interactive login shell has it,
/// the way Terminal starts one, with the [`USUAL_BINS`] it lacks after it.
/// If the shell doesn't say, it's `Err` with `vibed`'s own `PATH` and the
/// usual places instead.
pub fn login_path(home: &Path) -> std::result::Result<String, String> {
    login_path_with(&login_shell(), home)
}

fn login_path_with(shell: &Path, home: &Path) -> std::result::Result<String, String> {
    const MARK: &str = "__VIBE_PATH__";
    // `printenv`, not `$PATH`: fish keeps PATH as a list.
    let asked = native::run(
        &Run {
            argv: vec![
                shell.to_string_lossy().into_owned(),
                "-l".into(),
                "-i".into(),
                "-c".into(),
                format!("printf '\\n{MARK}\\n'; /usr/bin/printenv PATH"),
            ],
            env: vec![("HOME".into(), home.to_string_lossy().into_owned())],
            cwd: home.to_owned(),
            timeout: LOGIN_SHELL_TIMEOUT,
        },
        &[],
    );
    let found = asked.ok().filter(RunOutput::success).and_then(|out| {
        let text = String::from_utf8_lossy(&out.output).into_owned();
        let mut lines = text.lines();
        lines.find(|l| l.ends_with(MARK))?;
        lines
            .map(str::trim)
            .find(|l| l.starts_with('/'))
            .map(str::to_owned)
    });
    match found {
        Some(p) => Ok(with_usual_bins(&p, home)),
        None => Err(with_usual_bins(
            &std::env::var("PATH").unwrap_or_default(),
            home,
        )),
    }
}

fn with_usual_bins(path: &str, home: &Path) -> String {
    let mut dirs: Vec<PathBuf> = std::env::split_paths(path)
        .filter(|d| !d.as_os_str().is_empty())
        .collect();
    for bin in USUAL_BINS {
        let d = home.join(bin);
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    std::env::join_paths(dirs)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_owned())
}

impl Compartment for Macos {
    /// Its temp dir. Claude's state is the Mac's own `~/.claude`.
    fn create(&self, spec: &AgentSpec) -> Result<Handle> {
        let tmp = spec.agent_dir.join(vibe_core::paths::TMP_DIR);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&tmp)
            .map_err(|e| EnvError::new(format!("can't create {}: {e}", tmp.display())))?;
        Ok(Handle {
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
        })
    }

    fn run(&self, h: &Handle, r: &Run) -> Result<RunOutput> {
        // As the shim does for the agent's terminal.
        let tmp = h.agent_dir.join(vibe_core::paths::TMP_DIR);
        fs::create_dir_all(&tmp)?;
        let mut env = self.env();
        env.push(("TMPDIR".into(), tmp.to_string_lossy().into_owned()));
        native::run_for(h, r, &env)
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
        let program = match launch.agent {
            AgentKind::Claude => self.claude()?,
            AgentKind::Pi => self.pi()?,
        };
        let native::AgentCommand {
            command,
            env,
            clean_env,
        } = native::agent_command(&program, h, launch, self.env())?;
        // A new launch runs: nothing of an old pause, sleep or stop holds.
        let agent = self.agent(h);
        let mut st = lock(&agent.state);
        agent.set_mode(&mut st, Mode::Running);
        st.stopped = false;
        st.known.clear();
        Ok(TerminalEnd::Pty {
            command,
            cwd: launch.cwd.clone(),
            env,
            clean_env,
        })
    }

    fn processes(&self, h: &Handle) -> Result<Vec<Proc>> {
        let agent = self.agent(h);
        let members = Macos::members(h, &lock(&agent.state));
        Ok(members.map(|mut m| m.refresh(true)).unwrap_or_default())
    }

    fn stop(&self, h: &Handle) -> Result<()> {
        let agent = self.agent(h);
        let members = {
            let mut st = lock(&agent.state);
            st.stopped = true;
            if let Some(g) = st.governor.take() {
                g.stop.store(true, Ordering::SeqCst);
            }
            let members = Macos::members(h, &st);
            let procs = members
                .clone()
                .map(|mut m| m.refresh(true))
                .unwrap_or_default();
            let_go(&mut st, &procs);
            members
        };
        if let Some(mut m) = members
            && !m.end(STOP_GRACE)
        {
            return Err(EnvError::new(format!(
                "{}'s processes didn't end",
                h.agent_ref
            )));
        }
        let mut st = lock(&agent.state);
        agent.set_mode(&mut st, Mode::Running);
        st.known.clear();
        Ok(())
    }

    /// Ends it, and removes its temp dir, its state and Claude's records
    /// of its worktree. What it installed elsewhere stays (PRD: Non-goals).
    fn delete(&self, h: &Handle) -> Result<()> {
        self.stop(h)?;
        h.home
            .forget(&h.worktree)
            .map_err(|e| EnvError::new(format!("can't remove its agent's state: {e}")))?;
        let tmp = h.agent_dir.join(vibe_core::paths::TMP_DIR);
        let gone = |r: io::Result<()>, what: &Path| match r {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(EnvError::new(format!(
                "can't remove {}: {e}",
                what.display()
            ))),
            _ => Ok(()),
        };
        gone(make_writable_and_remove(&tmp), &tmp)?;
        let file = h.agent_dir.join(STATE_FILE);
        gone(fs::remove_file(&file), &file)?;
        lock(&self.agents).remove(&h.agent_ref);
        Ok(())
    }
}

/// Removes `dir` whole, even where the agent left a directory read-only.
fn make_writable_and_remove(dir: &Path) -> io::Result<()> {
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

impl Limiter for Macos {
    /// Starts its governor, or gives the one it has new limits.
    fn apply(&self, h: &Handle, limits: &Limits) -> Result<()> {
        let not_running = || EnvError::new(format!("{} isn't running", h.agent_ref));
        let sid = h.session.ok_or_else(not_running)?;
        let agent = self.agent(h);
        let mut st = lock(&agent.state);
        // Its shim must be there: without it the tree is hung up.
        if st.stopped || !Macos::members(h, &st).is_some_and(|m| m.shim_alive()) {
            return Err(not_running());
        }
        st.limits = Some(*limits);
        if st
            .governor
            .as_ref()
            .is_some_and(|g| g.shim == sid && !g.stop.load(Ordering::SeqCst))
        {
            return Ok(());
        }
        if let Some(old) = st.governor.take() {
            old.stop.store(true, Ordering::SeqCst);
        }
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (agent, stop, intervals) = (Arc::clone(&agent), Arc::clone(&stop), self.intervals);
            let members = Members::new(sid).knowing(st.known.clone());
            std::thread::Builder::new()
                .name(format!("governor {}", h.agent_ref))
                .spawn(move || govern(&agent, members, &stop, intervals))?;
        }
        st.governor = Some(Running { shim: sid, stop });
        Ok(())
    }

    fn usage(&self, h: &Handle) -> Result<Usage> {
        let procs = self.processes(h)?;
        let agent = self.agent(h);
        let st = lock(&agent.state);
        Ok(Usage {
            memory: procs.iter().map(|p| p.memory).sum(),
            cpu_ms: procs.iter().map(|p| p.cpu_ms).sum(),
            processes: procs.len() as u32,
            paused: match st.mode {
                Mode::Paused { limit } => Some(limit),
                _ => None,
            },
            warnings: st.warnings.clone(),
            disk: st.disk,
        })
    }
}

impl Sleeper for Macos {
    /// `SIGSTOP` on the tree. A paused agent is stopped already, and stays
    /// paused.
    fn sleep(&self, h: &Handle) -> Result<()> {
        let agent = self.agent(h);
        let mut st = lock(&agent.state);
        let not_running = || EnvError::new(format!("{} isn't running", h.agent_ref));
        let mut members = Macos::members(h, &st).ok_or_else(not_running)?;
        if members.is_empty() {
            return Err(not_running());
        }
        let was = st.mode;
        if was == Mode::Running {
            // What a duty cycle stopped stays stopped.
            st.duty_stopped.clear();
            agent.set_mode(&mut st, Mode::Asleep);
        }
        if let Err(e) = members.freeze() {
            // Half asleep isn't asleep: it runs on, and says why.
            if was == Mode::Running {
                agent.set_mode(&mut st, Mode::Running);
                members.thaw();
            }
            return Err(e.into());
        }
        Ok(())
    }

    /// `SIGCONT` on the tree, asleep or paused; `--resume` if its processes
    /// are gone, as after a reboot.
    fn wake(&self, h: &Handle) -> Result<Woke> {
        let agent = self.agent(h);
        let mut st = lock(&agent.state);
        let members = Macos::members(h, &st);
        // Without its shim the agent has no terminal: what's left of its
        // tree is hung up.
        let Some(mut members) = members.filter(Members::shim_alive) else {
            if let Some(mut m) = Macos::members(h, &st) {
                m.end(STOP_GRACE);
            }
            agent.set_mode(&mut st, Mode::Running);
            return Ok(Woke::Resume);
        };
        agent.set_mode(&mut st, Mode::Running);
        st.duty_stopped.clear();
        st.woke = true;
        members.thaw();
        Ok(Woke::Exact)
    }
}

impl Backend for Macos {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            sleep_frees_memory: false,
            exact_after_reboot: false,
            kernel_limits: false,
            file_containment: false,
            wake_cost: Duration::from_millis(10),
        }
    }
}

/// An agent's tree: walked from its shim and from every member found
/// before, and, with a scan, matched by session too. It's the one account
/// of the agent's processes: the governor counts them, and sleep, wake,
/// pauses, stops and `processes` act on them.
#[derive(Debug, Clone)]
struct Members {
    shim: u32,
    /// When the shim started, once seen: a pid reused by someone else
    /// isn't taken for it.
    shim_started: Option<u64>,
    /// Each member found, by pid, with when it started.
    known: HashMap<u32, u64>,
}

impl Members {
    fn new(shim: u32) -> Members {
        let shim_started = tree::process(shim)
            .filter(|(p, _)| p.name == tree::SHIM_NAME)
            .map(|(p, _)| p.started_ms);
        Members {
            shim,
            shim_started,
            known: HashMap::new(),
        }
    }

    fn knowing(mut self, known: HashMap<u32, u64>) -> Members {
        self.known = known;
        self
    }

    /// Whether the shim is still there, the one that started.
    fn shim_alive(&self) -> bool {
        self.shim_started.is_some_and(|started| {
            tree::process(self.shim).is_some_and(|(p, _)| p.started_ms == started)
        })
    }

    /// Walks the tree from the shim and every member still there; with
    /// `scan`, from every process of the session too. The shim isn't a
    /// member.
    fn refresh(&mut self, scan: bool) -> Vec<Proc> {
        let alive = self.shim_alive();
        // A pid that's some process's session ID isn't reused, so a shim
        // gone with no one in its place leaves its session to be scanned.
        let shim_free = alive || tree::process(self.shim).is_none();
        let mut roots: Vec<u32> = Vec::new();
        if alive {
            roots.push(self.shim);
        }
        for (&pid, &started) in &self.known {
            if tree::process(pid).is_some_and(|(p, _)| p.started_ms == started) {
                roots.push(pid);
            }
        }
        if scan && shim_free {
            roots.extend(SessionTree::new(self.shim).all().iter().map(|p| p.pid));
        }
        let mut seen: HashSet<u32> = HashSet::new();
        let mut found = Vec::new();
        while let Some(pid) = roots.pop() {
            if pid == self.shim && !alive {
                continue;
            }
            if !seen.insert(pid) {
                continue;
            }
            let Some((p, _)) = tree::process(pid) else {
                continue;
            };
            roots.extend(tree::children(pid));
            if pid != self.shim {
                found.push(p);
            }
        }
        self.known = found.iter().map(|p| (p.pid, p.started_ms)).collect();
        found
    }

    fn is_empty(&mut self) -> bool {
        !self.shim_alive() && self.refresh(true).is_empty()
    }

    /// Stops every member; see [`tree::freeze`].
    fn freeze(&mut self) -> io::Result<usize> {
        tree::freeze(|| self.refresh(true))
    }

    fn thaw(&mut self) {
        for p in self.refresh(true) {
            tree::signal(p.pid, libc::SIGCONT);
        }
    }

    /// Ends the whole tree: `SIGTERM` to the members (continued, so a
    /// frozen one gets it), up to `grace` for them to go, then `SIGKILL`
    /// to everything left, the shim included. Whether it emptied.
    fn end(&mut self, grace: Duration) -> bool {
        for p in self.refresh(true) {
            tree::signal(p.pid, libc::SIGTERM);
            tree::signal(p.pid, libc::SIGCONT);
        }
        if self.wait_empty(grace) {
            return true;
        }
        for p in self.refresh(true) {
            tree::signal(p.pid, libc::SIGKILL);
        }
        if self.shim_alive() {
            tree::signal(self.shim, libc::SIGKILL);
        }
        self.wait_empty(Duration::from_secs(5))
    }

    fn wait_empty(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.is_empty() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The latest disk measurement, and which one: a measurement begun before a
/// wake doesn't count after it.
#[derive(Debug, Default)]
struct Disk {
    generation: u64,
    bytes: Option<u64>,
}

/// The governor's thread: holds the agent to its limits until told to stop
/// or until its tree has gone.
fn govern(agent: &Agent, mut members: Members, stop: &AtomicBool, intervals: Intervals) {
    let mut governor: Option<Governor> = None;
    let started = Instant::now();
    let (mut next_walk, mut next_scan, mut next_disk) = (started, started, started);
    let disk = Arc::new(Mutex::new(Disk::default()));
    let measuring = Arc::new(AtomicBool::new(false));
    let mut procs: Vec<Proc> = Vec::new();
    let mut duty = 1.0;
    // A governor before this one, in a `vibed` that was killed, may have
    // left the agent in the background or stopped for a duty cycle. Unless
    // it's meant to be stopped, it runs as itself again.
    {
        let st = lock(&agent.state);
        if st.mode == Mode::Running && !stop.load(Ordering::SeqCst) {
            for p in members.refresh(true) {
                let _ = tree::set_background(p.pid, false);
                if p.stopped {
                    tree::signal(p.pid, libc::SIGCONT);
                }
            }
        }
    }
    while !stop.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now >= next_walk {
            next_walk = now + intervals.walk;
            let scan = now >= next_scan;
            if scan {
                next_scan = now + intervals.scan;
            }
            procs = members.refresh(scan);
            if procs.is_empty() && !members.shim_alive() {
                break;
            }
            let mut st = lock(&agent.state);
            if stop.load(Ordering::SeqCst) {
                break;
            }
            st.known = members.known.clone();
            let Some(limits) = st.limits else {
                break;
            };
            let g = governor.get_or_insert_with(|| Governor::new(limits));
            if *g.limits() != limits {
                g.set_limits(limits);
            }
            if std::mem::take(&mut st.woke) {
                g.reset();
                // What it took before the wake may be gone now.
                let mut d = lock(&disk);
                d.generation += 1;
                d.bytes = None;
                next_disk = now;
            }
            if next_disk <= now && !measuring.swap(true, Ordering::SeqCst) {
                next_disk = now + intervals.disk;
                let (into, done) = (Arc::clone(&disk), Arc::clone(&measuring));
                let generation = lock(&disk).generation;
                let paths = [agent.worktree.clone(), agent.tmp.clone()];
                let spawned = std::thread::Builder::new()
                    .name("governor disk".into())
                    .spawn(move || {
                        let bytes = disk_usage(&paths);
                        let mut d = lock(&into);
                        if d.generation == generation {
                            d.bytes = Some(bytes);
                        }
                        done.store(false, Ordering::SeqCst);
                    });
                if spawned.is_err() {
                    measuring.store(false, Ordering::SeqCst);
                }
            }
            let disk = lock(&disk).bytes;
            st.disk = disk;
            duty = 1.0;
            st.duty = duty;
            match st.mode {
                Mode::Running => {
                    let d = g.observe(now - started, wall_ms(), &procs, disk);
                    st.warnings = d.warnings;
                    if let Some(limit) = d.pause {
                        // Stopped whole, and held so until a wake.
                        st.duty_stopped.clear();
                        agent.set_mode(&mut st, Mode::Paused { limit });
                        let _ = members.freeze();
                        continue;
                    }
                    if d.background {
                        // New members too: each walk puts them there.
                        for p in &procs {
                            let _ = tree::set_background(p.pid, true);
                        }
                        st.background = true;
                    } else if std::mem::take(&mut st.background) {
                        for p in &procs {
                            let _ = tree::set_background(p.pid, false);
                        }
                    }
                    duty = d.duty;
                    st.duty = duty;
                }
                Mode::Asleep | Mode::Paused { .. } => {
                    // Still stopped: whatever runs again, or was started
                    // meanwhile, is stopped again.
                    if procs.iter().any(|p| !p.stopped) {
                        let _ = members.freeze();
                    }
                    g.reset();
                }
            }
        }
        if duty < 1.0 {
            {
                let mut st = lock(&agent.state);
                if st.mode != Mode::Running || stop.load(Ordering::SeqCst) {
                    // Asleep or paused since: no cycle until the next walk.
                    duty = 1.0;
                    continue;
                }
                for p in procs.iter().filter(|p| !p.stopped) {
                    if tree::signal(p.pid, libc::SIGSTOP) {
                        st.duty_stopped.push(p.pid);
                    }
                }
            }
            std::thread::sleep(PERIOD.mul_f64(1.0 - duty));
            {
                let mut st = lock(&agent.state);
                let stopped = std::mem::take(&mut st.duty_stopped);
                if st.mode == Mode::Running {
                    for pid in stopped {
                        tree::signal(pid, libc::SIGCONT);
                    }
                }
            }
            std::thread::sleep(PERIOD.mul_f64(duty));
        } else {
            std::thread::sleep(TICK.min(intervals.walk));
        }
    }
    let mut st = lock(&agent.state);
    // Only the governor still in charge lets go; a replaced one leaves its
    // successor's levers alone.
    let current = st
        .governor
        .as_ref()
        .is_some_and(|g| std::ptr::eq(Arc::as_ptr(&g.stop), stop));
    if current {
        st.governor = None;
        let_go(&mut st, &procs);
    }
}

/// The bytes `paths` take on disk, each file once however many links it
/// has, not following symbolic links.
pub fn disk_usage(paths: &[PathBuf]) -> u64 {
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut total = 0u64;
    let mut stack: Vec<PathBuf> = paths.to_vec();
    while let Some(path) = stack.pop() {
        let Ok(m) = fs::symlink_metadata(&path) else {
            continue;
        };
        if m.nlink() > 1 && !m.is_dir() && !seen.insert((m.dev(), m.ino())) {
            continue;
        }
        total = total.saturating_add(m.blocks().saturating_mul(512));
        if m.is_dir()
            && let Ok(entries) = fs::read_dir(&path)
        {
            stack.extend(entries.filter_map(|e| e.ok().map(|e| e.path())));
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_usage_counts_each_file_once_and_follows_no_links() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        fs::create_dir(&a).unwrap();
        let empty = disk_usage(std::slice::from_ref(&a));
        fs::write(a.join("one"), vec![1u8; 1 << 20]).unwrap();
        let one = disk_usage(std::slice::from_ref(&a));
        assert!(one >= empty + (1 << 20), "{empty} then {one}");
        // A second link to the same file, and a link to a big directory
        // outside, add nothing much.
        fs::hard_link(a.join("one"), a.join("two")).unwrap();
        let big = dir.path().join("big");
        fs::create_dir(&big).unwrap();
        fs::write(big.join("f"), vec![1u8; 4 << 20]).unwrap();
        std::os::unix::fs::symlink(&big, a.join("big")).unwrap();
        let after = disk_usage(std::slice::from_ref(&a));
        assert!(after < one + (64 << 10), "{one} then {after}");
        // Several roots add up, and a missing one is nothing.
        let both = disk_usage(&[a.clone(), big.clone(), dir.path().join("none")]);
        assert!(both >= after + (4 << 20), "{both}");
    }

    #[test]
    fn the_usual_bins_go_after_the_login_path() {
        let home = Path::new("/Users/u");
        assert_eq!(
            with_usual_bins("/opt/homebrew/bin:/usr/bin:/bin", home),
            "/opt/homebrew/bin:/usr/bin:/bin:/Users/u/.local/bin:/usr/local/bin"
        );
        assert_eq!(
            with_usual_bins("", home),
            "/Users/u/.local/bin:/opt/homebrew/bin:/usr/local/bin"
        );
    }

    /// A login shell that runs its command with `PATH` set to `path`,
    /// after some noise of the kind a profile makes.
    fn fake_shell(dir: &Path, path: &str, exit: i32) -> PathBuf {
        let shell = dir.join("shell");
        fs::write(
            &shell,
            format!(
                "#!/bin/sh\n\
                 printf 'welcome, no newline'\n\
                 echo 'zsh: no job control' >&2\n\
                 [ \"$1 $2 $3\" = '-l -i -c' ] || exit 9\n\
                 PATH='{path}' /bin/sh -c \"$4\" || exit 8\n\
                 exit {exit}\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
        shell
    }

    #[test]
    fn the_login_shell_says_its_path() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        let shell = fake_shell(h, "/opt/homebrew/bin:/usr/bin:/bin", 0);
        assert_eq!(
            login_path_with(&shell, h).unwrap(),
            format!(
                "/opt/homebrew/bin:/usr/bin:/bin:{}/.local/bin:/usr/local/bin",
                h.display()
            )
        );
        // One that fails says so, with vibed's own PATH instead.
        let shell = fake_shell(h, "/opt/x/bin:/usr/bin:/bin", 1);
        let fallback = login_path_with(&shell, h).unwrap_err();
        assert!(!fallback.contains("/opt/x/bin"), "{fallback}");
        assert!(fallback.contains(&format!("{}/.local/bin", h.display())));
        // And the account's own shell answers on this machine.
        assert!(login_path(h).is_ok(), "{:?}", login_path(h));
    }

    #[test]
    fn claude_is_found_on_the_path_or_said_to_be_missing() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let m = Macos::new(home.path()).with_path(bin.to_string_lossy().into_owned());
        let err = m.claude().unwrap_err();
        assert!(err.message().contains("can't find Claude Code"), "{err}");
        // Not executable isn't found either.
        fs::write(bin.join("claude"), "#!/bin/sh\n").unwrap();
        assert!(m.claude().is_err());
        fs::set_permissions(bin.join("claude"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(m.claude().unwrap(), bin.join("claude"));
        // Claude Code's old local install is looked in last.
        let legacy = home.path().join(".claude/local");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("claude"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(legacy.join("claude"), fs::Permissions::from_mode(0o755)).unwrap();
        let m = Macos::new(home.path()).with_path("/nonexistent");
        assert_eq!(m.claude().unwrap(), legacy.join("claude"));
        // One given is used as it is.
        let m = Macos::new(home.path()).with_claude("/x/claude");
        assert_eq!(m.claude().unwrap(), PathBuf::from("/x/claude"));
    }

    #[test]
    fn a_mode_is_kept_across_a_new_backend_until_it_runs_again() {
        let dir = tempfile::tempdir().unwrap();
        let h = Handle {
            agent_ref: "a".into(),
            agent_dir: dir.path().to_owned(),
            worktree: dir.path().join("w"),
            home: AgentHome::Claude(ClaudeHome::user(dir.path())),
            session: None,
        };
        let agent = Agent::new(&h);
        assert_eq!(lock(&agent.state).mode, Mode::Running);
        {
            let mut st = lock(&agent.state);
            agent.set_mode(
                &mut st,
                Mode::Paused {
                    limit: Limit::Memory,
                },
            );
        }
        let text = fs::read_to_string(dir.path().join(STATE_FILE)).unwrap();
        assert_eq!(text, r#"{"mode":"paused","limit":"memory"}"#);
        let again = Agent::new(&h);
        assert_eq!(
            lock(&again.state).mode,
            Mode::Paused {
                limit: Limit::Memory
            }
        );
        {
            let mut st = lock(&again.state);
            again.set_mode(&mut st, Mode::Running);
        }
        assert!(!dir.path().join(STATE_FILE).exists());
        // A usage read from a new backend says it's paused, as after a
        // restart of vibed.
        {
            let mut st = lock(&again.state);
            again.set_mode(&mut st, Mode::Asleep);
        }
        let m = Macos::new(dir.path()).with_path("/bin");
        let u = m.usage(&h).unwrap();
        assert_eq!(u.paused, None, "asleep isn't paused");
        assert_eq!(lock(&m.agent(&h).state).mode, Mode::Asleep);
    }

    #[test]
    fn a_wake_without_processes_is_a_resume_and_runs_again() {
        let dir = tempfile::tempdir().unwrap();
        let h = Handle {
            agent_ref: "a".into(),
            agent_dir: dir.path().to_owned(),
            worktree: dir.path().join("w"),
            home: AgentHome::Claude(ClaudeHome::user(dir.path())),
            session: Some(u32::MAX - 1),
        };
        fs::write(dir.path().join(STATE_FILE), r#"{"mode":"asleep"}"#).unwrap();
        let m = Macos::new(dir.path()).with_path("/bin");
        assert_eq!(m.wake(&h).unwrap(), Woke::Resume);
        assert!(!dir.path().join(STATE_FILE).exists());
        assert!(m.sleep(&h).is_err(), "nothing to put to sleep");
        assert!(
            m.apply(&h, &Limits::new(1 << 30, &Default::default()))
                .is_err()
        );
    }
}
