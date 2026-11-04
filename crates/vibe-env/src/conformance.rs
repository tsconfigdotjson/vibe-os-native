//! The conformance suite every backend passes (PRD: Environment backends):
//!
//! - start, then its processes are visible;
//! - a command runs to completion inside the compartment;
//! - a limit hit pauses it;
//! - sleep then wake leaves the session exactly where it was;
//! - after a reboot, wake is exact or reports `--resume`, as
//!   `exact_after_reboot` says;
//! - delete leaves nothing behind.
//!
//! A backend's tests implement [`Harness`] and call [`check_all`], or each
//! check on its own. The agent's "Claude" is
//! [`STAND_IN`](crate::testing::STAND_IN), which the harness installs as
//! its backend's `claude`; it runs on a real `vibe-shim`, as in `vibed`.
//! Each check panics with what went wrong.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use vibe_core::shim::{self, Frame, FrameKind};

use crate::tree::SessionTree;
use crate::{
    AgentSpec, Backend, Compartment, Handle, Launch, Limit, Limiter, Limits, Run, Sleeper,
    TerminalEnd, Woke,
};

/// How long anything may take to show up.
const PATIENCE: Duration = Duration::from_secs(30);

/// A limit to hit, and how.
#[derive(Debug, Clone)]
pub struct LimitCase {
    /// The stand-in's prompt, which goes over the limit once it runs.
    pub prompt: String,
    pub limits: Limits,
    /// The limit the backend should say it hit.
    pub expect: Limit,
    /// Limits the agent is back under.
    pub raised: Limits,
}

/// What the suite needs from a backend's tests.
pub trait Harness {
    type B: Backend;

    fn backend(&self) -> &Self::B;

    /// A spec for a new agent `agent_ref`, whose worktree and agent
    /// directory exist. Each call gets a fresh one.
    fn spec(&self, agent_ref: &str) -> AgentSpec;

    /// The `vibe-shim` to hold terminals with.
    fn shim(&self) -> PathBuf;

    /// A short directory, outside [`Harness::roots`], for the shims'
    /// sockets, logs and records.
    fn scratch(&self) -> PathBuf;

    /// The directories a delete must leave as it found them: wherever the
    /// backend keeps anything, Claude's home included.
    fn roots(&self) -> Vec<PathBuf>;

    /// What a host reboot does to a running agent. By default every
    /// process of its session ends, the shim's too.
    fn reboot(&self, h: &Handle) {
        if let Some(sid) = h.session {
            SessionTree::new(sid).end(Duration::from_secs(5));
        }
    }

    /// The limit [`limit_hit_pauses`] hits. By default the process count:
    /// twenty sleeps against a limit of eight.
    fn limit_case(&self) -> LimitCase {
        let limits = Limits {
            memory: 1 << 30,
            cpu_cores: 1,
            max_processes: 8,
            disk_soft: 1 << 30,
            disk_hard: 1 << 31,
        };
        LimitCase {
            prompt: "fork 20".into(),
            limits,
            expect: Limit::Processes,
            raised: Limits {
                max_processes: 1000,
                ..limits
            },
        }
    }
}

/// Runs every check.
pub fn check_all<H: Harness>(h: &H) {
    start_shows_processes(h);
    run_in_compartment(h);
    limit_hit_pauses(h);
    sleep_then_wake_is_exact(h);
    reboot_then_wake(h);
    delete_leaves_nothing(h);
}

/// An agent the suite started; deleted when dropped, whatever happened.
struct Agent<'a, H: Harness> {
    h: &'a H,
    handle: Handle,
    socket: PathBuf,
    deleted: bool,
}

impl<H: Harness> Drop for Agent<'_, H> {
    fn drop(&mut self) {
        if !self.deleted {
            let _ = self.h.backend().delete(&self.handle);
            if let Some(sid) = self.handle.session {
                SessionTree::new(sid).end(Duration::from_secs(2));
            }
        }
    }
}

