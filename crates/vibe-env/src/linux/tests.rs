//! The `linux` backend's policy, on a fake `vibe-vm` and guest. The real
//! thing runs on a Mac: `crates/vibed/tests/linux_vm.rs`.

use super::*;
use crate::images::Images;
use serde_json::json;
use std::collections::BTreeMap;

const TOKEN: &str = "sk-ant-oat01-test-token-for-the-linux-backend-0123456789";

/// One fake VM: what `vibe-vm` and `vibe-guest` would say about it.
#[derive(Debug, Default, Clone)]
struct Vm {
    running: bool,
    frozen: bool,
    /// None: nothing launched; Some(None): running; Some(Some(code)): exited.
    session: Option<Option<i32>>,
    config: PathBuf,
}

#[derive(Debug, Default)]
struct State {
    vms: HashMap<PathBuf, Vm>,
    calls: Vec<String>,
    launches: Vec<guest::Launch>,
    /// The next restore fails with `restore_failed`.
    restore_fails: bool,
    /// What a `run` writes before it exits, or `None` to hang.
    run_output: Option<(String, i32)>,
    /// A share the guest fails to mount.
    unmounted: Option<String>,
    runs: Vec<proto::Run>,
}

#[derive(Debug, Default)]
struct Fake {
    state: Mutex<State>,
}

impl Fake {
    fn calls(&self) -> Vec<String> {
        lock(&self.state).calls.clone()
    }

    fn vm(&self, socket: &Path) -> Vm {
        lock(&self.state)
            .vms
            .get(socket)
            .cloned()
            .unwrap_or_default()
    }

    fn with<T>(&self, socket: &Path, call: &str, f: impl FnOnce(&mut Vm) -> T) -> T {
        let mut s = lock(&self.state);
        s.calls.push(call.to_owned());
        f(s.vms.entry(socket.to_owned()).or_default())
    }

    fn status_of(vm: &Vm, unmounted: Option<&str>) -> guest::Status {
        let shares: Vec<_> = VmConfig::load(&vm.config)
            .map(|c| c.shares)
            .unwrap_or_default()
            .into_iter()
            .filter(|s| Some(s.tag.as_str()) != unmounted)
            .map(|s| json!({"tag": s.tag, "path": s.path}))
            .collect();
        serde_json::from_value(json!({
            "protocol": proto::PROTOCOL,
            "version": "0.1.0",
            "boot_id": "b",
            "uptime_ms": 10,
            "state": match vm.session {
                None => "idle",
                Some(None) => "running",
                Some(Some(_)) => "exited",
            },
            "frozen": vm.frozen,
            "memory": {"total": 4u64 << 30, "available": 3u64 << 30},
            "layers": ["managed", "base"],
            "shares": shares,
        }))
        .unwrap()
    }
}

fn refused(what: &str) -> EnvError {
    EnvError::new(format!("fake: {what}"))
}

impl Machine for Fake {
    fn create(&self, config: &Path) -> Result<()> {
        lock(&self.state).calls.push("create".into());
        let mut c = VmConfig::load(config).unwrap();
        c.machine_id = Some("AAAA".into());
        c.mac = Some("02:00:00:00:00:01".into());
        c.save(config).unwrap();
        Ok(())
    }

    fn boot(&self, config: &Path, socket: &Path, _log: &Path) -> std::result::Result<(), VmError> {
        let c = VmConfig::load(config).unwrap();
        assert!(c.machine_id.is_some(), "booted before create");
        let _ = fs::remove_file(&c.snapshot);
        self.with(socket, "boot", |vm| {
            *vm = Vm {
                running: true,
                config: config.to_owned(),
                ..Vm::default()
            }
        });
        Ok(())
    }

