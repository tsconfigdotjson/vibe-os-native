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

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use vibe_env::vm::{
    self, Disk, ErrorCode, Launch, Probe, Share, Slept, Started, VmConfig, VmState,
};

/// The vsock port `vibe-testguest` serves.
const PORT: u32 = 1024;

const MIB: u64 = 1 << 20;

/// Starting `vibe-vm` to the guest's first answer over vsock, on a fresh
/// boot. The spike's tailored guest took 0.35 to 1 s; the test guest loads
/// its drivers from a squashfs first.
const BOOT_LIMIT: Duration = Duration::from_secs(10);

/// Saving an idle VM, after the balloon: about 1 s in the spike.
const SAVE_LIMIT_MS: u64 = 3000;

/// Starting `vibe-vm restore` to the guest's first answer, for an idle VM:
/// about 0.5 s in the spike.
const RESTORE_LIMIT: Duration = Duration::from_secs(3);

/// An idle VM's snapshot after the balloon, though the guest had touched
/// 512 MiB and freed it. The spike's came to 41 MB.
const BALLOONED_LIMIT: u64 = 256 * MIB;

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

fn absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir().unwrap().join(path)
    }
}

/// Leaves a mark that `test` really ran.
fn mark(test: &str) {
    if let Some(dir) = std::env::var_os("VIBE_VM_MARKER") {
        fs::create_dir_all(&dir).unwrap();
        fs::write(Path::new(&dir).join(test), "ran\n").unwrap();
    }
}

/// A short root under /tmp: socket paths must fit in 104 bytes.
fn short_tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("vmc")
        .tempdir_in("/tmp")
        .unwrap()
}

struct TestVm {
    name: String,
    config_path: PathBuf,
    socket: PathBuf,
    log: PathBuf,
    share: PathBuf,
    config: VmConfig,
    vibe_vm: PathBuf,
}

