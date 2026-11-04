//! The `macos` backend: the conformance suite (`vibe_env::conformance`), one
//! test per check, and what its governor does (PRD: How macos is governed):
//! a fork bomb is paused, a 200 MB allocation shows up in the agent's
//! memory, a `nohup` child is still counted once it's reparented, and CPU,
//! memory and disk are held to their limits.
//!
//! The backend builds for every Unix, so these run on Linux too, where
//! `PRIO_DARWIN_BG` has no counterpart: the background step is checked on a
//! Mac only (`scripts/check-full.sh` runs this file there).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use vibe_core::limits::HostLimits;
use vibe_env::conformance::{self, Harness};
use vibe_env::macos::{Intervals, Macos, STATE_FILE};
use vibe_env::testing::STAND_IN;
use vibe_env::tree::SessionTree;
use vibe_env::{
    AgentSpec, Compartment, Handle, Launch, Limit, Limiter, Limits, Proc, Sleeper, TerminalEnd,
    Woke,
};

const MIB: u64 = 1 << 20;

/// One test at a time, whatever the test threads: they time what the
/// governor does, and one spins two cores.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// A "Claude" that runs its prompt as a shell script, then echoes what it's
/// sent, for the governor's own tests.
const SCRIPT: &str = r#"#!/bin/sh
while [ $# -gt 0 ]; do case $1 in --) shift; break ;; *) shift ;; esac; done
eval "$*"
echo Ready.
while read -r line; do echo "> $line"; done
"#;

struct MacHarness {
    backend: Macos,
    /// Under /tmp, as a socket's path in it must fit in 104 bytes on macOS.
    dir: tempfile::TempDir,
}

fn install(path: &PathBuf, body: &str) {
    // Written beside its place and renamed in, so no test thread forks
    // while it's open for writing ("text file busy" on Linux).
    let tmp = path.with_extension("new");
    fs::write(&tmp, body).unwrap();
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&tmp, path).unwrap();
}

impl MacHarness {
    /// With the stand-in for Claude Code, as the conformance suite wants.
    fn new() -> MacHarness {
        MacHarness::with(STAND_IN, Intervals::default())
    }

    /// With `claude` as Claude Code, governing at `intervals`.
    fn with(claude: &str, intervals: Intervals) -> MacHarness {
        let dir = tempfile::Builder::new()
            .prefix("mac")
            .tempdir_in("/tmp")
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        install(&bin.join("claude"), claude);
        // The owner's home, as a Mac has it: Claude's state is there before
        // any agent, and is the Mac's own, not the agent's.
        let home = root.join("home");
        fs::create_dir_all(home.join(".claude/projects")).unwrap();
        fs::write(home.join(".claude.json"), "{}").unwrap();
        fs::create_dir_all(root.join("scratch")).unwrap();
        fs::set_permissions(root.join("scratch"), fs::Permissions::from_mode(0o700)).unwrap();
        let backend = Macos::new(&home)
            .with_claude(bin.join("claude"))
            .with_path(std::env::var("PATH").unwrap())
            .with_intervals(intervals);
        MacHarness { backend, dir }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }
}

impl Harness for MacHarness {
    type B = Macos;

    fn backend(&self) -> &Macos {
        &self.backend
    }

    fn spec(&self, agent_ref: &str) -> AgentSpec {
        let root = self.root();
        let worktree = root.join("worktrees").join(agent_ref);
        let agent_dir = root.join("agents").join(agent_ref);
        for d in [&worktree, &agent_dir] {
            fs::create_dir_all(d).unwrap();
        }
        AgentSpec {
            agent: Default::default(),
            agent_ref: agent_ref.into(),
            project: "web".into(),
            worktree,
            git_dir: root.join("web/.git"),
            agent_dir,
            limits: Limits::new(1 << 30, &HostLimits::default()),
        }
    }

    fn shim(&self) -> PathBuf {
        // Built beside the tests by `cargo test --workspace`.
        let exe = std::env::current_exe().unwrap();
        let path = exe.parent().unwrap().parent().unwrap().join("vibe-shim");
        assert!(
            path.is_file(),
            "{} isn't built; run `cargo build --workspace` first",
            path.display()
        );
        path
    }