    fn restore(
        &self,
        config: &Path,
        socket: &Path,
        _log: &Path,
    ) -> std::result::Result<(), VmError> {
        let c = VmConfig::load(config).unwrap();
        let fails = std::mem::take(&mut lock(&self.state).restore_fails);
        self.with(socket, "restore", |vm| {
            if fails {
                return Err(VmError::Refused(vm::Failure {
                    code: vm::ErrorCode::RestoreFailed,
                    error: "invalid argument".into(),
                }));
            }
            fs::remove_file(&c.snapshot).expect("a snapshot to restore");
            vm.running = true;
            Ok(())
        })
    }

    fn running(&self, socket: &Path) -> bool {
        self.vm(socket).running
    }

    fn sleep(&self, socket: &Path, balloon_floor: u64) -> Result<()> {
        assert!(balloon_floor > 0);
        let config = self.vm(socket).config;
        let c = VmConfig::load(&config).unwrap();
        self.with(socket, "sleep", |vm| {
            assert!(vm.frozen, "saved before the freeze");
            vm.running = false;
        });
        fs::write(&c.snapshot, "ram").unwrap();
        Ok(())
    }

    fn stop(&self, socket: &Path) -> Result<()> {
        self.with(socket, "stop", |vm| {
            vm.running = false;
            vm.session = None;
        });
        Ok(())
    }

    fn wait_gone(&self, socket: &Path, _: Duration) -> bool {
        !self.vm(socket).running
    }

    fn ready(&self, socket: &Path, _: Duration) -> Result<guest::Status> {
        self.status(socket)
    }

    fn status(&self, socket: &Path) -> Result<guest::Status> {
        let vm = self.vm(socket);
        if !vm.running {
            return Err(refused("not running"));
        }
        let unmounted = lock(&self.state).unmounted.clone();
        Ok(Fake::status_of(&vm, unmounted.as_deref()))
    }

    fn launch(&self, socket: &Path, launch: &guest::Launch) -> Result<()> {
        let mut s = lock(&self.state);
        s.calls.push("launch".into());
        s.launches.push(launch.clone());
        let vm = s.vms.entry(socket.to_owned()).or_default();
        if !vm.running || vm.frozen || vm.session == Some(None) {
            return Err(refused("busy"));
        }
        vm.session = Some(None);
        Ok(())
    }

    fn processes(&self, socket: &Path) -> Result<guest::Processes> {
        let vm = self.vm(socket);
        let procs = if vm.session == Some(None) {
            json!([{"pid": 214, "ppid": 1, "name": "claude", "args": ["claude"],
                    "state": "S", "started_ms": 1, "cpu_ms": 5, "rss": 1000}])
        } else {
            json!([])
        };
        Ok(serde_json::from_value(json!({"uptime_ms": 1, "processes": procs})).unwrap())
    }

    fn freeze(&self, socket: &Path) -> Result<guest::Frozen> {
        self.with(socket, "freeze", |vm| vm.frozen = true);
        Ok(serde_json::from_value(json!({
            "processes": 1, "freeze_ms": 1, "sync_ms": 1,
            "memory": {"total": 4u64 << 30, "available": 3u64 << 30},
        }))
        .unwrap())
    }

    fn thaw(&self, socket: &Path, _: SystemTime) -> Result<guest::Thawed> {
        self.with(socket, "thaw", |vm| vm.frozen = false);
        Ok(serde_json::from_value(json!({"shares_ms": 900, "thaw_ms": 1})).unwrap())
    }

    fn poweroff(&self, socket: &Path) -> Result<()> {
        self.with(socket, "poweroff", |vm| {
            vm.running = false;
            vm.session = None;
        });
        Ok(())
    }

    fn run(&self, socket: &Path, run: &proto::Run) -> Result<proto::Ran> {
        let mut st = lock(&self.state);
        st.calls.push("run".into());
        st.runs.push(run.clone());
        let vm = st.vms.get(socket).cloned().unwrap_or_default();
        if !vm.running {
            return Err(refused("not running"));
        }
        // As vibe-guest says it went; out of time, killed.
        Ok(match st.run_output.clone() {
            Some((output, code)) => proto::Ran {
                code: Some(code),
                signal: None,
                timed_out: false,
                output,
            },
            None => proto::Ran {
                code: None,
                signal: Some("SIGKILL".into()),
                timed_out: true,
                output: String::new(),
            },
        })
    }
}

