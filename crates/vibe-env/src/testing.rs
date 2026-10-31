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
