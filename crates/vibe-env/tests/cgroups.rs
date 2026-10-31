//! The Linux host's `linux` backend, on real cgroups (PRD: How agents run
//! on a Linux host): the conformance suite (`vibe_env::conformance`), one
//! test per check, then what the kernel does with the limits: they're in
//! the agent's cgroup and change with it, a memory hog is stopped by its own
//! limit without touching its neighbour, a fork past `pids.max` and a full
//! disk pause it, membership is the cgroup (a `setsid` job included), a
//! pause outlives `vibed`, and a run ends whatever it leaves behind.
//!
//! These need Linux with cgroups v2 and a systemd user manager given the
//! `memory` and `pids` controllers. Without them each test says so and
//! passes, unless `VIBE_TEST_CGROUPS` is set, as `scripts/check-full.sh`
//! sets it where they must run. Every agent here has a memory limit of tens
//! of MB, and everything runs in a slice of this process's own,
//! `vibetest<pid>…`, which each harness stops when it's dropped, so nothing
//! is left behind and nothing else is touched.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use vibe_core::shim::{self, Frame, FrameKind};
use vibe_env::cgroups::{Cgroup, Cgroups, Intervals};
use vibe_env::conformance::{self, Harness, LimitCase};
use vibe_env::testing::STAND_IN;
use vibe_env::tree::SessionTree;
use vibe_env::{
    AgentSpec, Backend, Compartment, Handle, Launch, Limit, Limiter, Limits, Proc, Run, Sleeper,
    TerminalEnd, Woke,
};

const MIB: u64 = 1 << 20;

/// Every test agent's memory: tiny, as the box running these may be small
/// and shared.
const MEMORY: u64 = 48 * MIB;

/// One test at a time, whatever the test threads: they time what the
/// kernel and the watcher do.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// A "Claude" that runs its prompt as a shell script, then echoes what it's
/// sent.
const SCRIPT: &str = r#"#!/bin/sh
while [ $# -gt 0 ]; do case $1 in --) shift; break ;; *) shift ;; esac; done
eval "$*"
echo Ready.
while read -r line; do echo "> $line"; done
"#;

fn install(path: &Path, body: &str) {
    // Written beside its place and renamed in, so no test thread forks
    // while it's open for writing ("text file busy").
    let tmp = path.with_extension("new");
    fs::write(&tmp, body).unwrap();
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&tmp, path).unwrap();
}

fn systemctl(args: &[&str]) -> String {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .expect("systemctl runs");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

struct CgHarness {
    backend: Cgroups,
    /// Under /tmp, so the shims' sockets have short paths.
    dir: tempfile::TempDir,
}

impl Drop for CgHarness {
    fn drop(&mut self) {
        // Whatever a failed test left: every scope in its slice, killed as
        // it is (frozen or not, which systemd may not know), then the slice.
        if let Ok((sd, _)) = self.backend.check() {
            let slice = sd.manager.join(self.backend.slice());
            for e in fs::read_dir(&slice).into_iter().flatten().flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    let cg = Cgroup { dir: e.path() };
                    cg.kill();
                    cg.wait_empty(Duration::from_secs(5));
                }
            }
        }
        systemctl(&["stop", self.backend.slice()]);
    }
}

static HARNESSES: AtomicU32 = AtomicU32::new(0);

impl CgHarness {
    /// With the stand-in for Claude Code, as the conformance suite wants;
    /// or `None`, having said why, where there are no cgroups to test.
    fn new() -> Option<CgHarness> {
        CgHarness::with(STAND_IN, Intervals::default())
    }

