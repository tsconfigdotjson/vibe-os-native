//! A backend for tests, so the core and the conformance suite don't wait on
//! the real backends (#9, #11, #17). Never used for real agents.
//!
//! It runs the agent natively under the shim's PTY, like `macos` does,
//! with a Claude home of its own, so nothing touches the real `~/.claude`.
//! Its "Claude" is any program, normally [`STAND_IN`]. A supervisor thread
//! per agent holds it to its memory and process limits by pausing it, and
//! sleep is `SIGSTOP` on its tree.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::claude::{CONFIG_DIR_ENV, ClaudeHome};
use crate::tree::SessionTree;
use crate::{
    AgentSpec, Backend, Capabilities, Compartment, EnvError, Handle, Launch, Limit, Limiter,
    Limits, Result, Run, RunOutput, Sleeper, TerminalEnd, Usage, Woke, native,
};

/// A stand-in for Claude Code, as a POSIX shell script. It reads Claude's
/// arguments, writes a line to the transcript where Claude would, and then
/// does what the prompt's words say:
///
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
/// - anything else: says what it's working on.
///
/// Then it prints "Ready." and echoes each line it's sent until its
/// terminal hangs up. Resumed with `--resume`, it says so first. It's
/// `stand-in.sh` beside this file, which `scripts/check-full.sh` uses too.
pub const STAND_IN: &str = include_str!("stand-in.sh");

/// How often a supervisor checks its agent.
const TICK: Duration = Duration::from_millis(50);

/// How long a stopping agent gets after `SIGTERM`.
const STOP_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct TestBackend {
    claude: PathBuf,
    supervisors: Mutex<HashMap<String, Supervisor>>,
    /// Held to pause, to replace a supervisor and to wake, so a supervisor
    /// that was just replaced can't pause the agent a wake has continued.
    gate: Arc<Mutex<()>>,
}

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
            supervisors: Mutex::new(HashMap::new()),
            gate: Arc::new(Mutex::new(())),
        }
    }

    /// The agent's session, unless its pid now names someone else's.
    fn tree(h: &Handle) -> Option<SessionTree> {
        h.session
            .map(SessionTree::new)
            .filter(SessionTree::is_shims)
    }

    fn env(h: &Handle) -> Vec<(String, String)> {
        vec![(
            CONFIG_DIR_ENV.to_owned(),
            h.claude.dir.to_string_lossy().into_owned(),
        )]
    }

    fn unsupervise(&self, h: &Handle) {
        let _gate = lock(&self.gate);
        lock(&self.supervisors).remove(&h.agent_ref);
    }
}

impl Compartment for TestBackend {
    fn create(&self, spec: &AgentSpec) -> Result<Handle> {
        let dir = spec.agent_dir.join("claude");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|e| EnvError::new(format!("can't create {}: {e}", dir.display())))?;
        Ok(Handle {
            agent_ref: spec.agent_ref.clone(),
            agent_dir: spec.agent_dir.clone(),
            worktree: spec.worktree.clone(),
            claude: ClaudeHome::own(dir),
            session: None,
        })
    }

    fn run(&self, h: &Handle, r: &Run) -> Result<RunOutput> {
        // As the shim does for the agent's terminal.
        let tmp = h.agent_dir.join(vibe_core::paths::TMP_DIR);
        fs::create_dir_all(&tmp)?;
        let mut env = TestBackend::env(h);
        env.push(("TMPDIR".into(), tmp.to_string_lossy().into_owned()));
        native::run(r, &env)
    }

    fn launch(&self, h: &Handle, launch: &Launch) -> Result<TerminalEnd> {
        let mut command = vec![self.claude.to_string_lossy().into_owned()];
        command.extend(launch.args.iter().cloned());
        let mut env = TestBackend::env(h);
        env.extend(launch.env.iter().cloned());
        Ok(TerminalEnd::Pty {
            command,
            cwd: launch.cwd.clone(),
            env,
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
        Ok(())
    }

    fn delete(&self, h: &Handle) -> Result<()> {
        self.stop(h)?;
        h.claude
            .forget(&h.worktree)
            .map_err(|e| EnvError::new(format!("can't remove Claude's state: {e}")))?;
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
        Ok(Usage {
            memory: procs.iter().map(|p| p.memory).sum(),
            cpu_ms: procs.iter().map(|p| p.cpu_ms).sum(),
            processes: procs.len() as u32,
            paused,
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
        Ok(())
    }

    fn wake(&self, h: &Handle) -> Result<Woke> {
        let Some(tree) = TestBackend::tree(h) else {
            return Ok(Woke::Resume);
        };
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
}

impl Backend for TestBackend {
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

/// A toolbox for tests and for `VIBE_TEST_BACKEND`, so `vibed`'s handling
/// of it is tested without a VM. Its shell is any command, run natively
/// under the shim's PTY; closing it keeps a new, empty template and home
/// version in `~/VibeOS/linux/`, as the real one keeps the disks it wrote.
/// Update and recipe commands run natively with `sh -c` in its directory,
/// so `true` succeeds and `exit 3` fails; a rebuild reports `uncovered`.
#[derive(Debug)]
pub struct TestToolbox {
    layout: vibe_core::paths::Layout,
    images: crate::images::Images,
    shell: Vec<String>,
    uncovered: Vec<String>,
    outlives_shim: bool,
}

impl TestToolbox {
    pub fn new(layout: vibe_core::paths::Layout, shell: Vec<String>) -> TestToolbox {
        TestToolbox {
            images: crate::images::Images::new(layout.linux_dir()),
            layout,
            shell,
            uncovered: Vec::new(),
            outlives_shim: false,
        }
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

    fn close(&self) -> Result<crate::toolbox::Saved> {
        use crate::images::Kind;
        if !self.is_open() {
            return Err(EnvError::new("no toolbox shell is open"));
        }
        let _ = fs::remove_file(self.marker());
        let base = self
            .base()
            .ok_or_else(|| EnvError::new("no Linux base image yet"))?;
        Ok(crate::toolbox::Saved {
            template: Some(self.keep(Kind::Template, Some(base))?),
            home: Some(self.keep(Kind::Home, None)?),
            uncovered: Vec::new(),
        })
    }

    fn discard(&self) -> Result<()> {
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

    fn rebuild(&self, recipe: &vibe_core::toolbox::Recipe) -> Result<crate::toolbox::Saved> {
        fs::create_dir_all(self.dir()).map_err(|e| EnvError::new(e.to_string()))?;
        for install in &recipe.install {
            self.sh(&format!("installing {}", install.name), &install.run)?;
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
}