struct Setup {
    tmp: tempfile::TempDir,
    layout: Layout,
    fake: Arc<Fake>,
    backend: LinuxVm,
}

impl Setup {
    fn new() -> Setup {
        Setup::with_token(Some(TOKEN))
    }

    fn with_token(token: Option<&'static str>) -> Setup {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path(), "macos");
        // A base release and Claude, as vibed fetches them.
        let base = layout.linux_dir().join("base-1");
        fs::create_dir_all(&base).unwrap();
        for f in ["Image", "initrd", "base.img"] {
            fs::write(base.join(f), f).unwrap();
        }
        let bin = tmp.path().join("claude-bin");
        fs::write(&bin, "claude").unwrap();
        Images::new(layout.linux_dir())
            .add_managed("2.1.308", &bin, "x")
            .unwrap();
        let fake = Arc::new(Fake::default());
        let source: TokenSource = Arc::new(move || {
            token.map(str::to_owned).ok_or_else(|| {
                EnvError::new("linux agents have no Claude login yet: run `vibe setup-token`")
            })
        });
        let backend = LinuxVm::with_machine(layout.clone(), fake.clone(), source)
            .with_disk_check(Duration::from_millis(20));
        Setup {
            tmp,
            layout,
            fake,
            backend,
        }
    }

    fn spec(&self, r: &str) -> AgentSpec {
        let worktree = self.tmp.path().join("VibeOS/worktrees/app").join(r);
        let git_dir = self.tmp.path().join("VibeOS/projects/app/.git");
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir_all(&git_dir).unwrap();
        let agent_dir = self.layout.agent_dir(r);
        fs::create_dir_all(&agent_dir).unwrap();
        AgentSpec {
            agent_ref: r.into(),
            project: "app".into(),
            worktree,
            git_dir,
            agent_dir,
            limits: Limits::new(4 << 30, &HostLimits::default()),
        }
    }

    fn config(&self, h: &Handle) -> VmConfig {
        VmConfig::load(&h.agent_dir.join("vm.json")).unwrap()
    }

    fn socket(&self, r: &str) -> PathBuf {
        self.layout.vm_socket(r)
    }

    fn launch(&self, h: &Handle) -> Result<TerminalEnd> {
        self.backend.launch(
            h,
            &Launch {
                args: vec!["--session-id".into(), "s".into(), "--".into(), "hi".into()],
                env: vec![("GIT_CONFIG_COUNT".into(), "1".into())],
                cwd: h.worktree.clone(),
            },
        )
    }
}

