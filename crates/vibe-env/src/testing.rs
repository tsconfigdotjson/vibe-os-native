//! A backend for tests, so the core and the conformance suite don't wait on
//! the real backends. Never used for real agents.
//!
//! It runs the agent natively under the shim's PTY, like `macos` does,
//! with a Claude home of its own, so nothing touches the real `~/.claude`.
//! Its "Claude" is any program, normally [`STAND_IN`]. A supervisor thread
//! per agent holds it to its memory and process limits by pausing it, and
//! sleep is `SIGSTOP` on its tree. It can also stand in for a backend whose
//! sleep saves the agent to disk and frees its memory, as a VM's does
//! ([`TestBackend::sleeping_to_disk`]): its tree is still frozen, but it
//! keeps a snapshot file while it sleeps, and goes when that's dropped.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use vibe_core::model::AgentKind;

use crate::claude::{CONFIG_DIR_ENV, ClaudeHome};
use crate::tree::SessionTree;
use crate::{
    AgentHome, AgentSpec, Backend, Capabilities, Compartment, EnvError, Handle, Launch, Limit,
    Limiter, Limits, PiHome, Result, Run, RunOutput, Sleeper, TerminalEnd, Usage, Woke, native, pi,
};

/// A stand-in for Claude Code, as a POSIX shell script. It reads Claude's
/// arguments, writes a line to the transcript where Claude would, and then
/// does what the prompt's words say:
///
/// - `vanish`: the first time, exits having written nothing, as Claude can
///   before its first answer;
/// - `question`: asks "Red or blue?" and waits for the answer, writing
///   its prompt, an `AskUserQuestion`, the answer and its reply to the
///   transcript as Claude Code does, and then each line it's sent as a
///   turn of its own;
/// - `tick`: counts in `.vibe-ticks` in its directory, ten times a second;
/// - `fork N`: leaves `N` sleeping processes running;
/// - `fill N`: writes `N` MiB to `.vibe-fill` in its directory;
/// - `dirty`: leaves `notes.txt` uncommitted in its directory;
/// - `commit`: commits `work.txt` on its branch;
/// - `env`: shows its `TMPDIR` and the git settings agents get;
/// - `talk`: answers its prompt at once, coming to a firm rest, and then
///   each line it's sent as a turn of its own;
/// - `loop N`: as `talk`, but first arms a `/loop` wakeup due in `N`
///   seconds, as Claude Code's `ScheduleWakeup` records one, and fires it
///   then by the wall clock: the fire, its prompt and "Loop fired." go to
///   the transcript, and "Loop fired at <epoch seconds>." to the screen;
/// - anything else: says what it's working on.
///
/// Then it prints "Ready." and echoes each line it's sent until its
/// terminal hangs up. Resumed with `--resume`, it says so first. It's
/// `stand-in.sh` beside this file, which `scripts/check-full.sh` uses too.
pub const STAND_IN: &str = include_str!("stand-in.sh");

/// A stand-in for Pi, as a POSIX shell script: it takes Pi's arguments, starts
/// or resumes the session its `--session-id` names, as Pi's session files are
/// written, with a real session's entries for what the prompt's words say. So
/// the status engine reads what Pi writes, through agent-tail's own Pi
/// adapter, with no model behind it. See `pi-stand-in.sh`, which says what it
/// does for each word, beside this file.
pub const PI_STAND_IN: &str = include_str!("pi-stand-in.sh");

/// How often a supervisor checks its agent.
const TICK: Duration = Duration::from_millis(50);