    fn scratch(&self) -> PathBuf {
        self.root().join("scratch")
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root().join("agents"), self.root().join("home")]
    }
}

#[test]
fn start_shows_processes() {
    let _one = serial();
    conformance::start_shows_processes(&MacHarness::new());
}

#[test]
fn run_in_compartment() {
    let _one = serial();
    conformance::run_in_compartment(&MacHarness::new());
}

#[test]
fn limit_hit_pauses() {
    let _one = serial();
    conformance::limit_hit_pauses(&MacHarness::new());
}

#[test]
fn sleep_then_wake_is_exact() {
    let _one = serial();
    conformance::sleep_then_wake_is_exact(&MacHarness::new());
}

#[test]
fn reboot_then_wake() {
    let _one = serial();
    conformance::reboot_then_wake(&MacHarness::new());
}

#[test]
fn delete_leaves_nothing() {
    let _one = serial();
    conformance::delete_leaves_nothing(&MacHarness::new());
}

#[test]
fn a_memory_limit_pauses_too() {
    let _one = serial();
    struct Memory(MacHarness);
    impl Harness for Memory {
        type B = Macos;
        fn backend(&self) -> &Macos {
            self.0.backend()
        }
        fn spec(&self, r: &str) -> AgentSpec {
            self.0.spec(r)
        }
        fn shim(&self) -> PathBuf {
            self.0.shim()
        }
        fn scratch(&self) -> PathBuf {
            self.0.scratch()
        }
        fn roots(&self) -> Vec<PathBuf> {
            self.0.roots()
        }
        fn limit_case(&self) -> conformance::LimitCase {
            let mut case = self.0.limit_case();
            case.prompt = "fork 2".into();
            case.limits.memory = 1024;
            case.expect = Limit::Memory;
            case.raised.memory = 1 << 40;
            case
        }
    }
    conformance::limit_hit_pauses(&Memory(MacHarness::new()));
}

#[test]
fn the_backend_says_what_it_can_do() {
    let _one = serial();
    let caps = vibe_env::Backend::capabilities(MacHarness::new().backend());
    assert!(!caps.sleep_frees_memory, "macOS may swap, but keeps it");
    assert!(!caps.exact_after_reboot, "after a reboot it's --resume");
    assert!(!caps.kernel_limits, "a governor supervises");
    assert!(!caps.file_containment, "no file sandbox");
}

/// An agent started on the script "Claude", ended when dropped.
struct Agent<'a> {
    h: &'a MacHarness,
    handle: Handle,
}

impl Drop for Agent<'_> {
    fn drop(&mut self) {
        let _ = self.h.backend.delete(&self.handle);
        if let Some(sid) = self.handle.session {
            SessionTree::with_descendants(sid).end(Duration::from_secs(2));
        }
    }
}

/// Creates `agent_ref`'s compartment on `backend` and runs `script` in it,
/// on a shim, as `vibed` does.
fn start_on(backend: &Macos, h: &MacHarness, agent_ref: &str, script: &str) -> Handle {
    let spec = h.spec(agent_ref);
    let mut handle = backend.create(&spec).unwrap();
    let end = backend
        .launch(
            &handle,
            &Launch {
                agent: Default::default(),
                args: vec!["--".into(), script.into()],
                env: Vec::new(),
                cwd: spec.worktree.clone(),
            },
        )
        .unwrap();
    let TerminalEnd::Pty {
        command, cwd, env, ..
    } = end
    else {
        panic!("a macos agent runs on a PTY");
    };
    let out = Command::new(h.shim())
        .args(["--ref", agent_ref, "--socket"])
        .arg(h.scratch().join(format!("{agent_ref}.sock")))
        .arg("--state")
        .arg(&spec.agent_dir)
        .arg("--cwd")
        .arg(cwd)
        .arg("--")
        .args(command)
        .envs(env)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    handle.session = Some(String::from_utf8_lossy(&out.stdout).trim().parse().unwrap());
    handle
}