#[test]
fn create_writes_the_vm_as_the_contract_says() {
    let s = Setup::new();
    // The Linux Claude config, made in the toolbox.
    let config = s.layout.linux_claude_dir();
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("CLAUDE.md"), "Be brief.").unwrap();
    let spec = s.spec("quiet-amber-otter");
    let h = s.backend.create(&spec).unwrap();
    assert_eq!(h.claude, ClaudeHome::own(spec.agent_dir.join("claude")));
    assert!(h.claude.own);
    assert_eq!(
        fs::read_to_string(h.claude.dir.join("CLAUDE.md")).unwrap(),
        "Be brief."
    );
    let c = s.config(&h);
    assert_eq!(c.machine_id.as_deref(), Some("AAAA"), "vibe-vm create ran");
    assert_eq!(c.memory, 4 << 30);
    assert!(c.cpus >= 1 && c.cpus <= 4);
    let base = s.layout.linux_dir().join("base-1");
    assert_eq!(c.kernel, base.join("Image"));
    assert_eq!(c.initrd, Some(base.join("initrd")));
    let disks: Vec<_> = c
        .disks
        .iter()
        .map(|d| (d.id.as_str(), d.path.clone(), d.read_only))
        .collect();
    assert_eq!(
        disks,
        [
            ("base", base.join("base.img"), true),
            (
                "managed",
                s.layout.linux_dir().join("managed-2.1.308.img"),
                true
            ),
            ("agent", spec.agent_dir.join("disk.img"), false),
        ]
    );
    let disk = fs::metadata(spec.agent_dir.join("disk.img")).unwrap();
    assert_eq!(
        disk.len(),
        HostLimits::default().vm_disk,
        "sparse, vm_disk big"
    );
    let tags: Vec<_> = c.shares.iter().map(|s| s.tag.as_str()).collect();
    assert_eq!(
        tags,
        [
            "worktree",
            "git",
            "claude",
            "cache-npm",
            "cache-pnpm",
            "cache-cargo",
            "cache-cargo-git",
            "cache-pip"
        ]
    );
    assert!(c.shares.iter().all(|s| !s.read_only && s.path.is_dir()));
    assert_eq!(c.shares[2].path, spec.agent_dir.join("claude"));
    assert_eq!(c.shares[3].path, s.layout.cache_dir().join("npm"));
    let (boot, problems) = boot::Boot::parse(&c.cmdline);
    assert!(problems.is_empty(), "{problems:?}");
    assert!(
        c.cmdline.starts_with("console=hvc0 quiet "),
        "{}",
        c.cmdline
    );
    assert_eq!(boot.layers, ["managed", "base"]);
    assert_eq!(boot.upper.as_deref(), Some("agent"));
    let at: BTreeMap<_, _> = boot
        .shares
        .iter()
        .map(|s| (s.tag.as_str(), s.path.as_str()))
        .collect();
    assert_eq!(at["worktree"], spec.worktree.to_str().unwrap());
    assert_eq!(at["git"], spec.git_dir.to_str().unwrap());
    assert_eq!(at["claude"], "/home/vibe/.claude");
    assert_eq!(at["cache-pip"], "/home/vibe/.cache/pip");
    // SAFETY: getuid has no memory effects.
    assert_eq!(boot.uid, Some(unsafe { libc::getuid() }));
    assert_eq!(c.snapshot, s.layout.snapshot("quiet-amber-otter"));
    assert_eq!(s.fake.calls(), ["create"], "nothing boots until a launch");
}

#[test]
fn package_caches_can_be_opted_out_of() {
    let s = Setup::new();
    fs::create_dir_all(s.layout.config_dir()).unwrap();
    fs::write(
        s.layout.host_config(),
        "package_caches = false\nvm_disk = \"2G\"\n",
    )
    .unwrap();
    let h = s.backend.create(&s.spec("calm-blue-heron")).unwrap();
    let c = s.config(&h);
    let tags: Vec<_> = c.shares.iter().map(|s| s.tag.as_str()).collect();
    assert_eq!(tags, ["worktree", "git", "claude"]);
    assert!(!c.cmdline.contains("cache-"));
    assert_eq!(
        fs::metadata(h.agent_dir.join("disk.img")).unwrap().len(),
        2 << 30
    );
}

#[test]
fn nothing_to_boot_says_why() {
    let s = Setup::new();
    fs::remove_file(s.layout.linux_dir().join("managed-2.1.308.img")).unwrap();
    let e = s.backend.create(&s.spec("a-b-c")).unwrap_err();
    assert!(e.message().contains("Claude Code for linux agents"), "{e}");
    fs::remove_dir_all(s.layout.linux_dir().join("base-1")).unwrap();
    let e = s.backend.create(&s.spec("a-b-d")).unwrap_err();
    assert!(e.message().contains("no Linux base image"), "{e}");
}