/// How long a stopping agent gets after `SIGTERM`.
const STOP_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct TestBackend {
    claude: PathBuf,
    /// What runs as Pi, if anything does, and the one agent dir its agents
    /// share.
    pi: Option<(PathBuf, PathBuf)>,
    /// With [`TestBackend::with_pi_copies`], the store.
    pi_copies: Option<crate::pi_logins::Broker>,
    pi_config: Option<PathBuf>,
    /// With [`TestBackend::with_native_pi_in`], where agents would find their
    /// `pi` on this machine.
    native_pi: Option<PathBuf>,
    /// With [`TestBackend::with_pi_layer_in`], where a VM's Pi layer would be.
    pi_layer: Option<crate::images::Images>,
    supervisors: Mutex<HashMap<String, Supervisor>>,
    /// Held to pause, to replace a supervisor and to wake, so a supervisor
    /// that was just replaced can't pause the agent a wake has continued.
    gate: Arc<Mutex<()>>,
    /// What [`Limiter::usage`] says each running agent's memory is, in
    /// place of what its processes use.
    memory: Option<u64>,
    /// How big a snapshot its sleep writes, when it sleeps to disk.
    snapshot: Option<u64>,
    capabilities: Capabilities,
}

/// The snapshot a test backend that sleeps to disk keeps, in the agent's
/// directory.
pub const TEST_SNAPSHOT: &str = "test-snapshot";

/// One agent's limits, held by a thread of its own.
#[derive(Debug)]
struct Supervisor {
    stop: Arc<AtomicBool>,
    paused: Arc<Mutex<Option<Limit>>>,
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl TestBackend {
    /// A backend whose agents run `claude` as Claude Code.
    pub fn new(claude: impl Into<PathBuf>) -> TestBackend {
        TestBackend {
            claude: claude.into(),
            pi: None,
            pi_copies: None,
            pi_config: None,
            native_pi: None,
            pi_layer: None,
            supervisors: Mutex::new(HashMap::new()),
            gate: Arc::new(Mutex::new(())),
            memory: None,
            snapshot: None,
            capabilities: Capabilities {
                sleep_frees_memory: false,
                exact_after_reboot: false,
                kernel_limits: false,
                file_containment: false,
                wake_cost: Duration::from_millis(10),
            },
        }
    }

    /// Keeps each Pi agent a private agent dir as a `linux` VM has, in step with
    /// the store in `layout` by the real code.
    pub fn with_pi_copies(
        mut self,
        pi: impl Into<PathBuf>,
        layout: &vibe_core::paths::Layout,
    ) -> TestBackend {
        self.pi = Some((pi.into(), layout.linux_pi_logins_dir()));
        self.pi_copies = Some(crate::pi_logins::Broker::of(layout));
        self.pi_config = Some(layout.linux_pi_dir());
        self
    }

    /// Runs `pi` as Pi, with `dir` as the agent dir all its agents share.
    pub fn with_pi(mut self, pi: impl Into<PathBuf>, dir: impl Into<PathBuf>) -> TestBackend {
        self.pi = Some((pi.into(), dir.into()));
        self
    }

    /// Looks for Pi on this machine as a native backend does, at `path`. What
    /// runs is still what [`TestBackend::with_pi`] says.
    pub fn with_native_pi_in(mut self, path: impl Into<PathBuf>) -> TestBackend {
        self.native_pi = Some(path.into());
        self
    }

    /// Wants a VM's Pi layer as the VM backend does, from the images in `linux`.
    pub fn with_pi_layer_in(mut self, linux: impl Into<PathBuf>) -> TestBackend {
        self.pi_layer = Some(crate::images::Images::new(linux.into()));
        self
    }

    /// Says it has `capabilities`, so policy that reads them can be tested
    /// on each kind of backend.
    pub fn with_capabilities(mut self, capabilities: Capabilities) -> TestBackend {
        self.capabilities = capabilities;
        self
    }

    /// Sleeps the way a VM does: each sleep keeps a snapshot of `bytes`
    /// (sparse) in the agent's directory, and frees the agent's memory, as
    /// its capabilities say. A snapshot dropped while it sleeps takes its
    /// processes with it, so it wakes with `claude --resume`.
    pub fn sleeping_to_disk(mut self, bytes: u64) -> TestBackend {
        self.snapshot = Some(bytes);
        self.capabilities.sleep_frees_memory = true;
        self.capabilities.exact_after_reboot = true;
        self
    }

    fn snapshot_path(h: &Handle) -> PathBuf {
        h.agent_dir.join(TEST_SNAPSHOT)
    }

    /// Reports `bytes` as each running agent's memory, so listings that
    /// show it are the same every time. Limits still see what its
    /// processes really use.
    pub fn reporting_memory(mut self, bytes: u64) -> TestBackend {
        self.memory = Some(bytes);
        self
    }

    /// As the `linux` backend readies a VM's private agent dir, with
    /// [`TestBackend::with_pi_copies`].
    fn prepare_pi_copy(&self, agent: &str, agent_dir: &Path, running: bool) -> Result<()> {
        if let Some(broker) = &self.pi_copies {
            let _ = broker.prepare_vm(
                &crate::pi_logins::vm_copy(agent, agent_dir),
                running,
                crate::pi_logins::now_ms(),
            );
        }
        Ok(())
    }

    /// The agent's session, unless its pid now names someone else's.
    fn tree(h: &Handle) -> Option<SessionTree> {
        h.session
            .map(SessionTree::new)
            .filter(SessionTree::is_shims)
    }

    fn env(h: &Handle) -> Vec<(String, String)> {
        match &h.home {
            AgentHome::Claude(c) => vec![(
                CONFIG_DIR_ENV.to_owned(),
                c.dir.to_string_lossy().into_owned(),
            )],
            AgentHome::Pi(p) => vec![(
                pi::AGENT_DIR_ENV.to_owned(),
                p.dir.to_string_lossy().into_owned(),
            )],
        }
    }

    fn unsupervise(&self, h: &Handle) {
        let _gate = lock(&self.gate);
        lock(&self.supervisors).remove(&h.agent_ref);
    }
}

impl Compartment for TestBackend {
    fn native_pi(&self) -> Option<std::result::Result<PathBuf, String>> {
        let path = self.native_pi.as_ref()?;
        Some(if path.exists() {
            Ok(path.clone())
        } else {
            Err(format!("no pi at {}", path.display()))
        })
    }

