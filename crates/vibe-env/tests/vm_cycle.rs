//! Boots, saves and restores real VMs through `vibe-vm`, the way `vibed`
//! will, and holds them to the numbers the spikes measured (PRD: Spike
//! results: VM snapshots). It needs a Mac, a `vibe-vm` signed with the
//! virtualization entitlement, and the test guest from
//! `scripts/test-guest.sh`:
//!
//! ```text
//! VIBE_VM_BIN=build/vibe-vm VIBE_VM_GUEST=build/test-guest \
//!   cargo test -p vibe-env --test vm_cycle -- --test-threads=1 --nocapture
//! ```
//!
//! Without those it skips. `scripts/check-full.sh` runs it, with
//! `VIBE_VM_MARKER` set to a directory where each test that ran leaves a file,
//! so a skip can't pass for a pass. `scripts/vm-reboot-test.sh` runs the
//! `reboot` test's two halves on either side of a reboot.
//!
//! `guest_question_survives_sleep` boots the other test guest, with
//! `vibe-guest` as PID 1, and drives it as `vibed` will, through a real
//! `vibe-shim --vm`: `VIBE_SHIM_BIN`, or `target/debug/vibe-shim`.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use vibe_env::guest::{self as vibe_guest, boot};
use vibe_env::vm::{
    self, Disk, ErrorCode, Launch, Probe, Ready, Share, Slept, Started, VmConfig, VmState,
};

/// The vsock port `vibe-testguest` serves.
const PORT: u32 = 1024;

const MIB: u64 = 1 << 20;

/// Starting `vibe-vm` to the guest's first answer over vsock, on a fresh
/// boot. The spike's tailored guest took 0.35 to 1 s; the test guest loads
/// its drivers from a squashfs first.
const BOOT_LIMIT: Duration = Duration::from_secs(10);

/// vibe-vm's own save of an idle VM, after the balloon: about 1 s in the
/// spike (0.65 s with layered disks, 0.9 s for three at once).
const SAVE_LIMIT_MS: u64 = 2000;

/// vibe-vm's own restore and resume of an idle VM: 0.42 to 0.5 s in the
/// spike.
const RESTORE_LIMIT_MS: u64 = 1000;

/// Starting `vibe-vm restore` to the guest's first answer, for an idle VM:
/// about 0.5 s in the spike.
const RESTORE_LIMIT: Duration = Duration::from_secs(2);

/// An idle VM's snapshot after the balloon, though the guest had touched
/// 512 MiB and freed it. The spike's came to 41 MB.
const BALLOONED_LIMIT: u64 = 128 * MIB;

/// How many times a timed save or restore is tried, the best counting. On
/// a Mac doing other work too, one sample can be slow for reasons that have
/// nothing to do with vibe-vm; the limits above are what an idle one takes,
/// and every try is reported with the load it ran under.
const TIMING_TRIES: usize = 3;

/// The host's load average over the last minute.
fn load() -> f64 {
    let mut avg = [0f64; 3];
    // SAFETY: getloadavg fills at most the 1 sample asked for.
    if unsafe { libc::getloadavg(avg.as_mut_ptr(), 1) } < 1 {
        return f64::NAN;
    }
    avg[0]
}

/// How long one poll of a starting guest waits for its vsock to answer.
const POLL_TIMEOUT: Duration = Duration::from_secs(2);

struct Guest {
    vibe_vm: PathBuf,
    dir: PathBuf,
}

/// The binary and test guest to use, or `None` to skip.
fn guest(test: &str) -> Option<Guest> {
    let vibe_vm = std::env::var_os("VIBE_VM_BIN").map(PathBuf::from);
    let dir = std::env::var_os("VIBE_VM_GUEST").map(PathBuf::from);
    match (vibe_vm, dir) {
        (Some(vibe_vm), Some(dir)) => Some(Guest {
            vibe_vm: absolute(vibe_vm),
            dir: absolute(dir),
        }),
        _ => {
            eprintln!("{test}: skipped; set VIBE_VM_BIN and VIBE_VM_GUEST to run it on a Mac");
            None
        }
    }
}

/// A path from the environment: relative ones are to the repo's root, where
/// the commands above run, not to this crate, where cargo runs the test.
fn absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(path)
    }
}

/// Leaves a mark that `test` really ran.
fn mark(test: &str) {
    if let Some(dir) = std::env::var_os("VIBE_VM_MARKER") {
        let dir = absolute(PathBuf::from(dir));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(test), "ran\n").unwrap();
    }
}

/// A short root under /tmp: socket paths must fit in 104 bytes.
fn short_tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("vmc")
        .tempdir_in("/tmp")
        .unwrap()
}

/// Which PID 1 a test VM boots.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// `vibe-testguest`, which answers on [`PORT`].
    TestGuest,
    /// `vibe-guest`, with `vibe-testguest ask` to run; and with a directory
    /// holding a real `claude`, that shared at `/opt/claude`, and a Claude
    /// home shared at `~/.claude` as `vibed` will.
    VibeGuest { claude: Option<PathBuf> },
}

struct TestVm {
    name: String,
    config_path: PathBuf,
    socket: PathBuf,
    log: PathBuf,
    share: PathBuf,
    config: VmConfig,
    vibe_vm: PathBuf,
    /// The running vibe-vm's pid, to kill it if a failing test can't stop it.
    pid: AtomicU32,
}

/// A failing test must not leave detached VMs running.
impl Drop for TestVm {
    fn drop(&mut self) {
        if !vm::lock_held(&self.socket) {
            return;
        }
        if vm::stop(&self.socket).is_ok() && vm::wait_gone(&self.socket, Duration::from_secs(10)) {
            return;
        }
        let pid = self.pid.load(Ordering::SeqCst);
        if pid != 0 {
            // SAFETY: a plain kill(2) of the vibe-vm this test started.
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
    }
}

impl TestVm {
    /// Writes the VM's `vm.json` under `root` and runs `vibe-vm create` on it.
    fn create(g: &Guest, root: &Path, name: &str, memory: u64, socket: PathBuf) -> TestVm {
        TestVm::create_as(g, root, name, memory, socket, &Kind::TestGuest)
    }