#[test]
fn launch_boots_and_starts_claude_with_the_token() {
    let s = Setup::new();
    let h = s.backend.create(&s.spec("quiet-amber-otter")).unwrap();
    let end = s.launch(&h).unwrap();
    let socket = s.socket("quiet-amber-otter");
    assert_eq!(
        end,
        TerminalEnd::Vm {
            socket: socket.clone()
        }
    );
    assert_eq!(s.fake.calls(), ["create", "boot", "launch"]);
    let launch = lock(&s.fake.state).launches[0].clone();
    assert_eq!(launch.command, ["claude", "--session-id", "s", "--", "hi"]);
    assert_eq!(launch.cwd.as_deref(), h.worktree.to_str());
    assert_eq!(launch.env["CLAUDE_CODE_OAUTH_TOKEN"], TOKEN);
    assert_eq!(launch.env["CLAUDE_CONFIG_DIR"], "/home/vibe/.claude");
    assert_eq!(launch.env["DISABLE_AUTOUPDATER"], "1");
    assert_eq!(launch.env["DISABLE_INSTALLATION_CHECKS"], "1");
    assert_eq!(launch.env["GIT_CONFIG_COUNT"], "1");
    assert!(launch.time_ms.is_some(), "the clock is set first");
    // The token is in no file VibeOS keeps.
    for dir in [s.layout.agents_dir(), s.layout.linux_dir()] {
        for f in walk(&dir) {
            // The sparse disk is empty, and too big to read.
            if f.is_file() && fs::metadata(&f).unwrap().len() < 1 << 20 {
                let bytes = fs::read(&f).unwrap();
                assert!(
                    !String::from_utf8_lossy(&bytes).contains(TOKEN),
                    "{} has the token",
                    f.display()
                );
            }
        }
    }
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&h.claude.state).unwrap()).unwrap();
    assert_eq!(state["installMethod"], "native");
    assert_eq!(state["autoUpdates"], false);

    // A running VM isn't booted again.
    s.fake.with(&socket, "-", |vm| vm.session = Some(Some(0)));
    s.launch(&h).unwrap();
    assert_eq!(s.fake.calls().iter().filter(|c| *c == "boot").count(), 1);
    let procs = s.backend.processes(&h).unwrap();
    assert_eq!(procs.len(), 1);
    assert_eq!(procs[0].name, "claude");
    assert_eq!(s.backend.usage(&h).unwrap().memory, 1000);
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            }
            out.push(p);
        }
    }
    out
}

#[test]
fn without_a_login_nothing_boots() {
    let s = Setup::with_token(None);
    let h = s.backend.create(&s.spec("quiet-amber-otter")).unwrap();
    let e = s.launch(&h).unwrap_err();
    assert!(e.message().contains("vibe setup-token"), "{e}");
    assert_eq!(s.fake.calls(), ["create"]);
}

#[test]
fn a_new_claude_release_is_in_the_next_fresh_boot_not_a_restore() {
    let s = Setup::new();
    let h = s.backend.create(&s.spec("quiet-amber-otter")).unwrap();
    let images = Images::new(s.layout.linux_dir());
    let managed = |h: &Handle| {
        s.config(h)
            .disks
            .iter()
            .find(|d| d.id == "managed")
            .unwrap()
            .path
            .clone()
    };
    let first = managed(&h);
    s.launch(&h).unwrap();
    assert_eq!(managed(&h), first);
    let identity = (s.config(&h).machine_id, s.config(&h).mac);

    // Claude 2.1.314 lands while the agent sleeps.
    s.backend.sleep(&h).unwrap();
    let bin = s.tmp.path().join("claude-new");
    fs::write(&bin, "newer").unwrap();
    let newer = images.add_managed("2.1.314", &bin, "y").unwrap();
    // Its snapshot restores into the disks it was saved with.
    assert_eq!(
        s.backend.wake(&h).unwrap(),
        Woke::Reattach(TerminalEnd::Vm {
            socket: s.socket("quiet-amber-otter")
        })
    );
    assert_eq!(managed(&h), first);

    // A fresh boot takes the new layer, and keeps the VM's identity.
    s.backend.stop(&h).unwrap();
    s.launch(&h).unwrap();
    assert_eq!(managed(&h), newer);
    assert_eq!((s.config(&h).machine_id, s.config(&h).mac), identity);
    let (boot, _) = boot::Boot::parse(&s.config(&h).cmdline);
    assert_eq!(boot.layers, ["managed", "base"]);
    assert_eq!(boot.shares.len(), 8, "the shares are kept");
}