impl TestVm {
    /// Writes the VM's `vm.json` under `root` and runs `vibe-vm create` on it.
    fn create(g: &Guest, root: &Path, name: &str, memory: u64, socket: PathBuf) -> TestVm {
        let dir = root.join(name);
        let share = dir.join("share");
        fs::create_dir_all(&share).unwrap();
        fs::write(share.join("probe"), "before").unwrap();
        let disk = dir.join("disk.img");
        vm::create_disk(&disk, 64 * MIB).unwrap();
        let host_cpus = thread::available_parallelism().map_or(1, |n| n.get());
        let config = VmConfig {
            version: vm::CONFIG_VERSION,
            agent: name.into(),
            cpus: vm::vcpus(2, host_cpus),
            memory,
            kernel: g.dir.join("Image"),
            initrd: Some(g.dir.join("initrd")),
            cmdline: "console=hvc0".into(),
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
            shares: vec![Share {
                tag: "share".into(),
                path: share.clone(),
                read_only: false,
            }],
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
        let mut stream = vm::connect(&self.socket, PORT).map_err(|e| e.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
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
            match self.ask("status") {
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
        assert_eq!(ready.started, Started::Boot);
        assert_eq!(ready.protocol, vm::PROTOCOL);
        assert!(ready.timings.start_ms.is_some() && ready.timings.total_ms.is_some());
        self.first_answer(t, BOOT_LIMIT)
    }

    fn sleep(&self, balloon_ms: Option<u64>) -> Slept {
        let slept = vm::sleep(&self.socket, balloon_ms)
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

    fn restore(&self) -> Duration {
        let t = Instant::now();
        let ready = vm::restore(&self.launch())
            .unwrap_or_else(|e| panic!("restore {}: {e}\n{}", self.name, self.logs()));
        assert_eq!(ready.started, Started::Restore);
        assert!(ready.timings.restore_ms.is_some() && ready.timings.resume_ms.is_some());
        let answer = self.first_answer(t, RESTORE_LIMIT);
        // A running VM's disks move on, so its snapshot is gone.
        assert!(!self.config.snapshot.exists());
        answer
    }

    fn stop(&self) {
        vm::stop(&self.socket).unwrap();
        assert!(vm::wait_gone(&self.socket, Duration::from_secs(10)));
    }

    /// Waits up to 5 s for the share's probe file to read `want` in the
    /// guest: right after a restore the share is missing for about a second.
    fn wait_for_share(&self, want: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = self.must_ask("status");
            if status["share"] == want {
                return status;
            }
            if Instant::now() >= deadline {
                panic!(
                    "{}: the share reads {status}, not {want}\n{}",
                    self.name,
                    self.logs()
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

fn mib(bytes: u64) -> u64 {
    bytes / MIB
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

    // Boot.
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
    let first = vm.must_ask("status");
    let boot_id = first["boot_id"].clone();
    assert!(boot_id.is_string(), "{first}");
    assert_eq!(
        first["mac"].as_str().map(str::to_lowercase),
        vm.config.mac.clone(),
        "the guest's NIC has the fixed MAC"
    );
    assert_eq!(first["share"], "before", "{first}");

    // One vibe-vm per VM.
    let err = vm::boot(&vm.launch()).unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::AlreadyRunning), "{err}");
    // A connect to a port nobody serves is refused, not hung.
    let err = vm::connect(&vm.socket, 9).unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::ConnectFailed), "{err}");

    assert_eq!((vm.tick(), vm.tick(), vm.tick()), (1, 2, 3));

    // Without the balloon, memory the guest touched and freed is saved too.
    vm.must_ask("dirty 512");
    let unballooned = vm.sleep(Some(0));
    eprintln!(
        "no balloon, 512 MiB touched and freed: {} MiB snapshot, {:?}",
        mib(unballooned.snapshot_bytes),
        unballooned.timings
    );
    assert!(
        unballooned.snapshot_bytes >= 384 * MIB,
        "the guest's freed memory should be in the snapshot: {} MiB",
        mib(unballooned.snapshot_bytes)
    );
    let restored = vm.restore();
    eprintln!("restore to first answer: {} ms", restored.as_millis());
    assert_eq!(vm.tick(), 4, "the guest carries on exactly");

    // With it, the snapshot holds only what the guest really uses.
    vm.must_ask("dirty 512");
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
    let save_ms = slept.timings.save_ms.unwrap();
    assert!(save_ms <= SAVE_LIMIT_MS, "the save took {save_ms} ms");

    // A file changed on the host while the VM slept.
    fs::write(vm.share.join("probe"), "after").unwrap();
    let restored = vm.restore();
    eprintln!("idle restore to first answer: {} ms", restored.as_millis());
    assert!(restored <= RESTORE_LIMIT, "the restore took {restored:?}");
    assert_eq!(vm.tick(), 5, "the guest carries on exactly");
    let status = vm.wait_for_share("after");
    assert_eq!(status["boot_id"], boot_id, "the same boot");
    assert_eq!(status["disk"], "not yet flushed", "{status}");
    assert_eq!(
        status["mac"].as_str().map(str::to_lowercase),
        vm.config.mac.clone()
    );

    // Nothing to restore now: the snapshot went when the VM ran again.
    vm.stop();
    let err = vm::restore(&vm.launch()).unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::NoSnapshot), "{err}");
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

    let all = |f: &(dyn Fn(&TestVm) + Sync)| {
        let t = Instant::now();
        thread::scope(|s| {
            for vm in &vms {
                s.spawn(move || f(vm));
            }
        });
        t.elapsed()
    };

    let boot = all(&|vm| {
        vm.boot();
    });
    eprintln!("three boots: {} ms", boot.as_millis());
    for vm in &vms {
        assert_eq!(vm.tick(), 1);
    }
    let save = all(&|vm| {
        vm.sleep(None);
    });
    eprintln!("three sleeps: {} ms", save.as_millis());
    let restore = all(&|vm| {
        vm.restore();
    });
    eprintln!("three restores: {} ms", restore.as_millis());
    for vm in &vms {
        assert_eq!(vm.tick(), 2, "{} carries on exactly", vm.name);
    }
    all(&|vm| vm.stop());
    // The spike saved three in 0.9 s and restored them in 0.5 s, balloon
    // aside; each sleep here holds the balloon for 2 s.
    assert!(
        restore <= RESTORE_LIMIT * 2,
        "three restores took {restore:?}"
    );
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
            let restored = vm.restore();
            eprintln!(
                "restored after the reboot: {} ms to first answer",
                restored.as_millis()
            );
            assert_eq!(vm.tick(), 8, "the guest carries on exactly");
            let status = vm.must_ask("status");
            let boot_id = fs::read_to_string(root.join("boot_id")).unwrap();
            assert_eq!(status["boot_id"].to_string(), boot_id, "the same boot");
            assert_eq!(status["disk"], "saved before the reboot");
            vm.stop();
            mark("reboot");
        }
        _ => panic!("VIBE_VM_PHASE is save or restore"),
    }
}