    fn create_as(
        g: &Guest,
        root: &Path,
        name: &str,
        memory: u64,
        socket: PathBuf,
        kind: &Kind,
    ) -> TestVm {
        let dir = root.join(name);
        let share = dir.join("share");
        fs::create_dir_all(&share).unwrap();
        fs::write(share.join("probe"), "before").unwrap();
        // The worktree, as it were, at its host path; with Claude, its
        // binary and its home.
        let mut shares = vec![(share.clone(), share.to_string_lossy().into_owned(), false)];
        if let Kind::VibeGuest {
            claude: Some(claude),
        } = kind
        {
            let home = dir.join("claude-home");
            fs::create_dir_all(&home).unwrap();
            shares.push((claude.clone(), "/opt/claude".into(), true));
            shares.push((home, "/home/vibe/.claude".into(), false));
        }
        let tags = ["share", "claude-bin", "claude-home"];
        let disk = dir.join("disk.img");
        vm::create_disk(&disk, 64 * MIB).unwrap();
        let host_cpus = thread::available_parallelism().map_or(1, |n| n.get());
        let config = VmConfig {
            version: vm::CONFIG_VERSION,
            agent: name.into(),
            cpus: vm::vcpus(2, host_cpus),
            memory,
            kernel: g.dir.join("Image"),
            initrd: Some(g.dir.join(match kind {
                Kind::TestGuest => "initrd",
                Kind::VibeGuest { .. } => "initrd-guest",
            })),
            cmdline: match kind {
                Kind::TestGuest => "console=hvc0".into(),
                // `vibe` as the user running the test.
                Kind::VibeGuest { .. } => boot::Boot {
                    shares: tags
                        .iter()
                        .zip(&shares)
                        .map(|(tag, (_, at, _))| boot::Share {
                            tag: (*tag).into(),
                            path: at.clone(),
                        })
                        .collect(),
                    // SAFETY: getuid and getgid have no memory effects.
                    uid: Some(unsafe { libc::getuid() }),
                    gid: Some(unsafe { libc::getgid() }),
                    ..boot::Boot::default()
                }
                .append_to("console=hvc0")
                .unwrap(),
            },
            machine_id: None,
            mac: None,
            disks: vec![
                Disk {
                    id: "modloop".into(),
                    path: g.dir.join("modloop"),
                    read_only: true,
                },
                Disk {
                    id: "agent".into(),
                    path: disk,
                    read_only: false,
                },
            ],
            shares: tags
                .iter()
                .zip(&shares)
                .map(|(tag, (path, _, read_only))| Share {
                    tag: (*tag).into(),
                    path: path.clone(),
                    read_only: *read_only,
                })
                .collect(),
            console_log: Some(dir.join("console.log")),
            snapshot: root.join("snapshots").join(name),
        };
        let config_path = dir.join("vm.json");
        config.save(&config_path).unwrap();
        let created = vm::create(&g.vibe_vm, &config_path).unwrap();
        let config = VmConfig::load(&config_path).unwrap();
        assert_eq!(config.mac.as_deref(), Some(created.mac.as_str()));
        assert_eq!(
            config.machine_id.as_deref(),
            Some(created.machine_id.as_str())
        );
        TestVm {
            name: name.into(),
            config_path,
            socket,
            log: dir.join("vibe-vm.log"),
            share,
            config,
            vibe_vm: g.vibe_vm.clone(),
            pid: AtomicU32::new(0),
        }
    }

    /// The VM as `load` finds it under `root` after `create`.
    fn load(g: &Guest, root: &Path, name: &str, socket: PathBuf) -> TestVm {
        let dir = root.join(name);
        let config_path = dir.join("vm.json");
        TestVm {
            name: name.into(),
            config: VmConfig::load(&config_path).unwrap(),
            config_path,
            socket,
            log: dir.join("vibe-vm.log"),
            share: dir.join("share"),
            vibe_vm: g.vibe_vm.clone(),
            pid: AtomicU32::new(0),
        }
    }

    fn launch(&self) -> Launch<'_> {
        Launch {
            vibe_vm: &self.vibe_vm,
            config: &self.config_path,
            socket: &self.socket,
            log: &self.log,
        }
    }

    /// The end of the guest's console and of vibe-vm's log, for failures.
    fn logs(&self) -> String {
        let tail = |path: &Path| {
            let text = fs::read_to_string(path).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            lines[lines.len().saturating_sub(40)..].join("\n")
        };
        let console = self.config.console_log.as_deref().unwrap();
        format!(
            "--- {} console:\n{}\n--- vibe-vm log:\n{}",
            self.name,
            tail(console),
            tail(&self.log)
        )
    }

    /// One command to the guest, one JSON line back.
    fn ask(&self, command: &str) -> Result<Value, String> {
        self.ask_within(command, Duration::from_secs(30))
    }

    fn ask_within(&self, command: &str, timeout: Duration) -> Result<Value, String> {
        let mut stream =
            vm::connect_within(&self.socket, PORT, timeout).map_err(|e| e.to_string())?;
        stream.set_read_timeout(Some(timeout)).unwrap();
        stream
            .write_all(format!("{command}\n").as_bytes())
            .map_err(|e| e.to_string())?;
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        let value: Value = serde_json::from_str(&line).map_err(|e| format!("{e}: {line:?}"))?;
        if let Some(e) = value.get("error") {
            return Err(format!("the guest: {e}"));
        }
        Ok(value)
    }

    fn must_ask(&self, command: &str) -> Value {
        self.ask(command)
            .unwrap_or_else(|e| panic!("{command}: {e}\n{}", self.logs()))
    }

    fn tick(&self) -> u64 {
        self.must_ask("tick")["count"].as_u64().unwrap()
    }