#[test]
fn sleep_saves_and_wake_restores_on_a_new_shim() {
    let s = Setup::new();
    let h = s.backend.create(&s.spec("quiet-amber-otter")).unwrap();
    s.launch(&h).unwrap();
    let snapshot = s.layout.snapshot("quiet-amber-otter");
    s.backend.sleep(&h).unwrap();
    assert!(snapshot.exists());
    assert!(
        !s.fake.vm(&s.socket("quiet-amber-otter")).running,
        "its RAM is freed"
    );
    assert_eq!(s.backend.processes(&h).unwrap(), vec![]);
    let woke = s.backend.wake(&h).unwrap();
    assert_eq!(
        woke,
        Woke::Reattach(TerminalEnd::Vm {
            socket: s.socket("quiet-amber-otter")
        })
    );
    assert!(!snapshot.exists(), "a snapshot is never restored twice");
    let calls = s.fake.calls();
    assert_eq!(
        &calls[calls.len() - 4..],
        ["freeze", "sleep", "restore", "thaw"]
    );
    assert!(!s.fake.vm(&s.socket("quiet-amber-otter")).frozen);

    // Awake and running, with its shim gone: a new shim reattaches. With
    // Claude ended in the VM: it resumes.
    assert!(matches!(s.backend.wake(&h).unwrap(), Woke::Reattach(_)));
    s.fake.with(&s.socket("quiet-amber-otter"), "-", |vm| {
        vm.session = Some(Some(0))
    });
    assert_eq!(s.backend.wake(&h).unwrap(), Woke::Resume);
}

#[test]
fn a_snapshot_that_wont_restore_is_dropped_for_a_resume() {
    let s = Setup::new();
    let h = s.backend.create(&s.spec("quiet-amber-otter")).unwrap();
    s.launch(&h).unwrap();
    s.backend.sleep(&h).unwrap();
    lock(&s.fake.state).restore_fails = true;
    assert_eq!(s.backend.wake(&h).unwrap(), Woke::Resume);
    assert!(!s.layout.snapshot("quiet-amber-otter").exists());
    // Then the launch is a fresh boot.
    s.launch(&h).unwrap();
    assert_eq!(s.fake.calls().last().unwrap(), "launch");
    // Nothing to wake from at all: a resume too.
    s.backend.stop(&h).unwrap();
    assert_eq!(s.backend.wake(&h).unwrap(), Woke::Resume);
}

#[test]
fn a_command_runs_to_completion_inside_beside_claude() {
    let s = Setup::new();
    let h = s.backend.create(&s.spec("quiet-amber-otter")).unwrap();
    lock(&s.fake.state).run_output = Some(("installing\ndone\n".into(), 3));
    let run = |argv: &[&str], timeout| {
        s.backend
            .run(
                &h,
                &Run {
                    argv: argv.iter().map(|a| (*a).to_owned()).collect(),
                    env: vec![("X".into(), "1".into())],
                    cwd: h.worktree.clone(),
                    timeout,
                },
            )
            .unwrap()
    };
    let out = run(&["/bin/sh", ".vibe/setup.sh"], Duration::from_secs(10));
    assert_eq!(out.code, Some(3));
    assert!(!out.timed_out);
    assert_eq!(String::from_utf8(out.output).unwrap(), "installing\ndone\n");
    assert_eq!(
        s.fake.calls(),
        ["create", "boot", "run"],
        "it boots the VM first"
    );
    let sent = lock(&s.fake.state).runs[0].clone();
    assert_eq!(sent.command, ["/bin/sh", ".vibe/setup.sh"]);
    assert_eq!(sent.cwd.as_deref(), h.worktree.to_str());
    assert_eq!(sent.timeout_ms, 10_000);
    assert_eq!(sent.env["X"], "1");
    assert!(
        !sent.env.contains_key("CLAUDE_CODE_OAUTH_TOKEN"),
        "a run gets no token"
    );

    // With Claude running, a run goes ahead beside it, and the VM stays.
    s.launch(&h).unwrap();
    lock(&s.fake.state).run_output = None;
    let out = run(&["sleep", "60"], Duration::from_millis(300));
    assert!(out.timed_out && !out.success(), "{out:?}");
    assert_eq!(out.signal, Some(9));
    let vm = s.fake.vm(&s.socket("quiet-amber-otter"));
    assert!(vm.running && vm.session == Some(None), "Claude goes on");
    assert_eq!(s.fake.calls().last().unwrap(), "run");
}