/// The stand-in's arguments for `prompt`, as `vibed` gives Claude its own.
fn args(prompt: &str) -> Vec<String> {
    [
        "--session-id",
        "0b5c3f6e-c0f0-4000-8000-000000000001",
        "--model",
        "opus",
        "--",
        prompt,
    ]
    .map(str::to_owned)
    .to_vec()
}

impl<'a, H: Harness> Agent<'a, H> {
    /// Creates `agent_ref`'s compartment and launches the stand-in on
    /// `prompt`, on a shim.
    fn start(h: &'a H, agent_ref: &str, prompt: &str) -> Agent<'a, H> {
        let spec = h.spec(agent_ref);
        Agent::start_from(h, &spec, prompt)
    }

    fn start_from(h: &'a H, spec: &AgentSpec, prompt: &str) -> Agent<'a, H> {
        let handle = h
            .backend()
            .create(spec)
            .unwrap_or_else(|e| panic!("create {}: {e}", spec.agent_ref));
        let socket = h.scratch().join(format!("{}.sock", spec.agent_ref));
        let mut agent = Agent {
            h,
            handle,
            socket,
            deleted: false,
        };
        agent.launch(prompt);
        agent
    }

    fn launch(&mut self, prompt: &str) {
        let end = self
            .h
            .backend()
            .launch(
                &self.handle,
                &Launch {
                    agent: Default::default(),
                    args: args(prompt),
                    env: vec![("VIBE_CONFORMANCE".into(), "1".into())],
                    cwd: self.handle.worktree.clone(),
                },
            )
            .unwrap_or_else(|e| panic!("launch: {e}"));
        self.hold(&end);
    }

    /// Starts a shim on `end`, as `vibed` does.
    fn hold(&mut self, end: &TerminalEnd) {
        let r = &self.handle.agent_ref;
        let mut cmd = Command::new(self.h.shim());
        cmd.args(["--ref", r, "--socket"])
            .arg(&self.socket)
            .arg("--state")
            .arg(self.h.scratch().join(r))
            .args(["--size", "100x30"]);
        match end {
            TerminalEnd::Pty {
                command, cwd, env, ..
            } => {
                cmd.arg("--cwd")
                    .arg(cwd)
                    .arg("--")
                    .args(command)
                    .envs(env.iter().map(|(k, v)| (k, v)));
            }
            TerminalEnd::Vm { socket } => {
                cmd.arg("--vm").arg(socket);
            }
        }
        let out = cmd.output().expect("vibe-shim runs");
        assert!(
            out.status.success(),
            "vibe-shim: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let pid = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("vibe-shim prints its pid");
        self.handle.session = Some(pid);
    }

    fn connect(&self) -> shim::Connection {
        shim::connect(
            &self.socket,
            &shim::control_hello("conformance"),
            Duration::from_secs(10),
        )
        .unwrap_or_else(|e| panic!("the session of {}: {e}", self.handle.agent_ref))
    }

    fn screen(&self) -> String {
        self.connect()
            .read_output(None)
            .expect("the shim answers")
            .screen
    }

    fn wait_screen(&self, text: &str) {
        wait(&format!("{text:?} on the screen"), || {
            self.screen().contains(text)
        });
    }

    fn type_line(&self, line: &str) {
        let mut c = self.connect();
        shim::write_frame(
            &mut c.stream,
            &Frame::new(FrameKind::Input, format!("{line}\r")),
        )
        .unwrap();
        c.stream.flush().unwrap();
        // Read a reply, so the input is in before this connection closes.
        c.status().unwrap();
    }

    fn shim_alive(&self) -> bool {
        self.handle
            .session
            .is_some_and(|sid| SessionTree::new(sid).all().iter().any(|p| p.pid == sid))
    }

    fn ticks(&self) -> Option<u64> {
        fs::read_to_string(self.handle.worktree.join(".vibe-ticks"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    fn delete(mut self) {
        self.h
            .backend()
            .delete(&self.handle)
            .unwrap_or_else(|e| panic!("delete: {e}"));
        self.deleted = true;
        if let Some(sid) = self.handle.session {
            let tree = SessionTree::new(sid);
            assert!(
                tree.wait_empty(PATIENCE),
                "processes left after delete: {:?}",
                tree.all()
            );
        }
        let _ = fs::remove_dir_all(self.h.scratch().join(&self.handle.agent_ref));
    }
}

fn wait(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Start, then its processes are visible; stop ends them, and the
/// compartment launches again.
pub fn start_shows_processes<H: Harness>(h: &H) {
    let mut a = Agent::start(h, "conf-start", "hello there");
    a.wait_screen("Working on: hello there");
    let sid = a.handle.session.unwrap();
    let procs = h.backend().processes(&a.handle).unwrap();
    assert!(!procs.is_empty(), "no processes for a running agent");
    assert!(
        procs.iter().all(|p| p.pid != sid),
        "the shim isn't one of the agent's processes: {procs:?}"
    );
    let usage = h.backend().usage(&a.handle).unwrap();
    assert!(usage.processes >= 1, "{usage:?}");
    assert_eq!(usage.paused, None);
    let caps = h.backend().capabilities();
    assert!(caps.wake_cost < Duration::from_secs(60), "{caps:?}");

    h.backend().stop(&a.handle).unwrap();
    wait("the shim to go", || !a.shim_alive());
    assert!(
        h.backend()
            .processes(&a.handle)
            .map(|p| p.is_empty())
            .unwrap_or(true)
    );
    a.launch("again");
    a.wait_screen("Working on: again");
    a.delete();
}

/// A command runs to completion inside the compartment, in its directory,
/// with its environment, and is killed when it runs out of time.
pub fn run_in_compartment<H: Harness>(h: &H) {
    let spec = h.spec("conf-run");
    let backend = h.backend();
    let handle = backend.create(&spec).unwrap();
    let run = |script: &str, timeout| {
        backend
            .run(
                &handle,
                &Run {
                    argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
                    env: vec![("VIBE_X".into(), "x".into())],
                    cwd: spec.worktree.clone(),
                    timeout,
                },
            )
            .unwrap()
    };
    let out = run(
        "echo \"$VIBE_X in $(pwd -P)\"; echo oops >&2; exit 3",
        PATIENCE,
    );
    assert_eq!(out.code, Some(3), "{out:?}");
    let text = String::from_utf8_lossy(&out.output);
    let cwd = spec.worktree.canonicalize().unwrap();
    assert!(
        text.contains(&format!("x in {}", cwd.display())),
        "{text:?}"
    );
    assert!(text.contains("oops"), "stderr too: {text:?}");
    assert!(run("true", PATIENCE).success());
    let started = Instant::now();
    let out = run("sleep 60", Duration::from_millis(500));
    assert!(out.timed_out && !out.success(), "{out:?}");
    assert!(started.elapsed() < PATIENCE);
    backend.delete(&handle).unwrap();
}

/// A limit hit pauses the agent: it stops answering, until the limit is
/// raised and it's woken.
pub fn limit_hit_pauses<H: Harness>(h: &H) {
    let case = h.limit_case();
    let a = Agent::start(h, "conf-limit", &case.prompt);
    a.wait_screen("Ready.");
    h.backend().apply(&a.handle, &case.limits).unwrap();
    wait(&format!("a {:?} pause", case.expect), || {
        h.backend().usage(&a.handle).unwrap().paused == Some(case.expect)
    });
    // Paused, it doesn't answer.
    a.type_line("ping");
    std::thread::sleep(Duration::from_millis(500));
    assert!(!a.screen().contains("> ping"), "a paused agent answered");
    h.backend().apply(&a.handle, &case.raised).unwrap();
    let woke = h.backend().wake(&a.handle).unwrap();
    assert_eq!(woke, Woke::Exact, "a paused agent resumes exactly");
    a.wait_screen("> ping");
    assert_eq!(h.backend().usage(&a.handle).unwrap().paused, None);
    a.delete();
}

/// Sleep then wake leaves the session exactly where it was: nothing runs
/// while it sleeps, and its question is still waiting after.
pub fn sleep_then_wake_is_exact<H: Harness>(h: &H) {
    let mut a = Agent::start(h, "conf-sleep", "tick question");
    a.wait_screen("Red or blue?");
    wait("ticks", || a.ticks().is_some());
    h.backend().sleep(&a.handle).unwrap();
    // Asleep, it doesn't count on.
    std::thread::sleep(Duration::from_millis(300));
    let asleep = a.ticks();
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(a.ticks(), asleep, "it kept running while asleep");

    match h.backend().wake(&a.handle).unwrap() {
        Woke::Exact => {}
        Woke::Reattach(end) => a.hold(&end),
        Woke::Resume => panic!("a wake from sleep is exact, not a resume"),
    }
    wait("ticks again", || a.ticks() > asleep);
    a.wait_screen("Red or blue?");
    a.type_line("red");
    a.wait_screen("You chose red.");
    a.delete();
}

/// After a reboot, a sleeping agent wakes exactly where it was if the
/// backend says so, and reports `--resume` otherwise.
pub fn reboot_then_wake<H: Harness>(h: &H) {
    let mut a = Agent::start(h, "conf-reboot", "question");
    a.wait_screen("Red or blue?");
    h.backend().sleep(&a.handle).unwrap();
    h.reboot(&a.handle);
    let woke = h.backend().wake(&a.handle).unwrap();
    if h.backend().capabilities().exact_after_reboot {
        match woke {
            Woke::Exact => {}
            Woke::Reattach(end) => a.hold(&end),
            Woke::Resume => panic!("exact_after_reboot, but the wake is a resume"),
        }
        a.wait_screen("Red or blue?");
    } else {
        assert_eq!(woke, Woke::Resume, "no exact_after_reboot, so --resume");
        assert!(!a.shim_alive());
        // The compartment is still there, to resume in.
        a.launch("resumed");
        a.wait_screen("Working on: resumed");
    }
    a.delete();
}

/// Delete leaves nothing behind: no processes, and the backend's
/// directories as they were before the agent, after an agent that ran,
/// wrote its transcript, was limited, slept and woke.
pub fn delete_leaves_nothing<H: Harness>(h: &H) {
    let spec = h.spec("conf-delete");
    let before = snapshot(&h.roots());
    let mut a = Agent::start_from(h, &spec, "fork 2 question");
    a.wait_screen("Red or blue?");
    let transcript = a
        .handle
        .claude()
        .unwrap()
        .transcript(&a.handle.worktree, "0b5c3f6e-c0f0-4000-8000-000000000001");
    wait("the transcript", || transcript.is_file());
    a.handle
        .claude()
        .unwrap()
        .prefill(&a.handle.worktree, false)
        .unwrap();
    h.backend()
        .run(
            &a.handle,
            &Run {
                argv: vec!["/bin/sh".into(), "-c".into(), "echo setup".into()],
                env: vec![],
                cwd: spec.worktree.clone(),
                timeout: PATIENCE,
            },
        )
        .unwrap();
    h.backend()
        .apply(&a.handle, &h.limit_case().raised)
        .unwrap();
    h.backend().sleep(&a.handle).unwrap();
    if let Woke::Reattach(end) = h.backend().wake(&a.handle).unwrap() {
        a.hold(&end);
    }
    a.type_line("blue");
    a.wait_screen("You chose blue.");
    let sid = a.handle.session.unwrap();
    a.delete();
    assert!(SessionTree::new(sid).is_empty());
    let after = snapshot(&h.roots());
    let added: Vec<_> = after.difference(&before).collect();
    let removed: Vec<_> = before.difference(&after).collect();
    assert!(
        added.is_empty() && removed.is_empty(),
        "delete left {added:?} and took {removed:?}"
    );
}

/// Every path under `roots`, with whether it's a directory.
fn snapshot(roots: &[PathBuf]) -> BTreeSet<(PathBuf, bool)> {
    fn walk(dir: &Path, out: &mut BTreeSet<(PathBuf, bool)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for e in entries.filter_map(Result::ok) {
            let path = e.path();
            let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
            out.insert((path.clone(), is_dir));
            if is_dir {
                walk(&path, out);
            }
        }
    }
    let mut out = BTreeSet::new();
    for root in roots {
        walk(root, &mut out);
    }
    out
}