    fn pi_layer_missing(&self) -> bool {
        self.pi_layer.as_ref().is_some_and(|i| i.pi().is_none())
    }

    fn create(&self, spec: &AgentSpec) -> Result<Handle> {
        let home = match spec.agent {
            AgentKind::Claude => {
                let dir = spec.agent_dir.join("claude");
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&dir)
                    .map_err(|e| EnvError::new(format!("can't create {}: {e}", dir.display())))?;
                AgentHome::Claude(ClaudeHome::own(dir))
            }
            AgentKind::Pi => {
                let (_, dir) = self
                    .pi
                    .clone()
                    .ok_or_else(|| EnvError::new("this test backend has no Pi"))?;
                match &self.pi_config {
                    None => AgentHome::Pi(PiHome::at(dir, spec.agent_dir.join(pi::VM_SESSIONS))),
                    Some(config) => {
                        self.prepare_pi_copy(&spec.agent_ref, &spec.agent_dir, false)?;
                        AgentHome::Pi(
                            PiHome::at(
                                spec.agent_dir.join(pi::VM_AGENT_DIR),
                                spec.agent_dir.join(pi::VM_SESSIONS),
                            )
                            .with_config(config.clone()),
                        )
                    }
                }
            }
        };
        Ok(Handle {
            agent_ref: spec.agent_ref.clone(),
            agent_dir: spec.agent_dir.clone(),
            worktree: spec.worktree.clone(),
            home,
            session: None,
        })
    }

    fn run(&self, h: &Handle, r: &Run) -> Result<RunOutput> {
        // As the shim does for the agent's terminal.
        let tmp = h.agent_dir.join(vibe_core::paths::TMP_DIR);
        fs::create_dir_all(&tmp)?;
        let mut env = TestBackend::env(h);
        env.push(("TMPDIR".into(), tmp.to_string_lossy().into_owned()));
        native::run_for(h, r, &env)
    }