    fn with(claude: &str, intervals: Intervals) -> Option<CgHarness> {
        let must = std::env::var_os("VIBE_TEST_CGROUPS").is_some_and(|v| !v.is_empty());
        if !cfg!(target_os = "linux") {
            assert!(!must, "VIBE_TEST_CGROUPS is set, but cgroups are Linux's");
            eprintln!("not Linux: no cgroups to test");
            return None;
        }
        let dir = tempfile::Builder::new()
            .prefix("cg")
            .tempdir_in("/tmp")
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        install(&bin.join("claude"), claude);
        // The owner's home: Claude's state is there before any agent, and
        // is the server's own, not the agent's.
        let home = root.join("home");
        fs::create_dir_all(home.join(".claude/projects")).unwrap();
        fs::write(home.join(".claude.json"), "{}").unwrap();
        fs::create_dir_all(root.join("scratch")).unwrap();
        fs::set_permissions(root.join("scratch"), fs::Permissions::from_mode(0o700)).unwrap();
        let slice = format!(
            "vibetest{}x{}.slice",
            std::process::id(),
            HARNESSES.fetch_add(1, Ordering::SeqCst)
        );
        let backend = Cgroups::new(&home)
            .with_claude(bin.join("claude"))
            .with_path(std::env::var("PATH").unwrap_or_default())
            .with_slice(slice)
            .with_intervals(intervals);
        match backend.check() {
            Ok(_) => Some(CgHarness { backend, dir }),
            Err(e) if must => panic!("VIBE_TEST_CGROUPS is set, but: {e}"),
            Err(e) => {
                eprintln!("no cgroups to test here: {e}");
                None
            }
        }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn spec_with(&self, agent_ref: &str, limits: Limits) -> AgentSpec {
        let root = self.root();
        let worktree = root.join("worktrees").join(agent_ref);
        let agent_dir = root.join("agents").join(agent_ref);
        for d in [&worktree, &agent_dir] {
            fs::create_dir_all(d).unwrap();
        }
        AgentSpec {
            agent_ref: agent_ref.into(),
            project: "web".into(),
            worktree,
            git_dir: root.join("web/.git"),
            agent_dir,
            limits,
        }
    }
}

/// Limits nothing here comes near, but for memory, which is always tiny.
fn roomy() -> Limits {
    Limits {
        memory: MEMORY,
        cpu_cores: 1,
        max_processes: 64,
        disk_soft: 1 << 30,
        disk_hard: 1 << 31,
    }
}

impl Harness for CgHarness {
    type B = Cgroups;

    fn backend(&self) -> &Cgroups {
        &self.backend
    }

    fn spec(&self, agent_ref: &str) -> AgentSpec {
        self.spec_with(agent_ref, roomy())
    }