#[test]
fn the_dirs_it_makes_are_private_whatever_the_umask() {
    use std::os::unix::fs::PermissionsExt;
    let s = Setup::new();
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    // Made open before, as a umask of 022 leaves a new directory.
    for dir in [s.layout.snapshots_dir(), s.layout.cache_dir().join("npm")] {
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let h = s.backend.create(&s.spec("quiet-amber-otter")).unwrap();
    assert_eq!(mode(&s.layout.snapshots_dir()), 0o700);
    assert_eq!(mode(&h.claude.dir), 0o700);
    for (_, name, _) in CACHES {
        assert_eq!(mode(&s.layout.cache_dir().join(name)), 0o700, "{name}");
    }
}

/// A real `vibe-shim` on `command`, as on an agent's terminal: its pid,
/// which leads its session, and its socket.
fn real_shim(s: &Setup, command: &[&str]) -> (u32, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let shim = {
        let exe = std::env::current_exe().unwrap();
        exe.parent().unwrap().parent().unwrap().join("vibe-shim")
    };
    assert!(shim.is_file(), "{} isn't built", shim.display());
    let run = s.tmp.path().join("r");
    fs::create_dir_all(&run).unwrap();
    fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).unwrap();
    let socket = run.join("q.sock");
    let out = std::process::Command::new(&shim)
        .args(["--ref", "quiet-amber-otter", "--socket"])
        .arg(&socket)
        .arg("--state")
        .arg(s.tmp.path().join("shim-state"))
        .arg("--cwd")
        .arg(s.tmp.path())
        .arg("--")
        .args(command)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pid = String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    (pid, socket)
}

fn shim_runs(sid: u32) -> bool {
    SessionTree::new(sid).all().iter().any(|p| p.pid == sid)
}

#[test]
fn a_sleep_returns_once_the_shim_has_gone() {
    // A shim on a command that ends a moment later, as one on a VM's
    // stream ends once vibe-vm has.
    let s = Setup::new();
    let mut h = s.backend.create(&s.spec("quiet-amber-otter")).unwrap();
    s.launch(&h).unwrap();
    let (sid, _) = real_shim(&s, &["sleep", "1"]);
    h.session = Some(sid);
    let started = Instant::now();
    s.backend.sleep(&h).unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(500),
        "it didn't wait"
    );
    assert!(!shim_runs(sid), "the shim is still there");
}