    fn launch(&self, h: &Handle, launch: &Launch) -> Result<TerminalEnd> {
        let program = match launch.agent {
            AgentKind::Claude => self.claude.clone(),
            AgentKind::Pi => {
                self.prepare_pi_copy(&h.agent_ref, &h.agent_dir, true)?;
                self.pi
                    .clone()
                    .ok_or_else(|| EnvError::new("this test backend has no Pi"))?
                    .0
            }
        };
        let native::AgentCommand {
            command,
            env,
            clean_env,
        } = native::agent_command(&program, h, launch, TestBackend::env(h))?;
        Ok(TerminalEnd::Pty {
            command,
            cwd: launch.cwd.clone(),
            env,
            clean_env,
        })
    }

    fn processes(&self, h: &Handle) -> Result<Vec<crate::Proc>> {
        Ok(TestBackend::tree(h)
            .map(|t| t.members())
            .unwrap_or_default())
    }

    fn stop(&self, h: &Handle) -> Result<()> {
        self.unsupervise(h);
        if let Some(tree) = TestBackend::tree(h)
            && !tree.end(STOP_GRACE)
        {
            return Err(EnvError::new(format!(
                "{}'s processes didn't end",
                h.agent_ref
            )));
        }
        // Asleep, its processes were in the snapshot: they end with it.
        match fs::remove_file(TestBackend::snapshot_path(h)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    fn delete(&self, h: &Handle) -> Result<()> {
        self.stop(h)?;
        h.home
            .forget(&h.worktree)
            .map_err(|e| EnvError::new(format!("can't remove its agent's state: {e}")))?;
        let tmp = h.agent_dir.join(vibe_core::paths::TMP_DIR);
        match fs::remove_dir_all(&tmp) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(EnvError::new(format!(
                "can't remove {}: {e}",
                tmp.display()
            ))),
            _ => Ok(()),
        }
    }
}

impl Limiter for TestBackend {
    fn apply(&self, h: &Handle, limits: &Limits) -> Result<()> {
        let tree = TestBackend::tree(h)
            .ok_or_else(|| EnvError::new(format!("{} isn't running", h.agent_ref)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(Mutex::new(None));
        let _gate = lock(&self.gate);
        {
            let (stop, paused, limits) = (Arc::clone(&stop), Arc::clone(&paused), *limits);
            let gate = Arc::clone(&self.gate);
            std::thread::Builder::new()
                .name(format!("limits {}", h.agent_ref))
                .spawn(move || supervise(tree, limits, &stop, &paused, &gate))?;
        }
        // Replacing the old supervisor stops it.
        lock(&self.supervisors).insert(h.agent_ref.clone(), Supervisor { stop, paused });
        Ok(())
    }

    fn usage(&self, h: &Handle) -> Result<Usage> {
        let procs = self.processes(h)?;
        let paused = lock(&self.supervisors)
            .get(&h.agent_ref)
            .and_then(|s| *lock(&s.paused));
        let memory = procs.iter().map(|p| p.memory).sum();
        Ok(Usage {
            memory: self.memory.filter(|_| !procs.is_empty()).unwrap_or(memory),
            cpu_ms: procs.iter().map(|p| p.cpu_ms).sum(),
            processes: procs.len() as u32,
            paused,
            ..Usage::default()
        })
    }
}

/// Pauses the tree when it's over a limit, until told to stop or the
/// session has ended.
fn supervise(
    tree: SessionTree,
    limits: Limits,
    stop: &AtomicBool,
    paused: &Mutex<Option<Limit>>,
    gate: &Mutex<()>,
) {
    while !stop.load(Ordering::SeqCst) {
        let all = tree.all();
        if all.is_empty() {
            return;
        }
        let members: Vec<_> = all.iter().filter(|p| p.pid != tree.sid).collect();
        let hit = if members.len() > limits.max_processes as usize {
            Some(Limit::Processes)
        } else if members.iter().map(|p| p.memory).sum::<u64>() > limits.memory {
            Some(Limit::Memory)
        } else {
            None
        };
        if let Some(hit) = hit {
            let _gate = lock(gate);
            if stop.load(Ordering::SeqCst) {
                return;
            }
            // Paused once it's stopped, not before.
            if tree.freeze().is_ok() {
                lock(paused).get_or_insert(hit);
            }
        }
        std::thread::sleep(TICK);
    }
}

impl Sleeper for TestBackend {
    fn sleep(&self, h: &Handle) -> Result<()> {
        let tree = TestBackend::tree(h)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| EnvError::new(format!("{} isn't running", h.agent_ref)))?;
        tree.freeze()?;
        if let Some(bytes) = self.snapshot {
            let saved =
                fs::File::create(TestBackend::snapshot_path(h)).and_then(|f| f.set_len(bytes));
            if let Err(e) = saved {
                tree.thaw();
                return Err(EnvError::new(format!("can't save {}: {e}", h.agent_ref)));
            }
        }
        Ok(())
    }

    fn wake(&self, h: &Handle) -> Result<Woke> {
        // As the `linux` backend does, before anything of it runs again.
        if let (Some(broker), AgentHome::Pi(_)) = (&self.pi_copies, &h.home) {
            let _ = broker.sync(
                &crate::pi_logins::vm_copy(&h.agent_ref, &h.agent_dir),
                crate::pi_logins::now_ms(),
            );
        }
        let Some(tree) = TestBackend::tree(h) else {
            return Ok(Woke::Resume);
        };
        // Saved to disk, it's restored from its snapshot, which is then
        // used up. One whose snapshot was dropped has no tree left.
        match fs::remove_file(TestBackend::snapshot_path(h)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        // Without its shim the agent has no terminal: its tree is hung up.
        if !tree.all().iter().any(|p| p.pid == tree.sid) {
            tree.end(STOP_GRACE);
            return Ok(Woke::Resume);
        }
        let _gate = lock(&self.gate);
        if let Some(s) = lock(&self.supervisors).get(&h.agent_ref) {
            *lock(&s.paused) = None;
        }
        tree.thaw();
        Ok(Woke::Exact)
    }

    fn sleep_size(&self, h: &Handle) -> Result<Option<u64>> {
        if self.snapshot.is_some() && TestBackend::tree(h).is_none_or(|t| t.is_empty()) {
            return Err(EnvError::new(format!("{} isn't running", h.agent_ref)));
        }
        Ok(self.snapshot)
    }

    fn saved(&self, h: &Handle) -> Option<crate::Saved> {
        self.snapshot?;
        let meta = fs::metadata(TestBackend::snapshot_path(h)).ok()?;
        Some(crate::Saved {
            bytes: meta.len(),
            at: meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
        })
    }

    fn drop_saved(&self, h: &Handle) -> Result<()> {
        match fs::remove_file(TestBackend::snapshot_path(h)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        self.unsupervise(h);
        if let Some(tree) = TestBackend::tree(h) {
            tree.end(STOP_GRACE);
        }
        Ok(())
    }
}

impl Backend for TestBackend {
    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }
}

/// A toolbox for tests and for `VIBE_TEST_BACKEND`, so `vibed`'s handling
/// of it is tested without a VM. Its shell is any command, run natively
/// under the shim's PTY; closing it keeps a new, empty template and home
/// version in `~/VibeOS/linux/`, as the real one keeps the disks it wrote.
/// Update and recipe commands run natively with `sh -c` in its directory,
/// so `true` succeeds and `exit 3` fails; a rebuild reports `uncovered`.
/// Each home it keeps holds the logins it was given ([`TestToolbox::with_logins`]).
#[derive(Debug)]
pub struct TestToolbox {
    layout: vibe_core::paths::Layout,
    images: crate::images::Images,
    shell: Vec<String>,
    uncovered: Vec<String>,
    outlives_shim: bool,
    discard_takes: Duration,
    open_takes: Duration,
    hang_up_fails: bool,
    logins: Vec<vibe_core::rpc::Login>,
    claude: bool,
}

impl TestToolbox {
    pub fn new(layout: vibe_core::paths::Layout, shell: Vec<String>) -> TestToolbox {
        TestToolbox {
            images: crate::images::Images::new(layout.linux_dir()),
            layout,
            shell,
            uncovered: Vec::new(),
            outlives_shim: false,
            discard_takes: Duration::ZERO,
            open_takes: Duration::ZERO,
            hang_up_fails: false,
            logins: Vec::new(),
            claude: false,
        }
    }

    /// The logins each home its shell saves holds, as the real toolbox's
    /// look finds them, and whether linux agents have a Claude login.
    pub fn with_logins(mut self, logins: Vec<vibe_core::rpc::Login>, claude: bool) -> TestToolbox {
        self.logins = logins;
        self.claude = claude;
        self
    }

    /// Its discard takes `how_long`, as stopping a VM an interrupted
    /// update left can, so tests can catch an open racing it.
    pub fn with_slow_discard(mut self, how_long: Duration) -> TestToolbox {
        self.discard_takes = how_long;
        self
    }

    /// Its open takes `how_long`, as a VM's boot does, so tests can catch
    /// what happens during one.
    pub fn with_slow_open(mut self, how_long: Duration) -> TestToolbox {
        self.open_takes = how_long;
        self
    }

    /// Hanging up on its shell fails, as a `kill` does in the moment after
    /// the shell has exited on its own.
    pub fn with_failing_hang_up(mut self) -> TestToolbox {
        self.hang_up_fails = true;
        self
    }

    /// Its shell counts as running while it's open, as a VM's shell runs
    /// on when its shim dies; opening it again then starts the shell anew,
    /// standing in for a new shim on the same one.
    pub fn outliving_its_shim(mut self) -> TestToolbox {
        self.outlives_shim = true;
        self
    }

    /// What a rebuild says the recipe doesn't cover.
    pub fn with_uncovered(mut self, uncovered: Vec<String>) -> TestToolbox {
        self.uncovered = uncovered;
        self
    }

    fn dir(&self) -> PathBuf {
        self.layout.agent_dir(crate::toolbox::TOOLBOX_REF)
    }

    fn marker(&self) -> PathBuf {
        self.dir().join("open.json")
    }

    fn keep(&self, kind: crate::images::Kind, base: Option<u32>) -> Result<u32> {
        let working = self.dir().join(format!("{kind:?}.img"));
        fs::write(&working, "").map_err(|e| EnvError::new(e.to_string()))?;
        let (n, _) = self.images.keep(kind, &working, base)?;
        Ok(n)
    }

    fn base(&self) -> Option<u32> {
        crate::toolbox::ToolboxHost::versions(self).base
    }

    fn sh(&self, what: &str, command: &str) -> Result<()> {
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .current_dir(self.dir())
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| EnvError::new(format!("{what}: {e}")))?;
        match out.status.code() {
            Some(0) => Ok(()),
            Some(code) => Err(EnvError::new(format!("{what} exited with code {code}"))),
            None => Err(EnvError::new(format!("{what} was ended by a signal"))),
        }
    }
}

impl crate::toolbox::ToolboxHost for TestToolbox {
    fn versions(&self) -> crate::toolbox::Versions {
        use crate::images::Kind;
        let template = self.images.newest_of(Kind::Template);
        crate::toolbox::Versions {
            base: match &template {
                Some((_, t)) => self.images.base_of(t).map(|b| b.version),
                None => self.images.base().map(|b| b.version),
            },
            template: template.map(|(n, _)| n),
            home: self.images.newest_of(Kind::Home).map(|(n, _)| n),
            newest_base: self.images.base().map(|b| b.version),
        }
    }