    /// Waits for the guest's first answer; how long since `since`.
    fn first_answer(&self, since: Instant, limit: Duration) -> Duration {
        let deadline = Instant::now() + limit.max(Duration::from_secs(30));
        loop {
            match self.ask_within("status", POLL_TIMEOUT) {
                Ok(_) => return since.elapsed(),
                Err(e) if Instant::now() >= deadline => {
                    panic!(
                        "{}: no answer from the guest: {e}\n{}",
                        self.name,
                        self.logs()
                    )
                }
                Err(_) => thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    fn boot(&self) -> Duration {
        let t = Instant::now();
        let ready = vm::boot(&self.launch())
            .unwrap_or_else(|e| panic!("boot {}: {e}\n{}", self.name, self.logs()));
        self.pid.store(ready.pid, Ordering::SeqCst);
        assert_eq!(ready.started, Started::Boot);
        assert!(ready.timings.start_ms.is_some() && ready.timings.total_ms.is_some());
        self.first_answer(t, BOOT_LIMIT)
    }

    fn sleep(&self, balloon_ms: Option<u64>) -> Slept {
        self.sleep_down_to(balloon_ms, None)
    }

    fn sleep_down_to(&self, balloon_ms: Option<u64>, balloon_floor: Option<u64>) -> Slept {
        let slept = vm::sleep(&self.socket, balloon_ms, balloon_floor)
            .unwrap_or_else(|e| panic!("sleep {}: {e}\n{}", self.name, self.logs()));
        assert_eq!(slept.snapshot, self.config.snapshot);
        assert!(self.config.snapshot.exists());
        let t = slept.timings;
        for step in [t.balloon_ms, t.pause_ms, t.save_ms, t.stop_ms, t.total_ms] {
            assert!(step.is_some(), "{t:?}");
        }
        // vibe-vm exits once it has answered, and takes its socket along.
        assert!(
            vm::wait_gone(&self.socket, Duration::from_secs(10)),
            "{}: vibe-vm didn't exit after the sleep\n{}",
            self.name,
            self.logs()
        );
        assert!(!self.socket.exists());
        assert!(matches!(vm::probe(&self.socket), Probe::Gone));
        slept
    }

    /// Restores the VM; vibe-vm's ready line, and how long it took from
    /// starting vibe-vm to the guest's first answer.
    fn restore(&self) -> (Ready, Duration) {
        let t = Instant::now();
        let ready = vm::restore(&self.launch())
            .unwrap_or_else(|e| panic!("restore {}: {e}\n{}", self.name, self.logs()));
        self.pid.store(ready.pid, Ordering::SeqCst);
        assert_eq!(ready.started, Started::Restore);
        let answer = self.first_answer(t, RESTORE_LIMIT);
        // A running VM's disks move on, so its snapshot is gone.
        assert!(!self.config.snapshot.exists());
        (ready, answer)
    }

    fn stop(&self) {
        vm::stop(&self.socket).unwrap();
        assert!(vm::wait_gone(&self.socket, Duration::from_secs(10)));
    }

    /// Waits up to `limit` for the guest's status to show `key` as `want`
    /// (`None`: anything but null).
    fn wait_for(&self, key: &str, want: Option<&str>, limit: Duration) -> Value {
        let deadline = Instant::now() + limit;
        loop {
            let status = self.must_ask("status");
            let ok = match want {
                Some(want) => status[key] == want,
                None => !status[key].is_null(),
            };
            if ok {
                return status;
            }
            if Instant::now() >= deadline {
                panic!(
                    "{}: {key} is {} in {status}, not {want:?}\n{}",
                    self.name,
                    status[key],
                    self.logs()
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// The first line of the agent disk's image, read on the host.
    fn disk_on_host(&self) -> String {
        let disk = &self
            .config
            .disks
            .iter()
            .find(|d| d.id == "agent")
            .unwrap()
            .path;
        let bytes = fs::read(disk).unwrap();
        let end = bytes.iter().position(|&b| b == b'\n' || b == 0).unwrap();
        String::from_utf8_lossy(&bytes[..end]).into_owned()
    }
}

fn mib(bytes: u64) -> u64 {
    bytes / MIB
}

fn ms(timing: Option<u64>) -> u64 {
    timing.expect("a timing")
}

#[test]
fn boot_save_restore() {
    let Some(g) = guest("boot_save_restore") else {
        return;
    };
    let root = short_tempdir();
    let vm = TestVm::create(
        &g,
        root.path(),
        "cycle",
        1024 * MIB,
        root.path().join("cycle.vm"),
    );

    // Boot, with every device there.
    let boot = vm.boot();
    eprintln!("boot to first answer: {} ms", boot.as_millis());
    assert!(boot <= BOOT_LIMIT, "boot took {boot:?}\n{}", vm.logs());
    let Probe::Running(status) = vm::probe(&vm.socket) else {
        panic!("vibe-vm isn't answering")
    };
    assert_eq!(
        (status.state, status.started, status.agent.as_str()),
        (VmState::Running, Started::Boot, "cycle")
    );
    // NAT networking: DHCP gives eth0 an address.
    let first = vm.wait_for("ip", None, Duration::from_secs(15));
    let boot_id = first["boot_id"].clone();
    assert!(boot_id.is_string(), "{first}");
    assert_eq!(
        first["mac"].as_str().map(str::to_lowercase),
        vm.config.mac.clone(),
        "the guest's NIC has the fixed MAC"
    );
    assert_eq!(first["share"], "before", "{first}");
    assert_eq!(first["cpus"].as_u64(), Some(u64::from(vm.config.cpus)));
    assert!(
        first["rng"].as_str().is_some_and(|r| r.contains("virtio")),
        "the entropy device: {first}"
    );

    // One vibe-vm per VM.
    let err = vm::boot(&vm.launch()).unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::AlreadyRunning), "{err}");
    // A connect to a port nobody serves is refused, not hung.
    let err = vm::connect(&vm.socket, 9).unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::ConnectFailed), "{err}");

    assert_eq!((vm.tick(), vm.tick(), vm.tick()), (1, 2, 3));

    // What a snapshot holds (PRD: Spike results). The guest's memory is
    // written with data that doesn't compress: a page of mostly zeros costs
    // a snapshot next to nothing, so it proves nothing. (Writing one byte a
    // page, 512 MiB of touched and freed memory came to a 29 MiB snapshot.)
    eprintln!("the guest's balloon: {}", first["balloon"]);

    // 1. Memory in use is saved whole. The balloon's floor stays above it,
    // as vibed's will (PRD: Sleep and wake).
    let held = vm.must_ask("hold 256")["sum"].clone();
    let slept = vm.sleep_down_to(None, Some(512 * MIB));
    eprintln!(
        "256 MiB in use, balloon down to 512 MiB: {} MiB snapshot, {:?}",
        mib(slept.snapshot_bytes),
        slept.timings
    );
    assert!(
        slept.snapshot_bytes >= 256 * MIB,
        "memory in use must be in the snapshot: {} MiB",
        mib(slept.snapshot_bytes)
    );
    let (ready, answer) = vm.restore();
    eprintln!(
        "restore of {} MiB to first answer: {} ms, {:?}",
        mib(slept.snapshot_bytes),
        answer.as_millis(),
        ready.timings
    );
    assert_eq!(vm.tick(), 4, "the guest carries on exactly");
    let status = vm.must_ask("status");
    assert_eq!(
        (&status["held_mib"], &status["held_sum"]),
        (&Value::from(256), &held),
        "the memory in use came back exactly"
    );
    vm.must_ask("release");

    // 2. Without the balloon. The spike saw memory the guest had freed
    // saved all the same (1.09 GB after freeing 1 GB); with free page
    // reporting the guest may hand it back on its own. Either way this is
    // what the balloon is measured against.
    vm.must_ask("dirty 512");
    let unballooned = vm.sleep(Some(0));
    let reporting = first["balloon"]["free_page_reporting"].as_bool();
    eprintln!(
        "no balloon, 512 MiB touched and freed: {} MiB snapshot ({}; free page reporting: {reporting:?}), {:?}",
        mib(unballooned.snapshot_bytes),
        if unballooned.snapshot_bytes >= 384 * MIB {
            "the freed memory was saved, as in the spike"
        } else {
            "the freed memory stayed out of it"
        },
        unballooned.timings
    );
    let (ready, answer) = vm.restore();
    eprintln!(
        "restore of {} MiB to first answer: {} ms, {:?}",
        mib(unballooned.snapshot_bytes),
        answer.as_millis(),
        ready.timings
    );
    assert_eq!(vm.tick(), 5, "the guest carries on exactly");

    // 3. With it, the snapshot holds only what the guest really uses, and
    // never more than without.
    vm.must_ask("dirty 512");
    // Written through the guest's page cache and not flushed: only the
    // snapshot can carry it.
    vm.must_ask("write-disk not yet flushed");
    let slept = vm.sleep(None);
    eprintln!(
        "balloon, 512 MiB touched and freed: {} MiB snapshot, {:?}",
        mib(slept.snapshot_bytes),
        slept.timings
    );
    assert!(
        slept.snapshot_bytes <= BALLOONED_LIMIT,
        "the balloon should keep freed memory out: {} MiB",
        mib(slept.snapshot_bytes)
    );
    assert!(
        slept.snapshot_bytes <= unballooned.snapshot_bytes + 32 * MIB,
        "the balloon made the snapshot bigger: {} MiB against {} MiB",
        mib(slept.snapshot_bytes),
        mib(unballooned.snapshot_bytes)
    );
    let mut save_ms = ms(slept.timings.save_ms);
    for _ in 1..TIMING_TRIES {
        if save_ms <= SAVE_LIMIT_MS {
            break;
        }
        eprintln!(
            "the save took {save_ms} ms, the load at {:.1}; again",
            load()
        );
        vm.restore();
        vm.must_ask("dirty 512");
        save_ms = save_ms.min(ms(vm.sleep(None).timings.save_ms));
    }
    assert!(
        save_ms <= SAVE_LIMIT_MS,
        "the save took {save_ms} ms at best of {TIMING_TRIES}, the load at {:.1}",
        load()
    );

    // A file changed on the host while the VM slept.
    fs::write(vm.share.join("probe"), "after").unwrap();
    let mut best: Option<(u64, Duration)> = None;
    for attempt in 1..=TIMING_TRIES {
        let (ready, answer) = vm.restore();
        let restore_ms = ms(ready.timings.restore_ms) + ms(ready.timings.resume_ms);
        eprintln!(
            "idle restore: {restore_ms} ms in vibe-vm, {} ms to first answer, the load at {:.1}",
            answer.as_millis(),
            load()
        );
        let (r, a) = best.unwrap_or((restore_ms, answer));
        best = Some((r.min(restore_ms), a.min(answer)));
        let fast = restore_ms <= RESTORE_LIMIT_MS && answer <= RESTORE_LIMIT;
        if fast || attempt == TIMING_TRIES {
            break;
        }
        vm.sleep(None);
    }
    let (restore_ms, answer) = best.unwrap();
    assert!(
        restore_ms <= RESTORE_LIMIT_MS,
        "the restore took {restore_ms} ms at best of {TIMING_TRIES}"
    );
    assert!(
        answer <= RESTORE_LIMIT,
        "the first answer took {answer:?} at best of {TIMING_TRIES}"
    );
    assert_eq!(vm.tick(), 6, "the guest carries on exactly");
    // The share is missing for about a second after a restore, then fresh.
    let status = vm.wait_for("share", Some("after"), Duration::from_secs(5));
    assert_eq!(status["boot_id"], boot_id, "the same boot");
    assert_eq!(status["ip"], first["ip"], "the same address");
    assert_eq!(
        status["mac"].as_str().map(str::to_lowercase),
        vm.config.mac.clone()
    );
    // The unflushed write survived in the guest, and reaches the agent's
    // disk image once flushed.
    assert_eq!(status["disk"], "not yet flushed", "{status}");
    vm.must_ask("sync");
    assert_eq!(vm.disk_on_host(), "not yet flushed");

    // Nothing to restore now: the snapshot went when the VM ran again.
    vm.stop();
    let err = vm::restore(&vm.launch()).unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::NoSnapshot), "{err}");
    // A snapshot that won't restore is reported and kept, for vibed.
    fs::write(&vm.config.snapshot, "not a snapshot").unwrap();
    let err = vm::restore(&vm.launch()).unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::RestoreFailed), "{err}");
    assert!(vm.config.snapshot.exists());
    assert!(!vm::lock_held(&vm.socket));
    mark("boot_save_restore");
}