#[test]
fn a_stop_ends_the_shim_so_a_delete_can_remove_its_socket() {
    // After a wake, a shim holds the VM's terminal; a forced delete stops
    // the agent and then removes the shim's socket, which must be free.
    let s = Setup::new();
    let mut h = s.backend.create(&s.spec("quiet-amber-otter")).unwrap();
    s.launch(&h).unwrap();
    // One that goes on its own a moment after the VM: waited for.
    let (sid, socket) = real_shim(&s, &["sleep", "1"]);
    h.session = Some(sid);
    s.backend.stop(&h).unwrap();
    assert!(!shim_runs(sid), "stop returned with the shim running");
    assert!(!socket.exists(), "its socket is still served");
    // One that wouldn't go: ended.
    let (sid, socket) = real_shim(&s, &["sleep", "60"]);
    h.session = Some(sid);
    s.launch(&h).unwrap();
    let started = Instant::now();
    s.backend.delete(&h).unwrap();
    assert!(!shim_runs(sid), "delete left the shim running");
    assert!(!socket.exists(), "its socket is still served");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_full_worktree_pauses_the_agent_until_a_wake() {
    let s = Setup::new();
    let h = s.backend.create(&s.spec("quiet-amber-otter")).unwrap();
    s.launch(&h).unwrap();
    let limits = Limits {
        disk_soft: 1 << 20,
        disk_hard: 2 << 20,
        ..Limits::new(4 << 30, &HostLimits::default())
    };
    s.backend.apply(&h, &limits).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(s.backend.usage(&h).unwrap().paused, None);
    fs::write(h.worktree.join("big"), vec![1u8; 3 << 20]).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while s.backend.usage(&h).unwrap().paused.is_none() {
        assert!(Instant::now() < deadline, "never paused");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(s.backend.usage(&h).unwrap().paused, Some(Limit::Disk));
    assert!(s.fake.vm(&s.socket("quiet-amber-otter")).frozen);
    assert!(s.backend.processes(&h).unwrap().iter().all(|p| p.stopped));
    fs::remove_file(h.worktree.join("big")).unwrap();
    s.backend.wake(&h).unwrap();
    assert_eq!(s.backend.usage(&h).unwrap().paused, None);
    assert!(!s.fake.vm(&s.socket("quiet-amber-otter")).frozen);
}

#[test]
fn delete_leaves_nothing_of_the_vm() {
    let s = Setup::new();
    let spec = s.spec("quiet-amber-otter");
    let h = s.backend.create(&spec).unwrap();
    s.launch(&h).unwrap();
    let transcript = h.claude.transcript(&h.worktree, "s");
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    fs::write(&transcript, "{}\n").unwrap();
    fs::write(spec.agent_dir.join("console.log"), "boot").unwrap();
    fs::write(spec.agent_dir.join("vibe-vm.log"), "log").unwrap();
    s.backend.sleep(&h).unwrap();
    fs::create_dir_all(s.layout.run_dir()).unwrap();
    fs::write(s.layout.run_dir().join("quiet-amber-otter.vm.lock"), "").unwrap();
    s.backend.delete(&h).unwrap();
    let left: Vec<_> = walk(&spec.agent_dir);
    assert!(left.is_empty(), "left in the agent's directory: {left:?}");
    assert!(!s.layout.snapshot("quiet-amber-otter").exists());
    assert!(
        !s.layout
            .run_dir()
            .join("quiet-amber-otter.vm.lock")
            .exists()
    );
    // The shared caches and images stay.
    assert!(s.layout.cache_dir().join("npm").is_dir());
    assert!(s.layout.linux_dir().join("managed-2.1.308.img").exists());
    // Deleting a running one powers it off first.
    let h = s.backend.create(&s.spec("calm-blue-heron")).unwrap();
    s.launch(&h).unwrap();
    s.backend.delete(&h).unwrap();
    assert!(!s.fake.vm(&s.socket("calm-blue-heron")).running);
    assert_eq!(s.fake.calls().last().unwrap(), "poweroff");
}

#[test]
fn capabilities_are_a_vms() {
    let s = Setup::new();
    let c = s.backend.capabilities();
    assert!(c.sleep_frees_memory && c.exact_after_reboot && c.kernel_limits && c.file_containment);
    assert_eq!(linux_signal("SIGKILL"), Some(9));
    assert_eq!(linux_signal("SIGTERM"), Some(15));
    assert_eq!(linux_signal("SIGWHAT"), None);
}

#[test]
fn a_share_that_didnt_mount_fails_the_boot() {
    let s = Setup::new();
    let h = s.backend.create(&s.spec("quiet-amber-otter")).unwrap();
    lock(&s.fake.state).unmounted = Some("claude".into());
    let e = s.launch(&h).unwrap_err();
    assert!(e.message().contains("couldn't mount claude"), "{e}");
    assert!(
        !s.fake.vm(&s.socket("quiet-amber-otter")).running,
        "it's stopped"
    );
    assert!(
        !s.fake.calls().contains(&"launch".to_owned()),
        "Claude never starts"
    );
    lock(&s.fake.state).unmounted = None;
    s.launch(&h).unwrap();
}