    fn shim(&self) -> PathBuf {
        // VIBE_SHIM where the tests run apart from their build, as on a
        // real server; otherwise built beside the tests by `cargo test
        // --workspace`.
        if let Some(shim) = std::env::var_os("VIBE_SHIM").filter(|v| !v.is_empty()) {
            return PathBuf::from(shim);
        }
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

    /// Every process ends: the agent's cgroup and its shim's session.
    fn reboot(&self, h: &Handle) {
        let cg = self.backend.cgroup(&h.agent_ref).unwrap();
        cg.kill();
        assert!(cg.wait_empty(Duration::from_secs(10)));
        if let Some(sid) = h.session {
            SessionTree::new(sid).end(Duration::from_secs(5));
        }
        cg.wait_gone(Duration::from_secs(10));
    }

    fn limit_case(&self) -> LimitCase {
        let limits = Limits {
            max_processes: 8,
            ..roomy()
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

macro_rules! harness {
    () => {
        match CgHarness::new() {
            Some(h) => h,
            None => return,
        }
    };
    ($claude:expr, $intervals:expr) => {
        match CgHarness::with($claude, $intervals) {
            Some(h) => h,
            None => return,
        }
    };
}

#[test]
fn start_shows_processes() {
    let _one = serial();
    conformance::start_shows_processes(&harness!());
}

#[test]
fn run_in_compartment() {
    let _one = serial();
    conformance::run_in_compartment(&harness!());
}

#[test]
fn limit_hit_pauses() {
    let _one = serial();
    conformance::limit_hit_pauses(&harness!());
}

#[test]
fn sleep_then_wake_is_exact() {
    let _one = serial();
    conformance::sleep_then_wake_is_exact(&harness!());
}

#[test]
fn reboot_then_wake() {
    let _one = serial();
    conformance::reboot_then_wake(&harness!());
}

#[test]
fn delete_leaves_nothing() {
    let _one = serial();
    let h = harness!();
    conformance::delete_leaves_nothing(&h);
    // Nor anything in systemd: the scope has gone.
    assert_eq!(
        systemctl(&[
            "show",
            "--property=LoadState",
            "--value",
            &h.backend.unit("conf-delete")
        ]),
        "not-found"
    );
}

#[test]
fn the_backend_says_what_it_can_do() {
    let caps = Cgroups::new("/nowhere").capabilities();
    assert!(
        !caps.sleep_frees_memory,
        "frozen memory stays, and may swap"
    );
    assert!(!caps.exact_after_reboot, "after a reboot it's --resume");
    assert!(caps.kernel_limits, "the kernel holds the limits");
    assert!(!caps.file_containment, "no file sandbox");
}

#[test]
fn a_server_that_cant_limit_agents_says_how_to_fix_it() {
    // A stand-in for systemd whose user manager has memory but not pids.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("cgroup");
    let manager = root.join("user.slice/user@1000.service");
    fs::create_dir_all(&manager).unwrap();
    fs::write(root.join("cgroup.controllers"), "cpu memory pids io\n").unwrap();
    fs::write(manager.join("cgroup.controllers"), "memory\n").unwrap();
    fs::write(manager.join("cgroup.freeze"), "0\n").unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    install(
        &bin.join("systemctl"),
        "#!/bin/sh\nprintf 'Version=250\\nControlGroup=/user.slice/user@1000.service\\n'\n",
    );
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let backend = Cgroups::new(dir.path())
        .with_path(&path)
        .with_cgroup_root(&root);
    let h = |r: &str| AgentSpec {
        agent_ref: r.into(),
        project: "web".into(),
        worktree: dir.path().join("w"),
        git_dir: dir.path().join(".git"),
        agent_dir: dir.path().join("a"),
        limits: roomy(),
    };
    let err = backend.create(&h("x")).unwrap_err();
    assert!(err.message().contains("can't limit pids"), "{err}");
    assert!(err.message().contains("Delegate=cpu memory pids"), "{err}");
    assert!(!dir.path().join("a").exists(), "nothing was made");

    // Given pids but not cpu, agents start without the CPU cap, and it's
    // said.
    fs::write(manager.join("cgroup.controllers"), "memory pids\n").unwrap();
    let backend = Cgroups::new(dir.path())
        .with_path(&path)
        .with_cgroup_root(&root);
    let (sd, warnings) = backend.check().unwrap();
    assert_eq!(sd.version, 250);
    assert!(!sd.cpu());
    assert!(warnings[0].contains("cpu_cores"), "{warnings:?}");

    // Without a freezer (Linux before 5.2), nothing could sleep or pause.
    fs::remove_file(manager.join("cgroup.freeze")).unwrap();
    let old = Cgroups::new(dir.path())
        .with_path(&path)
        .with_cgroup_root(&root);
    assert!(
        old.check()
            .unwrap_err()
            .message()
            .contains("no cgroup freezer")
    );
    fs::write(manager.join("cgroup.freeze"), "0\n").unwrap();

    // Without cgroups v2, or a user manager, it says so.
    let none = Cgroups::new(dir.path())
        .with_path(&path)
        .with_cgroup_root(dir.path().join("nothing"));
    assert!(none.check().unwrap_err().message().contains("cgroups v2"));
    install(
        &bin.join("systemctl"),
        "#!/bin/sh\necho 'Failed to connect to bus' >&2\nexit 1\n",
    );
    let down = Cgroups::new(dir.path())
        .with_path(&path)
        .with_cgroup_root(&root);
    let err = down.check().unwrap_err();
    assert!(err.message().contains("Failed to connect to bus"), "{err}");
}

/// An agent started on the harness's "Claude", ended when dropped.
struct Agent<'a> {
    h: &'a CgHarness,
    handle: Handle,
}

impl Drop for Agent<'_> {
    fn drop(&mut self) {
        let _ = self.h.backend.delete(&self.handle);
        if let Some(sid) = self.handle.session {
            SessionTree::new(sid).end(Duration::from_secs(2));
        }
    }
}

impl<'a> Agent<'a> {
    /// Creates `agent_ref`'s compartment with `limits` and launches the
    /// harness's "Claude" on `prompt` in it, on a shim, as `vibed` does.
    fn start(h: &'a CgHarness, agent_ref: &str, limits: Limits, prompt: &str) -> Agent<'a> {
        let spec = h.spec_with(agent_ref, limits);
        let handle = h.backend.create(&spec).unwrap();
        let mut a = Agent { h, handle };
        a.launch(prompt);
        a
    }

    /// Launches the harness's "Claude" on `prompt` in its compartment.
    fn launch(&mut self, prompt: &str) {
        let (h, r) = (self.h, self.handle.agent_ref.clone());
        let end = h
            .backend
            .launch(
                &self.handle,
                &Launch {
                    args: vec!["--".into(), prompt.into()],
                    env: Vec::new(),
                    cwd: self.handle.worktree.clone(),
                },
            )
            .unwrap();
        let TerminalEnd::Pty { command, cwd, env } = end else {
            panic!("a Linux host's agent runs on a PTY");
        };
        let out = Command::new(h.shim())
            .args(["--ref", &r, "--socket"])
            .arg(h.scratch().join(format!("{r}.sock")))
            .arg("--state")
            .arg(h.scratch().join(&r))
            .args(["--size", "100x30", "--cwd"])
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
        self.handle.session = Some(String::from_utf8_lossy(&out.stdout).trim().parse().unwrap());
    }

    fn cgroup(&self) -> Cgroup {
        self.h.backend.cgroup(&self.handle.agent_ref).unwrap()
    }

    fn file(&self, name: &str) -> String {
        fs::read_to_string(self.cgroup().dir.join(name))
            .unwrap_or_default()
            .trim()
            .to_owned()
    }

    fn event(&self, file: &str, key: &str) -> u64 {
        self.file(file)
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(' ')?.parse().ok())
            .unwrap_or(0)
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

    fn connect(&self) -> shim::Connection {
        shim::connect(
            &self
                .h
                .scratch()
                .join(format!("{}.sock", self.handle.agent_ref)),
            &shim::control_hello("test"),
            Duration::from_secs(10),
        )
        .unwrap()
    }

    fn screen(&self) -> String {
        self.connect().read_output(None).unwrap().screen
    }

    fn wait_screen(&self, text: &str) {
        wait(&format!("{text:?} on the screen"), PATIENCE, || {
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
        c.status().unwrap();
    }

    /// It answers what it's sent: it runs.
    fn answers(&self, line: &str) {
        self.type_line(line);
        self.wait_screen(&format!("> {line}"));
    }

    fn ticks(&self) -> Option<u64> {
        fs::read_to_string(self.handle.worktree.join(".vibe-ticks"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }
}

const PATIENCE: Duration = Duration::from_secs(30);

fn wait(what: &str, within: Duration, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_limits_are_the_kernels_and_change_with_it() {
    let _one = serial();
    let h = harness!();
    let limits = Limits {
        memory: 40 * MIB,
        cpu_cores: 1,
        max_processes: 50,
        ..roomy()
    };
    // `$HOME` reaches Claude as it was written: systemd expands nothing.
    let a = Agent::start(&h, "kernel", limits, "fix $HOME now");
    a.wait_screen("Working on: fix $HOME now");
    // In force from the start, before vibed holds it to anything.
    assert_eq!(a.file("memory.max"), (40 * MIB).to_string());
    assert_eq!(a.file("memory.swap.max"), (40 * MIB).to_string());
    // Throttled from 95%, where it's paused: well before the OOM killer.
    let high: u64 = a.file("memory.high").parse().unwrap();
    assert!(high.abs_diff(40 * MIB * 95 / 100) < 4096, "{high}");
    // Room for threads: eight tasks a process.
    assert_eq!(a.file("pids.max"), "400");
    let (sd, _) = h.backend.check().unwrap();
    if sd.cpu() {
        assert_eq!(a.file("cpu.max"), "100000 100000");
    }
    // Claude is in the cgroup, and its shim isn't.
    let sid = a.handle.session.unwrap();
    let pids = a.cgroup().pids();
    assert!(!pids.contains(&sid), "the shim isn't frozen with the agent");
    let procs = a.procs();
    assert!(
        procs
            .iter()
            .any(|p| p.args.iter().any(|x| x.ends_with("claude"))),
        "{procs:?}"
    );
    assert!(procs.iter().all(|p| pids.contains(&p.pid)));

    a.apply(Limits {
        memory: 44 * MIB,
        cpu_cores: 2,
        max_processes: 60,
        ..roomy()
    });
    assert_eq!(a.file("memory.max"), (44 * MIB).to_string());
    assert_eq!(a.file("memory.swap.max"), (44 * MIB).to_string());
    assert_eq!(a.file("pids.max"), "480");
    if sd.cpu() {
        assert_eq!(a.file("cpu.max"), "200000 100000");
    }
    let usage = a.usage();
    assert!(usage.memory > 0 && usage.memory < 44 * MIB, "{usage:?}");
    assert!(usage.processes >= 1 && usage.paused.is_none(), "{usage:?}");
    a.answers("still here");
}

#[test]
fn a_memory_hog_is_stopped_by_its_own_limit_and_hurts_nobody() {
    let _one = serial();
    let h = harness!(SCRIPT, Intervals::default());
    let limits = Limits {
        memory: 32 * MIB,
        ..roomy()
    };
    // The neighbour counts on, the whole time.
    let quiet = Agent::start(
        &h,
        "neighbour",
        limits,
        "( while :; do i=$((i+1)); echo $i >.vibe-ticks; sleep 0.1; done ) &",
    );
    quiet.wait_screen("Ready.");
    // A hog that wants 128 MiB, four times its limit, in a child the way a
    // test run or a build is Claude's child.
    let hog_cmd =
        "( x=$(head -c 134217728 /dev/zero | tr '\\0' a); echo kept ${#x} ) & echo hogging";
    let hog = Agent::start(&h, "hog", limits, hog_cmd);
    hog.wait_screen("hogging");

    // With nothing but the kernel's limits (vibed down, say), RAM and swap
    // together can't pass twice the limit, and the OOM killer ends the hog
    // inside its own cgroup.
    wait("the OOM killer", Duration::from_secs(60), || {
        hog.event("memory.events", "oom_kill") >= 1
    });
    assert!(!hog.screen().contains("kept"), "the hog got its memory");
    if let Ok(peak) = fs::read_to_string(hog.cgroup().dir.join("memory.peak")) {
        assert!(peak.trim().parse::<u64>().unwrap() <= 32 * MIB, "{peak}");
    }
    let swap = hog.file("memory.swap.peak");
    if !swap.is_empty() {
        assert!(swap.parse::<u64>().unwrap() <= 32 * MIB, "{swap}");
    }
    // The agent itself carries on, and so does its neighbour.
    hog.answers("survived");
    let before = quiet.ticks();
    wait("the neighbour to tick on", PATIENCE, || {
        quiet.ticks() > before
    });
    assert_eq!(quiet.event("memory.events", "oom_kill"), 0);
    quiet.answers("unbothered");

    // Held to its limits, as vibed holds it from its start, a hog pauses
    // the agent, and says why; its neighbour is held too, and runs on.
    quiet.apply(limits);
    let again = Agent::start(&h, "hog2", limits, hog_cmd);
    again.apply(limits);
    wait("a memory pause", Duration::from_secs(60), || {
        again.usage().paused == Some(Limit::Memory)
    });
    assert!(again.cgroup().frozen());
    assert!(again.procs().iter().all(|p| p.stopped));
    assert_eq!(quiet.usage().paused, None);
    let before = quiet.ticks();
    wait("the neighbour to tick on", PATIENCE, || {
        quiet.ticks() > before
    });
    quiet.answers("still unbothered");
    // Raised and woken, it runs on.
    again.apply(Limits {
        memory: 512 * MIB,
        ..limits
    });
    assert_eq!(h.backend.wake(&again.handle).unwrap(), Woke::Exact);
    again.answers("woken");
    // The first hog's OOM, before anything held it, counts once it is.
    hog.apply(limits);
    wait("a pause for the OOM", PATIENCE, || {
        hog.usage().paused == Some(Limit::Memory)
    });
}

#[test]
fn a_fork_refused_at_pids_max_pauses_it() {
    let _one = serial();
    let h = harness!(SCRIPT, Intervals::default());
    let limits = Limits {
        max_processes: 6,
        ..roomy()
    };
    let a = Agent::start(&h, "forks", roomy(), "echo up");
    a.wait_screen("Ready.");
    a.apply(limits);
    // Its threads are tasks too: the kernel gives each process room for
    // them, so only processes count against max_processes.
    assert_eq!(
        a.file("pids.max"),
        (6 * vibe_env::cgroups::TASKS_PER_PROCESS).to_string()
    );
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(a.usage().paused, None, "under its limit, it runs");
    // More processes than its limit pause it.
    let b = Agent::start(
        &h,
        "many",
        roomy(),
        "for i in 1 2 3 4 5 6 7 8 9 10; do sleep 300 & done; echo forked",
    );
    b.wait_screen("Ready.");
    b.apply(limits);
    wait("a process pause", PATIENCE, || {
        b.usage().paused == Some(Limit::Processes)
    });
    assert!(b.usage().processes > 6, "{:?}", b.usage());
    // And so does a fork the kernel refused at pids.max, here 8 tasks.
    let tight = Limits {
        max_processes: 1,
        ..roomy()
    };
    let c = Agent::start(
        &h,
        "bomb",
        tight,
        "for i in 1 2 3 4 5 6 7 8 9 10 11 12; do sleep 300 & done; echo forked",
    );
    c.apply(tight);
    wait("a pause for a refused fork", PATIENCE, || {
        c.usage().paused == Some(Limit::Processes)
    });
    assert!(c.event("pids.events", "max") >= 1);
    assert!(c.cgroup().pids().len() as u64 <= vibe_env::cgroups::TASKS_PER_PROCESS);
    a.answers("fine");
}

#[test]
fn membership_is_the_cgroup() {
    let _one = serial();
    let h = harness!(SCRIPT, Intervals::default());
    let a = Agent::start(
        &h,
        "members",
        roomy(),
        "setsid sleep 301 </dev/null >/dev/null 2>&1 & nohup sleep 302 >/dev/null 2>&1 &",
    );
    a.wait_screen("Ready.");
    let args = |p: &Proc| p.args.join(" ");
    wait("both jobs", PATIENCE, || {
        let all: Vec<String> = a.procs().iter().map(args).collect();
        all.iter().any(|x| x == "sleep 301") && all.iter().any(|x| x == "sleep 302")
    });
    let escaped = a
        .procs()
        .into_iter()
        .find(|p| args(p) == "sleep 301")
        .unwrap();
    // It left the session, and is still the agent's.
    let sid = vibe_env::tree::process(escaped.pid).unwrap().1;
    assert_ne!(sid, a.handle.session.unwrap());
    // Asleep, every one of them is frozen, the escaped one too.
    h.backend.sleep(&a.handle).unwrap();
    assert!(a.procs().iter().all(|p| p.stopped), "{:?}", a.procs());
    assert_eq!(h.backend.wake(&a.handle).unwrap(), Woke::Exact);
    // A stop ends them all, the shim too, and the scope goes.
    let cg = a.cgroup();
    h.backend.stop(&a.handle).unwrap();
    assert!(!cg.dir.exists());
    assert!(vibe_env::tree::process(escaped.pid).is_none());
    assert!(SessionTree::new(a.handle.session.unwrap()).is_empty());
    assert_eq!(
        systemctl(&[
            "show",
            "--property=LoadState",
            "--value",
            &h.backend.unit("members")
        ]),
        "not-found"
    );
}

#[test]
fn a_pause_outlives_vibed() {
    let _one = serial();
    let h = harness!();
    let case = h.limit_case();
    let a = Agent::start(&h, "outlives", roomy(), &case.prompt);
    a.wait_screen("Ready.");
    a.apply(case.limits);
    wait("a pause", PATIENCE, || {
        a.usage().paused == Some(Limit::Processes)
    });
    // A new backend, as a restarted vibed has: the kernel kept the agent
    // frozen, and the backend knows why.
    let again = Cgroups::new(h.root().join("home"))
        .with_claude(h.root().join("bin/claude"))
        .with_path(std::env::var("PATH").unwrap_or_default())
        .with_slice(h.backend.slice());
    h.backend.release();
    assert_eq!(
        again.usage(&a.handle).unwrap().paused,
        Some(Limit::Processes)
    );
    assert!(
        again
            .processes(&a.handle)
            .unwrap()
            .iter()
            .all(|p| p.stopped)
    );
    again.apply(&a.handle, &case.raised).unwrap();
    assert_eq!(again.wake(&a.handle).unwrap(), Woke::Exact);
    assert_eq!(again.usage(&a.handle).unwrap().paused, None);
    a.answers("back");
}

#[test]
fn a_full_disk_warns_then_pauses() {
    let _one = serial();
    let quick = Intervals {
        watch: Duration::from_millis(100),
        disk: Duration::from_millis(200),
    };
    let h = harness!(SCRIPT, quick);
    let limits = Limits {
        disk_soft: 2 * MIB,
        disk_hard: 5 * MIB,
        ..roomy()
    };
    let a = Agent::start(
        &h,
        "disk",
        roomy(),
        "head -c 3145728 /dev/zero >a; sleep 2; head -c 3145728 /dev/zero >b; echo filled",
    );
    a.apply(limits);
    wait("a disk warning", PATIENCE, || {
        a.usage().warnings.contains(&Limit::Disk)
    });
    assert_eq!(a.usage().paused, None);
    wait("a disk pause", PATIENCE, || {
        a.usage().paused == Some(Limit::Disk)
    });
    assert!(a.usage().disk.unwrap() >= 5 * MIB);
    // Room made and woken, it measures again and runs on.
    fs::remove_file(a.handle.worktree.join("a")).unwrap();
    fs::remove_file(a.handle.worktree.join("b")).unwrap();
    assert_eq!(h.backend.wake(&a.handle).unwrap(), Woke::Exact);
    a.wait_screen("filled");
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(a.usage().paused, None);
}

#[test]
fn a_run_keeps_to_the_limits_and_ends_what_it_leaves() {
    let _one = serial();
    let h = harness!();
    let spec = h.spec_with(
        "runs",
        Limits {
            memory: 36 * MIB,
            ..roomy()
        },
    );
    let handle = h.backend.create(&spec).unwrap();
    let out = h
        .backend
        .run(
            &handle,
            &Run {
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "setsid sleep 303 </dev/null >/dev/null 2>&1 & \
                     d=/sys/fs/cgroup$(sed 's/^0:://' /proc/self/cgroup); \
                     echo \"in $d\"; echo \"max $(cat $d/memory.max)\"; echo \"tmp $TMPDIR\""
                        .into(),
                ],
                env: vec![],
                cwd: spec.worktree.clone(),
                timeout: PATIENCE,
            },
        )
        .unwrap();
    let text = String::from_utf8_lossy(&out.output).into_owned();
    assert!(out.success(), "{text}");
    assert!(text.contains(":run-"), "a scope of its own: {text}");
    assert!(text.contains(&format!("max {}", 36 * MIB)), "{text}");
    assert!(
        text.contains(&format!("tmp {}", spec.agent_dir.join("tmp").display())),
        "{text}"
    );
    let dir = text
        .lines()
        .find_map(|l| l.strip_prefix("in "))
        .unwrap()
        .to_owned();
    assert!(
        !Path::new(&dir).exists(),
        "its scope is gone, and the sleep with it"
    );
    h.backend.delete(&handle).unwrap();
}

#[test]
fn a_cpu_cap_throttles() {
    let _one = serial();
    let h = harness!(SCRIPT, Intervals::default());
    let (sd, _) = h.backend.check().unwrap();
    if !sd.cpu() {
        eprintln!("no cpu controller here, so no CPU cap to test");
        return;
    }
    if std::thread::available_parallelism().map_or(1, |n| n.get()) < 2 {
        eprintln!("one CPU: a cap of one core can't throttle anything");
        return;
    }
    // Two busy loops for a second against one core.
    let a = Agent::start(
        &h,
        "spin",
        roomy(),
        "( while :; do :; done ) & a=$!; ( while :; do :; done ) & b=$!; sleep 1; kill $a $b; echo spun",
    );
    a.wait_screen("spun");
    assert!(
        a.event("cpu.stat", "nr_throttled") >= 1,
        "{}",
        a.file("cpu.stat")
    );
}

#[test]
fn a_wake_without_its_shim_is_a_resume_and_leaves_nothing() {
    let _one = serial();
    let h = harness!(SCRIPT, Intervals::default());
    let a = Agent::start(&h, "orphan", roomy(), "nohup sleep 304 >/dev/null 2>&1 &");
    a.wait_screen("Ready.");
    h.backend.sleep(&a.handle).unwrap();
    // The shim dies; what it hung up, and its nohup job, are left frozen.
    let sid = a.handle.session.unwrap();
    vibe_env::tree::signal(sid, libc::SIGKILL);
    wait("the shim to go", PATIENCE, || {
        vibe_env::tree::process(sid).is_none()
    });
    assert!(!a.cgroup().pids().is_empty());
    assert_eq!(h.backend.wake(&a.handle).unwrap(), Woke::Resume);
    assert!(!a.cgroup().dir.exists());
    // And a launch starts it again in a fresh scope of the same name.
    let mut a = a;
    a.launch("echo again");
    a.wait_screen("again");
    assert!(a.cgroup().exists());
    a.answers("fresh");
}

#[test]
fn without_swap_a_hog_is_paused_before_the_oom_killer_picks_anything() {
    let _one = serial();
    let h = harness!(SCRIPT, Intervals::default());
    let limits = Limits {
        memory: 32 * MIB,
        ..roomy()
    };
    let a = Agent::start(&h, "noswap", limits, "echo up");
    a.wait_screen("Ready.");
    a.apply(limits);
    // A server with no swap, as many VPSes are.
    fs::write(a.cgroup().dir.join("memory.swap.max"), "0").unwrap();
    a.type_line("go");
    let hog = "( x=$(head -c 134217728 /dev/zero | tr '\\0' a); echo kept ${#x} ) &";
    let b = Agent::start(&h, "noswap2", limits, hog);
    b.apply(limits);
    fs::write(b.cgroup().dir.join("memory.swap.max"), "0").unwrap();
    wait("a memory pause", Duration::from_secs(60), || {
        b.usage().paused == Some(Limit::Memory)
    });
    // Throttled at memory.high, it was caught there: nothing was killed.
    assert_eq!(
        b.event("memory.events", "oom_kill"),
        0,
        "{}",
        b.file("memory.events")
    );
    assert!(b.event("memory.events", "high") >= 1);
    a.answers("unbothered");
}