impl<'a> Agent<'a> {
    fn start(h: &'a MacHarness, agent_ref: &str, script: &str) -> Agent<'a> {
        Agent {
            h,
            handle: start_on(&h.backend, h, agent_ref, script),
        }
    }

    fn procs(&self) -> Vec<Proc> {
        self.h.backend.processes(&self.handle).unwrap()
    }

    fn usage(&self) -> vibe_env::Usage {
        self.h.backend.usage(&self.handle).unwrap()
    }

    fn apply(&self, limits: Limits) {
        self.h.backend.apply(&self.handle, &limits).unwrap();
    }

    fn named(&self, name: &str) -> Vec<Proc> {
        self.procs()
            .into_iter()
            .filter(|p| p.name == name)
            .collect()
    }
}

fn wait(what: &str, within: Duration, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Types `line` into `agent_ref`'s terminal, through its shim.
fn type_line(h: &MacHarness, agent_ref: &str, line: &str) {
    use std::io::Write;
    use vibe_core::shim::{self, Frame, FrameKind};
    let mut c = shim::connect(
        &h.scratch().join(format!("{agent_ref}.sock")),
        &shim::control_hello("test"),
        Duration::from_secs(10),
    )
    .unwrap();
    shim::write_frame(
        &mut c.stream,
        &Frame::new(FrameKind::Input, format!("{line}\r")),
    )
    .unwrap();
    c.stream.flush().unwrap();
    c.status().unwrap();
}

/// Limits nothing here comes near.
fn roomy() -> Limits {
    Limits {
        memory: 1 << 40,
        cpu_cores: 64,
        max_processes: 10_000,
        disk_soft: 1 << 40,
        disk_hard: 1 << 41,
    }
}

/// Governed at a tenth of the usual intervals, so a minute's disk check
/// comes round in a few seconds.
fn quick() -> Intervals {
    Intervals {
        walk: Duration::from_millis(250),
        scan: Duration::from_millis(1000),
        disk: Duration::from_millis(500),
    }
}

#[test]
fn a_fork_bomb_is_paused() {
    let _one = serial();
    // Bounded, so a governor that failed wouldn't take the machine down:
    // each level forks two more, 2^(depth+1) shells and their sleeps.
    let depth: u32 = std::env::var("VIBE_FORK_BOMB_DEPTH")
        .ok()
        .and_then(|d| d.parse().ok())
        .unwrap_or(6);
    let h = MacHarness::with(SCRIPT, Intervals::default());
    let a = Agent::start(
        &h,
        "bomb",
        &format!(
            "b() {{ if [ $1 -gt 0 ]; then b $(($1-1)) & b $(($1-1)) & fi; sleep 120; }}; b {depth} &"
        ),
    );
    a.apply(Limits {
        max_processes: 32,
        ..roomy()
    });
    wait("a processes pause", Duration::from_secs(10), || {
        a.usage().paused == Some(Limit::Processes)
    });
    // Paused means stopped, all of it, and it doesn't grow any more.
    std::thread::sleep(Duration::from_millis(500));
    let procs = a.procs();
    assert!(procs.len() > 32, "{} processes", procs.len());
    let running: Vec<_> = procs.iter().filter(|p| !p.stopped).collect();
    assert!(running.is_empty(), "still running: {running:?}");
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(a.procs().len(), procs.len(), "it grew while paused");
    // The shim isn't stopped: it must keep answering.
    let shim = vibe_env::tree::process(a.handle.session.unwrap())
        .unwrap()
        .0;
    assert!(!shim.stopped);
    // A delete ends all of it.
    h.backend.delete(&a.handle).unwrap();
    assert!(SessionTree::with_descendants(a.handle.session.unwrap()).is_empty());
}

#[test]
fn a_200_mb_allocation_shows_up_in_the_agents_memory() {
    let _one = serial();
    let h = MacHarness::with(SCRIPT, Intervals::default());
    let a = Agent::start(
        &h,
        "alloc",
        // A size known only when it runs, as a constant one would be built
        // once at compile time and then copied: 400 MB.
        "perl -e '$n = 200 * 1024 * 1024; $x = \"a\" x $n; sleep 300' &",
    );
    a.apply(roomy());
    wait(
        "200 MB in the agent's memory",
        Duration::from_secs(30),
        || a.usage().memory >= 200 * MIB,
    );
    let perl = a.named("perl");
    assert_eq!(perl.len(), 1, "{:?}", a.procs());
    // About what it allocated, not double.
    assert!(
        perl[0].memory >= 200 * MIB && perl[0].memory < 300 * MIB,
        "{:?}",
        perl[0]
    );
    // And it counts against the agent's memory: 83% of 240 MB warns, and
    // puts it in the background, without pausing it.
    a.apply(Limits {
        memory: 240 * MIB,
        ..roomy()
    });
    wait("a memory warning", Duration::from_secs(10), || {
        a.usage().warnings.contains(&Limit::Memory)
    });
    assert_eq!(a.usage().paused, None);
    #[cfg(target_os = "macos")]
    wait("perl in the background", Duration::from_secs(5), || {
        priority(perl[0].pid) <= 10
    });
    // Over all of it, it's paused.
    a.apply(Limits {
        memory: 150 * MIB,
        ..roomy()
    });
    wait("a memory pause", Duration::from_secs(10), || {
        a.usage().paused == Some(Limit::Memory)
    });
    assert!(a.procs().iter().all(|p| p.stopped));
    // Given room and woken, it runs again, unwarned and as itself.
    a.apply(Limits {
        memory: 1 << 30,
        ..roomy()
    });
    assert_eq!(h.backend.wake(&a.handle).unwrap(), Woke::Exact);
    wait("it to run", Duration::from_secs(5), || {
        a.procs().iter().all(|p| !p.stopped)
    });
    wait("the warning to go", Duration::from_secs(10), || {
        a.usage().warnings.is_empty()
    });
    assert_eq!(a.usage().paused, None);
    #[cfg(target_os = "macos")]
    wait(
        "perl back from the background",
        Duration::from_secs(5),
        || priority(perl[0].pid) > 10,
    );
}

/// A process's scheduling priority as `ps` shows it: 31 normally, 4 in the
/// background (PRD: Spike results).
#[cfg(target_os = "macos")]
fn priority(pid: u32) -> i32 {
    let out = Command::new("ps")
        .args(["-o", "pri=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(-1)
}

#[test]
fn a_nohup_child_is_still_counted_once_its_reparented() {
    let _one = serial();
    let h = MacHarness::with(SCRIPT, quick());
    // Five sleeps whose parent shell exits, so launchd (or init, or a
    // subreaper) adopts them; then the agent goes on. They start once the
    // governor holds it, so it's its walks and scans that must find them.
    let a = Agent::start(
        &h,
        "nohup",
        "read go; sh -c 'for i in 1 2 3 4 5; do nohup sleep 300 >/dev/null 2>&1 & done'",
    );
    a.apply(Limits {
        max_processes: 3,
        ..roomy()
    });
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(a.usage().paused, None, "the script alone is under three");
    type_line(&h, "nohup", "go");
    // Each one's parent is outside the tree once the shell has gone.
    let shim = a.handle.session.unwrap();
    let reparented = |a: &Agent| {
        let procs = a.procs();
        let ours: Vec<u32> = procs.iter().map(|p| p.pid).collect();
        procs
            .iter()
            .filter(|p| p.name == "sleep" && p.ppid != shim && !ours.contains(&p.ppid))
            .count()
    };
    wait("five reparented sleeps", Duration::from_secs(10), || {
        reparented(&a) == 5
    });
    let ours: Vec<u32> = a.procs().iter().map(|p| p.pid).collect();
    assert_eq!(ours.len(), 6, "the script and its five sleeps");
    // The governor counts them, and they take it over three processes.
    wait("a processes pause", Duration::from_secs(10), || {
        a.usage().paused == Some(Limit::Processes)
    });
    assert!(a.named("sleep").iter().all(|p| p.stopped));
    // Sleep and delete reach them too.
    h.backend.delete(&a.handle).unwrap();
    for pid in ours {
        assert!(vibe_env::tree::process(pid).is_none(), "{pid} was left");
    }
}

#[test]
fn a_child_in_a_session_of_its_own_is_found_by_the_walk() {
    let _one = serial();
    let h = MacHarness::with(SCRIPT, quick());
    let a = Agent::start(
        &h,
        "setsid",
        "perl -MPOSIX -e 'POSIX::setsid(); sleep 300' &",
    );
    // Found, once it has a session of its own: only the walk finds it.
    let shim = a.handle.session.unwrap();
    wait(
        "perl in a session of its own",
        Duration::from_secs(10),
        || {
            a.named("perl")
                .iter()
                .any(|p| vibe_env::tree::process(p.pid).is_some_and(|(_, sid)| sid != shim))
        },
    );
    let perl = a.named("perl")[0].pid;
    assert!(
        SessionTree::new(shim).all().iter().all(|p| p.pid != perl),
        "the session alone doesn't have it"
    );
    // Still the agent's: it sleeps and wakes with it.
    h.backend.sleep(&a.handle).unwrap();
    assert!(vibe_env::tree::process(perl).unwrap().0.stopped);
    assert_eq!(h.backend.wake(&a.handle).unwrap(), Woke::Exact);
    wait("perl to run", Duration::from_secs(5), || {
        !vibe_env::tree::process(perl).unwrap().0.stopped
    });
    h.backend.delete(&a.handle).unwrap();
    assert!(vibe_env::tree::process(perl).is_none());
}

#[test]
fn a_member_that_leaves_tree_and_session_is_still_counted() {
    let _one = serial();
    let h = MacHarness::with(SCRIPT, quick());
    // A child that, a second in, starts a session of its own; a second
    // after that, its parent exits, so it's reparented too: by then
    // neither a walk nor a scan would find it, only having found it.
    let a = Agent::start(
        &h,
        "daemon",
        "perl -MPOSIX -e 'if (fork) { sleep 2; exit } sleep 1; POSIX::setsid(); sleep 300' &",
    );
    a.apply(roomy());
    wait("two perls", Duration::from_secs(10), || {
        a.named("perl").len() == 2
    });
    let child = a.named("perl").iter().map(|p| p.pid).max().unwrap();
    wait("its parent to go", Duration::from_secs(10), || {
        a.named("perl").len() == 1
    });
    let (p, sid) = vibe_env::tree::process(child).unwrap();
    assert_ne!(sid, a.handle.session.unwrap(), "it left the session");
    assert!(
        a.procs().iter().all(|q| q.pid != p.ppid),
        "it left the tree: {p:?}"
    );
    assert!(
        a.named("perl").iter().any(|q| q.pid == child),
        "still counted"
    );
    // The governor counts it, and a pause stops it.
    a.apply(Limits {
        max_processes: 1,
        ..roomy()
    });
    wait("a processes pause", Duration::from_secs(10), || {
        a.usage().paused == Some(Limit::Processes)
    });
    assert!(vibe_env::tree::process(child).unwrap().0.stopped);
    h.backend.delete(&a.handle).unwrap();
    assert!(vibe_env::tree::process(child).is_none(), "delete ends it");
}

#[test]
fn cpu_is_held_to_its_cap() {
    let _one = serial();
    let h = MacHarness::with(SCRIPT, Intervals::default());
    let a = Agent::start(
        &h,
        "spin",
        "while :; do :; done & while :; do :; done & echo spinning",
    );
    wait("two spinners", Duration::from_secs(10), || {
        a.procs().len() >= 3
    });
    let cpu = |a: &Agent| a.usage().cpu_ms;
    // What it uses unlimited, which a busy machine may hold under the cap.
    let cores_over = |a: &Agent, d: Duration| {
        let (before, t) = (cpu(a), Instant::now());
        std::thread::sleep(d);
        (cpu(a) - before) as f64 / t.elapsed().as_millis() as f64
    };
    let free = cores_over(&a, Duration::from_secs(1));
    a.apply(Limits {
        cpu_cores: 1,
        ..roomy()
    });
    // The background, then duty cycling, take a few seconds to settle.
    std::thread::sleep(Duration::from_secs(7));
    // Meanwhile, a duty cycle is seen stopping the spinners now and then.
    let t = Instant::now();
    let mut seen_stopped = false;
    let before = cpu(&a);
    while t.elapsed() < Duration::from_secs(4) {
        let procs = a.procs();
        seen_stopped |= procs.iter().any(|p| p.stopped);
        std::thread::sleep(Duration::from_millis(20));
    }
    let cores = (cpu(&a) - before) as f64 / t.elapsed().as_millis() as f64;
    eprintln!("{free:.2} cores unlimited, {cores:.2} held to 1");
    assert!(cores < 1.35, "{cores:.2} cores against a cap of 1");
    assert!(cores > 0.2, "{cores:.2} cores: throttled, not stopped");
    if free > 1.4 {
        assert!(seen_stopped, "over the cap, but never duty cycled");
    } else {
        eprintln!("the machine gave it only {free:.2} cores: duty cycling not required");
    }
    assert_eq!(a.usage().paused, None, "CPU never pauses");
    // Asleep, a duty cycle doesn't continue it; woken, it runs on.
    h.backend.sleep(&a.handle).unwrap();
    std::thread::sleep(Duration::from_secs(2));
    assert!(a.procs().iter().all(|p| p.stopped), "{:?}", a.procs());
    let asleep = cpu(&a);
    std::thread::sleep(Duration::from_millis(500));
    assert!(cpu(&a) - asleep < 50, "it ran while asleep");
    assert_eq!(h.backend.wake(&a.handle).unwrap(), Woke::Exact);
    let woke = cpu(&a);
    wait("it to spin again", Duration::from_secs(5), || {
        cpu(&a) > woke + 200
    });
}

#[test]
fn disk_past_soft_warns_and_past_hard_pauses() {
    let _one = serial();
    let h = MacHarness::with(SCRIPT, quick());
    // Three MiB in the worktree, and one in the temp dir.
    let a = Agent::start(
        &h,
        "disk",
        "head -c 3145728 /dev/zero >fill && head -c 1048576 /dev/zero >\"$TMPDIR/fill\"",
    );
    a.apply(Limits {
        disk_soft: 2 * MIB,
        ..roomy()
    });
    wait("a disk warning", Duration::from_secs(10), || {
        a.usage().warnings.contains(&Limit::Disk)
    });
    let u = a.usage();
    assert!(u.disk.unwrap() >= 4 * MIB, "{u:?}");
    assert_eq!(u.paused, None);
    a.apply(Limits {
        disk_soft: 2 * MIB,
        disk_hard: 3 * MIB,
        ..roomy()
    });
    wait("a disk pause", Duration::from_secs(10), || {
        a.usage().paused == Some(Limit::Disk)
    });
    assert!(a.procs().iter().all(|p| p.stopped), "paused means stopped");
    // Room made and woken, the measurement from before doesn't pause it
    // again: a fresh one does, or doesn't.
    fs::remove_file(a.handle.worktree.join("fill")).unwrap();
    assert_eq!(h.backend.wake(&a.handle).unwrap(), Woke::Exact);
    std::thread::sleep(Duration::from_secs(2));
    let u = a.usage();
    assert_eq!(u.paused, None, "{u:?}");
    assert!(u.disk.is_some_and(|d| d < 3 * MIB), "{u:?}");
    assert!(a.procs().iter().all(|p| !p.stopped));
}

#[test]
fn a_pause_holds_across_a_restart_of_vibed() {
    let _one = serial();
    let h = MacHarness::with(SCRIPT, quick());
    let backend = |h: &MacHarness| {
        Macos::new(h.root().join("home"))
            .with_claude(h.root().join("bin/claude"))
            .with_path(std::env::var("PATH").unwrap())
            .with_intervals(quick())
    };
    let first = backend(&h);
    let handle = start_on(&first, &h, "restart", "sleep 300 & sleep 300 & sleep 300 &");
    // Whatever happens, nothing of it is left running.
    struct Ends(u32);
    impl Drop for Ends {
        fn drop(&mut self) {
            SessionTree::with_descendants(self.0).end(Duration::from_secs(2));
        }
    }
    let _ends = Ends(handle.session.unwrap());
    let procs = || first.processes(&handle).unwrap();
    wait("three sleeps", Duration::from_secs(10), || {
        procs().iter().filter(|p| p.name == "sleep").count() == 3
    });
    first
        .apply(
            &handle,
            &Limits {
                max_processes: 2,
                ..roomy()
            },
        )
        .unwrap();
    wait("a processes pause", Duration::from_secs(10), || {
        first.usage(&handle).unwrap().paused == Some(Limit::Processes)
    });
    let file = handle.agent_dir.join(STATE_FILE);
    assert!(file.is_file());
    // vibed goes, and its governors with it.
    drop(first);
    std::thread::sleep(Duration::from_millis(500));

    // A new vibed's backend says it's paused, before and after it holds
    // the agent to its limits again, and it stays stopped.
    let again = backend(&h);
    let u = again.usage(&handle).unwrap();
    assert_eq!(u.paused, Some(Limit::Processes), "{u:?}");
    let procs = || again.processes(&handle).unwrap();
    assert!(procs().iter().all(|p| p.stopped));
    let limits = Limits {
        max_processes: 100,
        ..roomy()
    };
    again.apply(&handle, &limits).unwrap();
    std::thread::sleep(Duration::from_millis(600));
    assert!(procs().iter().all(|p| p.stopped), "still paused");
    assert_eq!(again.usage(&handle).unwrap().paused, Some(Limit::Processes));
    // A wake brings it back, and the pause is over.
    assert_eq!(again.wake(&handle).unwrap(), Woke::Exact);
    wait("it to run", Duration::from_secs(5), || {
        procs().iter().all(|p| !p.stopped)
    });
    std::thread::sleep(Duration::from_millis(600));
    assert!(procs().iter().all(|p| !p.stopped), "paused again");
    assert!(!file.exists());
    again.delete(&handle).unwrap();
    assert!(SessionTree::with_descendants(handle.session.unwrap()).is_empty());
}

#[test]
fn a_governor_from_a_killed_vibed_leaves_nothing_stopped() {
    let _one = serial();
    // As if a vibed was killed mid duty cycle: its agent's processes stay
    // stopped, though the agent is meant to run.
    let h = MacHarness::with(SCRIPT, quick());
    let a = Agent::start(&h, "killed", "sleep 300 &");
    wait("a sleep", Duration::from_secs(10), || {
        a.named("sleep").len() == 1
    });
    for p in a.procs() {
        unsafe { libc::kill(p.pid as i32, libc::SIGSTOP) };
    }
    wait("it to stop", Duration::from_secs(5), || {
        a.procs().iter().all(|p| p.stopped)
    });
    a.apply(roomy());
    wait("it to run", Duration::from_secs(5), || {
        a.procs().iter().all(|p| !p.stopped)
    });
}

#[test]
fn agents_use_the_owners_home_and_path() {
    let _one = serial();
    let h = MacHarness::with(SCRIPT, quick());
    let spec = h.spec("env");
    let handle = h.backend.create(&spec).unwrap();
    // Claude's state is the owner's own.
    assert_eq!(handle.claude().unwrap().dir, h.root().join("home/.claude"));
    assert!(!handle.claude().unwrap().own);
    let out = h
        .backend
        .run(
            &handle,
            &vibe_env::Run {
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "echo \"$HOME|$PATH|$TMPDIR\"".into(),
                ],
                env: Vec::new(),
                cwd: spec.worktree.clone(),
                timeout: Duration::from_secs(10),
            },
        )
        .unwrap();
    let text = String::from_utf8_lossy(&out.output).trim().to_owned();
    let tmp = spec.agent_dir.join("tmp");
    assert_eq!(
        text,
        format!(
            "{}|{}|{}",
            h.root().join("home").display(),
            std::env::var("PATH").unwrap(),
            tmp.display()
        )
    );
    // A launch runs the given claude with the same, on the shim's PTY.
    let end = h
        .backend
        .launch(
            &handle,
            &Launch {
                agent: Default::default(),
                args: vec!["--model".into(), "opus".into()],
                env: vec![("GIT_CONFIG_COUNT".into(), "0".into())],
                cwd: spec.worktree.clone(),
            },
        )
        .unwrap();
    let TerminalEnd::Pty {
        command, cwd, env, ..
    } = end
    else {
        panic!("a PTY");
    };
    assert_eq!(command[0], h.root().join("bin/claude").to_string_lossy());
    assert_eq!(&command[1..], ["--model", "opus"]);
    assert_eq!(cwd, spec.worktree);
    assert!(env.contains(&("GIT_CONFIG_COUNT".into(), "0".into())));
    assert!(env.iter().any(|(k, _)| k == "PATH"));
    assert!(
        !env.iter().any(|(k, _)| k == "CLAUDE_CONFIG_DIR"),
        "{env:?}"
    );
    h.backend.delete(&handle).unwrap();
    assert!(!tmp.exists());
}