#[test]
fn three_vms_at_once() {
    let Some(g) = guest("three_vms_at_once") else {
        return;
    };
    let root = short_tempdir();
    let vms: Vec<TestVm> = (1..=3)
        .map(|i| {
            let name = format!("par{i}");
            let socket = root.path().join(format!("{name}.vm"));
            TestVm::create(&g, root.path(), &name, 512 * MIB, socket)
        })
        .collect();

    // Runs `f` on every VM at once; how long they took together.
    fn all<T: Send>(vms: &[TestVm], f: impl Fn(&TestVm) -> T + Sync) -> (Vec<T>, Duration) {
        let t = Instant::now();
        let results = thread::scope(|s| {
            let handles: Vec<_> = vms.iter().map(|vm| s.spawn(|| f(vm))).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        (results, t.elapsed())
    }

    let (_, boot) = all(&vms, TestVm::boot);
    eprintln!("three boots: {} ms", boot.as_millis());
    for vm in &vms {
        assert_eq!(vm.tick(), 1);
    }
    // Each round saves all three at once, then restores all three at once;
    // the slowest of a round is what counts, and the best round is held to
    // the limits.
    let mut best_save = u64::MAX;
    let mut best_restore = (u64::MAX, Duration::MAX);
    for round in 1..=TIMING_TRIES {
        let (slept, sleep) = all(&vms, |vm| vm.sleep(None));
        eprintln!(
            "three sleeps: {} ms, the load at {:.1}",
            sleep.as_millis(),
            load()
        );
        for s in &slept {
            eprintln!("  {} MiB, {:?}", mib(s.snapshot_bytes), s.timings);
        }
        let save_ms = slept.iter().map(|s| ms(s.timings.save_ms)).max().unwrap();
        best_save = best_save.min(save_ms);
        let (restored, restore) = all(&vms, TestVm::restore);
        eprintln!(
            "three restores: {} ms, the load at {:.1}",
            restore.as_millis(),
            load()
        );
        let mut restore_ms = 0;
        for (ready, answer) in &restored {
            let r = ms(ready.timings.restore_ms) + ms(ready.timings.resume_ms);
            eprintln!(
                "  {r} ms in vibe-vm, {} ms to first answer",
                answer.as_millis()
            );
            restore_ms = restore_ms.max(r);
        }
        best_restore = (best_restore.0.min(restore_ms), best_restore.1.min(restore));
        let fast =
            save_ms <= SAVE_LIMIT_MS && restore_ms <= RESTORE_LIMIT_MS && restore <= RESTORE_LIMIT;
        if fast {
            break;
        }
        if round < TIMING_TRIES {
            eprintln!("a round was over the limits; again");
        }
    }
    assert!(
        best_save <= SAVE_LIMIT_MS,
        "a save took {best_save} ms in the best of {TIMING_TRIES} rounds"
    );
    assert!(
        best_restore.0 <= RESTORE_LIMIT_MS,
        "a restore took {} ms in the best of {TIMING_TRIES} rounds",
        best_restore.0
    );
    // The spike restored three at once in 0.5 s.
    assert!(
        best_restore.1 <= RESTORE_LIMIT,
        "three restores took {:?} in the best of {TIMING_TRIES} rounds",
        best_restore.1
    );
    for vm in &vms {
        assert_eq!(vm.tick(), 2, "{} carries on exactly", vm.name);
    }
    all(&vms, TestVm::stop);
    mark("three_vms_at_once");
}

/// Half of a test across a reboot: `VIBE_VM_PHASE=save` boots a VM under
/// `VIBE_VM_STATE`, ticks it to 7 and puts it to sleep; after a reboot,
/// `VIBE_VM_PHASE=restore` restores it and expects 8.
#[test]
fn reboot() {
    let Some(phase) = std::env::var_os("VIBE_VM_PHASE") else {
        eprintln!("reboot: skipped; scripts/vm-reboot-test.sh runs it");
        return;
    };
    let Some(g) = guest("reboot") else {
        return;
    };
    let root = absolute(PathBuf::from(
        std::env::var_os("VIBE_VM_STATE").expect("VIBE_VM_STATE"),
    ));
    let socket = PathBuf::from("/tmp/vibe-vm-reboot-test.vm");
    match phase.to_str() {
        Some("save") => {
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            let vm = TestVm::create(&g, &root, "reboot", 1024 * MIB, socket);
            vm.boot();
            for want in 1..=7 {
                assert_eq!(vm.tick(), want);
            }
            vm.must_ask("write-disk saved before the reboot");
            let boot_id = vm.must_ask("status")["boot_id"].clone();
            fs::write(root.join("boot_id"), boot_id.to_string()).unwrap();
            let slept = vm.sleep(None);
            eprintln!(
                "saved {} MiB to {}; reboot the Mac, then run the restore half",
                mib(slept.snapshot_bytes),
                slept.snapshot.display()
            );
        }
        Some("restore") => {
            let vm = TestVm::load(&g, &root, "reboot", socket);
            let (ready, answer) = vm.restore();
            eprintln!(
                "restored after the reboot: {:?}, {} ms to first answer",
                ready.timings,
                answer.as_millis()
            );
            assert_eq!(vm.tick(), 8, "the guest carries on exactly");
            let status = vm.must_ask("status");
            let boot_id = fs::read_to_string(root.join("boot_id")).unwrap();
            assert_eq!(status["boot_id"].to_string(), boot_id, "the same boot");
            assert_eq!(status["disk"], "saved before the reboot");
            vm.must_ask("sync");
            assert_eq!(vm.disk_on_host(), "saved before the reboot");
            vm.stop();
            mark("reboot");
        }
        _ => panic!("VIBE_VM_PHASE is save or restore"),
    }
}

// ---------------------------------------------------------------------------
// vibe-guest

/// A token as `claude setup-token` makes them, which must reach the command
/// and nowhere else.
const TOKEN: &str = "sk-ant-oat01-vm-cycle-test-token";

fn shim_binary() -> PathBuf {
    let path = std::env::var_os("VIBE_SHIM_BIN")
        .map(|p| absolute(PathBuf::from(p)))
        .unwrap_or_else(|| absolute(PathBuf::from("target/debug/vibe-shim")));
    assert!(
        path.exists(),
        "{} is missing: cargo build -p vibe-shim, or set VIBE_SHIM_BIN",
        path.display()
    );
    path
}

impl TestVm {
    /// Boots a `vibe-guest` VM; how long until the guest answered.
    fn boot_guest(&self) -> (Duration, vibe_guest::Status) {
        let t = Instant::now();
        let ready = vm::boot(&self.launch())
            .unwrap_or_else(|e| panic!("boot {}: {e}\n{}", self.name, self.logs()));
        self.pid.store(ready.pid, Ordering::SeqCst);
        let status =
            vibe_guest::wait_ready(&self.socket, Duration::from_secs(30)).unwrap_or_else(|e| {
                panic!(
                    "{}: vibe-guest didn't answer: {e}\n{}",
                    self.name,
                    self.logs()
                )
            });
        (t.elapsed(), status)
    }

    fn restore_guest(&self) -> (Ready, Duration) {
        let t = Instant::now();
        let ready = vm::restore(&self.launch())
            .unwrap_or_else(|e| panic!("restore {}: {e}\n{}", self.name, self.logs()));
        self.pid.store(ready.pid, Ordering::SeqCst);
        vibe_guest::wait_ready(&self.socket, Duration::from_secs(30)).unwrap_or_else(|e| {
            panic!(
                "{}: no answer after the restore: {e}\n{}",
                self.name,
                self.logs()
            )
        });
        assert!(!self.config.snapshot.exists());
        (ready, t.elapsed())
    }

    /// Waits for vibe-guest's DHCP lease, with a way out.
    fn wait_network(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        let network = loop {
            if let Some(n) = self.guest_status().network {
                break n;
            }
            assert!(Instant::now() < deadline, "no DHCP lease\n{}", self.logs());
            thread::sleep(Duration::from_millis(100));
        };
        eprintln!("the guest's network: {network:?}");
        assert!(network.gateway.is_some(), "{network:?}");
    }

    fn guest_status(&self) -> vibe_guest::Status {
        vibe_guest::status(&self.socket)
            .unwrap_or_else(|e| panic!("{}: status: {e}\n{}", self.name, self.logs()))
    }

    /// Starts a `vibe-shim --vm` on this VM's guest, as `vibed` does after a
    /// boot or a restore.
    fn shim(&self, root: &Path, name: &str) -> GuestShim {
        let run = root.join("run");
        let socket = run.join(format!("{name}.sock"));
        let state = root.join("agents").join(&self.name);
        let out = std::process::Command::new(shim_binary())
            .args(["--ref", &self.name, "--socket"])
            .arg(&socket)
            .arg("--state")
            .arg(&state)
            .arg("--vm")
            .arg(&self.socket)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "vibe-shim --vm failed: {}\n{}",
            String::from_utf8_lossy(&out.stderr),
            self.logs()
        );
        GuestShim {
            pid: String::from_utf8(out.stdout)
                .unwrap()
                .trim()
                .parse()
                .unwrap(),
            socket,
            state,
        }
    }
}

struct GuestShim {
    pid: i32,
    socket: PathBuf,
    state: PathBuf,
}

impl Drop for GuestShim {
    fn drop(&mut self) {
        if self.alive() {
            // SAFETY: a signal to the shim this test started.
            unsafe { libc::kill(self.pid, libc::SIGKILL) };
        }
    }
}

impl GuestShim {
    fn alive(&self) -> bool {
        // SAFETY: signal 0 only checks the process exists.
        unsafe { libc::kill(self.pid, 0) == 0 }
    }

    fn wait_gone(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.alive() {
            assert!(Instant::now() < deadline, "the shim didn't exit");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn record(&self) -> Option<vibe_core::shim::Record> {
        let text = fs::read_to_string(self.state.join(vibe_core::shim::RECORD_FILE)).ok()?;
        Some(serde_json::from_str(&text).unwrap())
    }

    /// Attaches as `vibe attach` would, at 100x30.
    fn attach(&self) -> Screen {
        use vibe_core::shim::{Hello, PROTOCOL, Role, Size, connect};
        let size = Size {
            rows: 30,
            cols: 100,
        };
        let hello = Hello {
            protocol: PROTOCOL,
            role: Role::Attach,
            client: "vm_cycle".into(),
            tty: None,
            size: Some(size),
        };
        let conn = connect(&self.socket, &hello, Duration::from_secs(20)).unwrap();
        let one = std::num::NonZeroU16::new;
        Screen {
            stream: conn.stream,
            parser: vt100::Parser::new(one(30).unwrap(), one(100).unwrap(), 0),
            exit: None,
        }
    }
}

/// What an attached terminal shows.
struct Screen {
    stream: std::os::unix::net::UnixStream,
    parser: vt100::Parser,
    exit: Option<vibe_core::rpc::SessionExit>,
}

impl Screen {
    fn key(&mut self, key: &str) {
        use vibe_core::shim::{Frame, FrameKind, write_frame};
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::Input, key.as_bytes()),
        )
        .unwrap();
    }

    fn text(&self) -> String {
        self.parser.screen().contents()
    }

    /// Reads a frame; false once the shim has closed.
    fn pump(&mut self, timeout: Duration) -> bool {
        use vibe_core::shim::{FrameKind, read_frame};
        self.stream.set_read_timeout(Some(timeout)).unwrap();
        match read_frame(&mut self.stream) {
            Ok(Some(f)) => {
                match f.kind {
                    FrameKind::Output => self.parser.process(&f.payload),
                    FrameKind::Exit => self.exit = Some(f.decode().unwrap()),
                    other => panic!("a {other:?} frame"),
                }
                true
            }
            Ok(None) => false,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => false,
            Err(e) => panic!("{e}"),
        }
    }

    fn expect(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.text().contains(text) {
            assert!(
                Instant::now() < deadline,
                "{text:?} never showed; the screen:\n{}",
                self.text()
            );
            if !self.pump(Duration::from_millis(100)) {
                assert!(
                    self.text().contains(text),
                    "closed before {text:?}:\n{}",
                    self.text()
                );
            }
        }
    }

    fn expect_close(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.pump(Duration::from_millis(100)) {
            assert!(Instant::now() < deadline, "the shim didn't close");
        }
    }
}

const QUESTION: &str = "Which colour should the button be?";

/// The Done-when of #8, with `vibe-testguest ask` standing in for Claude:
/// its TUI is reachable through the shim, and a pending question is still
/// on screen after freeze, save, restore and thaw.
#[test]
fn guest_question_survives_sleep() {
    let Some(g) = guest("guest_question_survives_sleep") else {
        return;
    };
    let root = short_tempdir();
    let vm = TestVm::create_as(
        &g,
        root.path(),
        "agent",
        1024 * MIB,
        root.path().join("agent.vm"),
        &Kind::VibeGuest { claude: None },
    );
    let (boot, status) = vm.boot_guest();
    eprintln!("boot to vibe-guest's first answer: {} ms", boot.as_millis());
    assert!(boot <= BOOT_LIMIT, "boot took {boot:?}\n{}", vm.logs());
    assert_eq!(status.state, vibe_guest::GuestState::Idle);
    assert!(
        status.problems.is_empty(),
        "{:?}\n{}",
        status.problems,
        vm.logs()
    );
    let boot_id = status.boot_id.clone();
    let share = vm.share.to_string_lossy().into_owned();
    assert_eq!(
        status.shares,
        [vibe_guest::proto::ShareMount {
            tag: "share".into(),
            path: share.clone()
        }],
        "the share is at its host path"
    );
    // DHCP, by vibe-guest itself.
    vm.wait_network();

    // Claude, as it were, with the token in its environment only.
    let launch = vibe_guest::Launch {
        command: vec!["/vibe-testguest".into(), "ask".into()],
        env: [("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), TOKEN.to_owned())].into(),
        cwd: Some(share.clone()),
        size: Some(vibe_guest::TermSize {
            rows: 30,
            cols: 100,
        }),
        time_ms: Some(vibe_guest::millis(std::time::SystemTime::now())),
    };
    let pid = vibe_guest::launch(&vm.socket, &launch).unwrap().pid;
    let shim = vm.shim(root.path(), "first");
    let mut screen = shim.attach();
    screen.expect(QUESTION);
    // SAFETY: getuid and getgid have no memory effects.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    screen.expect(&format!("as vibe uid {uid} gid {gid}, pid {pid}"));
    screen.expect(&format!("token set ({} bytes)", TOKEN.len()));
    screen.expect(&format!("XDG_RUNTIME_DIR /run/user/{uid} private"));
    screen.expect("PATH from /opt/vibeos/bin");
    screen.expect(&format!("cwd {share}"));
    // `vibe` may write to the share, which virtio-fs shows with its host
    // owner: that's why `vibe` has the host user's ids.
    screen.key("w");
    screen.expect("wrote: ");
    screen.expect("wrote: ok");
    assert_eq!(
        fs::read_to_string(vm.share.join("written")).unwrap(),
        "by vibe\n"
    );
    let procs = vibe_guest::processes(&vm.socket).unwrap();
    assert!(
        procs
            .processes
            .iter()
            .any(|p| p.pid == pid && p.args == ["/vibe-testguest", "ask"]),
        "{procs:?}"
    );

    // The shim dies: the agent goes on, and the next shim shows the question.
    // SAFETY: a signal to the shim this test started.
    unsafe { libc::kill(shim.pid, libc::SIGKILL) };
    shim.wait_gone();
    let shim = vm.shim(root.path(), "second");
    let mut screen = shim.attach();
    screen.expect(QUESTION);
    drop(screen);

    // Sleep: freeze, then save with the balloon floored at what the guest
    // uses. The shim's stream goes with vibe-vm, and the shim with it,
    // leaving no record: the session isn't over.
    let frozen = vibe_guest::freeze(&vm.socket).unwrap();
    eprintln!("freeze: {frozen:?}");
    assert!(frozen.processes >= 1);
    let floor = vibe_guest::balloon_floor(frozen.memory);
    let slept = vm.sleep_down_to(None, Some(floor));
    eprintln!(
        "asleep: {} MiB snapshot, balloon down to {} MiB, {:?}",
        mib(slept.snapshot_bytes),
        mib(floor),
        slept.timings
    );
    shim.wait_gone();
    assert!(shim.record().is_none(), "{:?}", shim.record());
    fs::write(vm.share.join("probe"), "after").unwrap();
    // Long enough for the guest's stopped clock to be plainly behind.
    thread::sleep(Duration::from_secs(3));

    // Wake: restore, thaw with the host's clock, and a new shim.
    let (ready, answer) = vm.restore_guest();
    eprintln!(
        "restore to vibe-guest's first answer: {} ms, {:?}",
        answer.as_millis(),
        ready.timings
    );
    let thawed = vibe_guest::thaw(&vm.socket, std::time::SystemTime::now(), None).unwrap();
    eprintln!("thaw: {thawed:?}");
    assert!(thawed.problems.is_empty(), "{thawed:?}");
    assert!(
        thawed.clock_step_ms.unwrap() >= 2500,
        "the clock was behind by the sleep: {thawed:?}"
    );
    let status = vm.guest_status();
    assert_eq!(
        (&status.boot_id, status.state, status.frozen),
        (&boot_id, vibe_guest::GuestState::Running, false),
        "the same boot, carrying on"
    );
    let shim = vm.shim(root.path(), "third");
    let mut screen = shim.attach();
    // The question is still there, with nothing replayed.
    screen.expect(QUESTION);
    screen.expect("\u{276f} 1. Red");
    // The shares answered before the agent ran again, and its clock is the
    // host's.
    screen.key("r");
    screen.expect("probe: after clock: ");
    let shown: u64 = screen
        .text()
        .rsplit("clock: ")
        .next()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap();
    let now = vibe_guest::millis(std::time::SystemTime::now()) / 1000;
    assert!(
        shown.abs_diff(now) <= 5,
        "the guest's clock is {shown}, the host's {now}"
    );
    screen.key("2");
    screen.expect("answered: Blue");
    assert!(
        !fs::read_to_string(vm.config.console_log.as_ref().unwrap())
            .unwrap()
            .contains(TOKEN)
    );

    // The agent exits: the shim records it, and a launch starts the next,
    // as `claude --resume` will.
    screen.key("x");
    screen.expect_close();
    assert_eq!(screen.exit.as_ref().and_then(|e| e.code), Some(7));
    shim.wait_gone();
    assert_eq!(shim.record().and_then(|r| r.exit.code), Some(7));
    assert_eq!(vm.guest_status().state, vibe_guest::GuestState::Exited);
    let again = vibe_guest::launch(&vm.socket, &launch).unwrap().pid;
    assert_ne!(again, pid);
    assert_eq!(vm.guest_status().state, vibe_guest::GuestState::Running);

    // A poweroff ends it all, and vibe-vm with it.
    vibe_guest::poweroff(&vm.socket).unwrap();
    assert!(
        vm::wait_gone(&vm.socket, Duration::from_secs(20)),
        "{}",
        vm.logs()
    );
    mark("guest_question_survives_sleep");
}

/// What a new status engine tail on the real Claude's `transcript` says,
/// once it says Claude is asking: up to 10 s, as Claude may still be
/// writing the question's entry.
fn claude_status(transcript: &Path, worktree: &str, session: &str) -> vibe_status::Status {
    use vibe_core::model::{Activity, AgentState, Certainty};
    let mut tail = vibe_status::Tail::new(
        transcript.parent().unwrap().to_owned(),
        Path::new(worktree),
        session,
        Duration::from_secs(60),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        tail.poll();
        let s = tail.status(vibe_status::Timestamp::from(std::time::SystemTime::now()));
        if s.activity == Activity::AwaitingAnswer || Instant::now() > deadline {
            assert_eq!(
                (s.state(), s.activity, s.certainty),
                (
                    AgentState::Question,
                    Activity::AwaitingAnswer,
                    Certainty::Firm
                ),
                "{s:?}"
            );
            assert_eq!(s.transcript.as_deref(), Some(transcript));
            assert!(s.at_rest(), "safe to sleep on");
            return s;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// Whether the screen shows a question with Red and Blue as its options.
fn shows_question(screen: &str) -> bool {
    let s = screen.to_lowercase();
    (s.contains("1. red") && s.contains("2. blue"))
        || (s.contains("1. blue") && s.contains("2. red"))
}

/// The Done-when of #8 with the real Claude: its TUI through the shim, and
/// an `AskUserQuestion` still on screen after freeze, save, restore and
/// thaw. It talks to the real API, so it runs only when asked to:
/// `VIBE_CLAUDE_BIN` is Claude Code's `linux-arm64-musl` build, and
/// `CLAUDE_CODE_OAUTH_TOKEN` a token from `claude setup-token`.
#[test]
fn guest_claude_question_survives_sleep() {
    let (Some(bin), Ok(token)) = (
        std::env::var_os("VIBE_CLAUDE_BIN"),
        std::env::var("CLAUDE_CODE_OAUTH_TOKEN"),
    ) else {
        eprintln!(
            "guest_claude_question_survives_sleep: skipped; set VIBE_CLAUDE_BIN and CLAUDE_CODE_OAUTH_TOKEN to run it"
        );
        return;
    };
    let Some(g) = guest("guest_claude_question_survives_sleep") else {
        return;
    };
    let root = short_tempdir();
    // A directory of its own, shared read-only at /opt/claude.
    let bin_dir = root.path().join("claude-bin");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::copy(absolute(PathBuf::from(bin)), bin_dir.join("claude")).unwrap();
    let vm = TestVm::create_as(
        &g,
        root.path(),
        "claude",
        2048 * MIB,
        root.path().join("claude.vm"),
        &Kind::VibeGuest {
            claude: Some(bin_dir),
        },
    );
    let (_, status) = vm.boot_guest();
    assert!(
        status.problems.is_empty(),
        "{:?}\n{}",
        status.problems,
        vm.logs()
    );
    vm.wait_network();
    let share = vm.share.to_string_lossy().into_owned();
    let session = format!(
        "{:08x}-0000-4000-8000-{:012x}",
        std::process::id(),
        vibe_guest::millis(std::time::SystemTime::now())
    );
    // Onboarding done and the worktree trusted, as vibed will pre-fill
    // them, or Claude stops at its theme picker (PRD: Spike results).
    let prefill = serde_json::json!({
        "hasCompletedOnboarding": true,
        "theme": "dark",
        "projects": { share.clone(): {
            "hasTrustDialogAccepted": true,
            "hasCompletedProjectOnboarding": true,
        }},
    });
    let prompt = "Use the AskUserQuestion tool to ask me one question: should the button be \
                  red or blue? Offer exactly two options, Red and Blue. Do nothing else.";
    let launch = vibe_guest::Launch {
        command: vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf '%s' \"$PREFILL\" >\"$HOME/.claude.json\" && \
             exec /opt/claude/claude --session-id \"$SESSION\" \"$PROMPT\""
                .into(),
        ],
        env: [
            ("CLAUDE_CODE_OAUTH_TOKEN", token.as_str()),
            ("DISABLE_AUTOUPDATER", "1"),
            ("PREFILL", &prefill.to_string()),
            ("SESSION", &session),
            ("PROMPT", prompt),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect(),
        cwd: Some(share.clone()),
        size: Some(vibe_guest::TermSize {
            rows: 30,
            cols: 100,
        }),
        time_ms: Some(vibe_guest::millis(std::time::SystemTime::now())),
    };
    vibe_guest::launch(&vm.socket, &launch).unwrap();
    let shim = vm.shim(root.path(), "claude-1");
    let mut screen = shim.attach();
    let deadline = Instant::now() + Duration::from_secs(180);
    while !shows_question(&screen.text()) {
        assert!(
            Instant::now() < deadline,
            "Claude never asked; the screen:\n{}\n{}",
            screen.text(),
            vm.logs()
        );
        assert!(
            screen.pump(Duration::from_millis(200)),
            "the shim closed; the screen:\n{}",
            screen.text()
        );
    }
    // Its transcript is on the host, where agent-tail will read it.
    let transcripts = vm.share.parent().unwrap().join("claude-home/projects");
    let transcript = fs::read_dir(&transcripts)
        .unwrap()
        .flatten()
        .map(|d| d.path().join(format!("{session}.jsonl")))
        .find(|p| p.exists())
        .unwrap_or_else(|| {
            panic!(
                "no transcript for {session} under {}",
                transcripts.display()
            )
        });
    assert!(
        fs::read_to_string(&transcript)
            .unwrap()
            .contains("AskUserQuestion")
    );
    // The status engine reads it there as vibed does: a firm question.
    let asking = claude_status(&transcript, &share, &session);
    drop(screen);

    let frozen = vibe_guest::freeze(&vm.socket).unwrap();
    let slept = vm.sleep_down_to(None, Some(vibe_guest::balloon_floor(frozen.memory)));
    eprintln!(
        "Claude asleep: {} MiB snapshot, {:?}",
        mib(slept.snapshot_bytes),
        slept.timings
    );
    shim.wait_gone();
    thread::sleep(Duration::from_secs(3));
    vm.restore_guest();
    let thawed = vibe_guest::thaw(&vm.socket, std::time::SystemTime::now(), None).unwrap();
    assert!(thawed.problems.is_empty(), "{thawed:?}");
    let shim = vm.shim(root.path(), "claude-2");
    let mut screen = shim.attach();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !shows_question(&screen.text()) {
        assert!(
            Instant::now() < deadline,
            "the question is gone after the restore; the screen:\n{}",
            screen.text()
        );
        screen.pump(Duration::from_millis(200));
    }
    // Read afresh, as a restarted vibed would, still the same question. (Its
    // title may have come in since: Claude writes one when it's ready.)
    let after = claude_status(&transcript, &share, &session);
    assert_eq!((after.line, after.since), (asking.line, asking.since));
    let procs = vibe_guest::processes(&vm.socket).unwrap();
    assert!(
        procs
            .processes
            .iter()
            .any(|p| p.args.first().is_some_and(|a| a.ends_with("claude"))),
        "{procs:?}"
    );
    let console = fs::read_to_string(vm.config.console_log.as_ref().unwrap()).unwrap();
    assert!(
        !console.contains(&token),
        "the token reached the console log"
    );
    vibe_guest::poweroff(&vm.socket).unwrap();
    assert!(vm::wait_gone(&vm.socket, Duration::from_secs(20)));
    mark("guest_claude_question_survives_sleep");
}