    fn open(&self) -> Result<TerminalEnd> {
        std::thread::sleep(self.open_takes);
        let dir = self.dir();
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .and_then(|()| fs::write(self.marker(), "{}"))
            .map_err(|e| EnvError::new(format!("can't open the test toolbox: {e}")))?;
        Ok(TerminalEnd::Pty {
            command: self.shell.clone(),
            cwd: dir,
            env: Vec::new(),
            clean_env: false,
        })
    }

    fn is_open(&self) -> bool {
        self.marker().exists()
    }

    /// Its shell runs under the shim, so it's gone with the shim, unless
    /// it stands in for a VM's ([`TestToolbox::outliving_its_shim`]).
    fn shell_running(&self) -> bool {
        self.outlives_shim && self.is_open()
    }

    /// Its shell runs here, under the shim's PTY, leading its own group.
    fn hang_up(&self, shell_pid: u32) -> Result<()> {
        if !self.is_open() {
            return Err(EnvError::new("no toolbox shell is open"));
        }
        if self.hang_up_fails {
            return Err(EnvError::new("kill: No such process"));
        }
        let group = libc::pid_t::try_from(shell_pid)
            .ok()
            .filter(|&p| p > 1)
            .ok_or_else(|| EnvError::new(format!("{shell_pid} isn't the shell's pid")))?;
        // SAFETY: kill has no memory effects; a negative pid names the group.
        if unsafe { libc::kill(-group, libc::SIGHUP) } == -1 {
            return Err(EnvError::new(format!(
                "can't hang up on the test toolbox's shell: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    fn close(&self) -> Result<crate::toolbox::Saved> {
        use crate::images::Kind;
        if !self.is_open() {
            return Err(EnvError::new("no toolbox shell is open"));
        }
        let _ = fs::remove_file(self.marker());
        let base = self
            .base()
            .ok_or_else(|| EnvError::new("no Linux base image yet"))?;
        let template = self.keep(Kind::Template, Some(base))?;
        let working = self.dir().join("Home.img");
        fs::write(&working, "").map_err(|e| EnvError::new(e.to_string()))?;
        let (home, _) = self.images.keep_home(&working, Some(&self.logins))?;
        Ok(crate::toolbox::Saved {
            template: Some(template),
            home: Some(home),
            uncovered: Vec::new(),
        })
    }

    fn discard(&self) -> Result<()> {
        std::thread::sleep(self.discard_takes);
        match fs::remove_file(self.marker()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(EnvError::new(e.to_string())),
            _ => Ok(()),
        }
    }

    fn update(&self, commands: &[String]) -> Result<crate::toolbox::Saved> {
        fs::create_dir_all(self.dir()).map_err(|e| EnvError::new(e.to_string()))?;
        for command in commands {
            self.sh(&format!("`{command}`"), command)?;
        }
        let base = self
            .base()
            .ok_or_else(|| EnvError::new("no Linux base image yet"))?;
        Ok(crate::toolbox::Saved {
            template: Some(self.keep(crate::images::Kind::Template, Some(base))?),
            ..crate::toolbox::Saved::default()
        })
    }

    fn rebuild(
        &self,
        recipe: &vibe_core::toolbox::Recipe,
        whole_only: bool,
    ) -> Result<crate::toolbox::Saved> {
        fs::create_dir_all(self.dir()).map_err(|e| EnvError::new(e.to_string()))?;
        for install in &recipe.install {
            self.sh(&format!("installing {}", install.name), &install.run)?;
        }
        if whole_only && !self.uncovered.is_empty() {
            return Ok(crate::toolbox::Saved {
                template: None,
                home: None,
                uncovered: self.uncovered.clone(),
            });
        }
        let newest = self
            .images
            .base()
            .ok_or_else(|| EnvError::new("no Linux base image yet"))?;
        Ok(crate::toolbox::Saved {
            template: Some(self.keep(crate::images::Kind::Template, Some(newest.version))?),
            home: None,
            uncovered: self.uncovered.clone(),
        })
    }

    fn log_in_to_github(&self, token: &str) -> Result<crate::toolbox::Saved> {
        fs::create_dir_all(self.dir()).map_err(|e| EnvError::new(e.to_string()))?;
        fs::write(self.dir().join("github-token"), token)
            .map_err(|e| EnvError::new(e.to_string()))?;
        Ok(crate::toolbox::Saved {
            home: Some(self.keep(crate::images::Kind::Home, None)?),
            ..Default::default()
        })
    }

    fn logins(&self) -> vibe_core::rpc::ToolboxLogins {
        let claude = if self.claude {
            Ok(())
        } else {
            Err("linux agents have no Claude login yet: Settings → Linux Toolbox signs in with one click".into())
        };
        crate::toolbox::home_logins(&self.images, claude)
    }
}
