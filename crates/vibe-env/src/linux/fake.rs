//! A fake `vibe-vm` and `vibe-guest` behind [`Machine`], for the `linux`
//! backend's and the toolbox's tests. The real thing runs on a Mac:
//! `crates/vibed/tests/linux_vm.rs`.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde_json::json;

use super::{Machine, lock};
use crate::guest::{self, boot, proto};
use crate::vm::{self, VmConfig, VmError};
use crate::{EnvError, Result};

/// One fake VM: what `vibe-vm` and `vibe-guest` would say about it.
#[derive(Debug, Default, Clone)]
pub(crate) struct Vm {
    pub(crate) running: bool,
    pub(crate) frozen: bool,
    /// None: nothing launched; Some(None): running; Some(Some(code)): exited.
    pub(crate) session: Option<Option<i32>>,
    pub(crate) config: PathBuf,
    /// Whether the host has set the guest's clock since it booted.
    pub(crate) clock_set: bool,
}

#[derive(Default)]
pub(crate) struct State {
    pub(crate) vms: HashMap<PathBuf, Vm>,
    pub(crate) calls: Vec<String>,
    pub(crate) launches: Vec<guest::Launch>,
    /// The next restore fails with `restore_failed`.
    pub(crate) restore_fails: bool,
    /// What a `run` writes before it exits, or `None` to hang.
    pub(crate) run_output: Option<(String, i32)>,
    /// A share the guest fails to mount.
    pub(crate) unmounted: Option<String>,
    pub(crate) runs: Vec<proto::Run>,
    /// Says how each run went, in place of `run_output`.
    pub(crate) run_handler: Option<RunHandler>,
    /// The next seal fails.
    pub(crate) seal_fails: bool,
    /// What the guest's status reports went wrong at boot.
    pub(crate) problems: Vec<String>,
    /// How many more status calls report no network yet; `u32::MAX` for
    /// never.
    pub(crate) network_after: u32,
}

/// How a fake run goes, from what it was asked to run and the VM's
/// configuration.
pub(crate) type RunHandler = Arc<dyn Fn(&proto::Run, &VmConfig) -> proto::Ran + Send + Sync>;

#[derive(Debug, Default)]
pub(crate) struct Fake {
    pub(crate) state: Mutex<State>,
}

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("State")
            .field("calls", &self.calls)
            .finish_non_exhaustive()
    }
}

impl Fake {
    /// The VM at `socket` stops on its own, as one that crashed.
    pub(crate) fn kill(&self, socket: &Path) {
        self.with(socket, "killed", |vm| {
            vm.running = false;
            vm.session = None;
        });
    }

    /// The shell or Claude in the VM at `socket` exits with `code`.
    pub(crate) fn exit(&self, socket: &Path, code: i32) {
        self.with(socket, "exited", |vm| vm.session = Some(Some(code)));
    }

    pub(crate) fn calls(&self) -> Vec<String> {
        lock(&self.state).calls.clone()
    }

    pub(crate) fn vm(&self, socket: &Path) -> Vm {
        lock(&self.state)
            .vms
            .get(socket)
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) fn with<T>(&self, socket: &Path, call: &str, f: impl FnOnce(&mut Vm) -> T) -> T {
        let mut s = lock(&self.state);
        s.calls.push(call.to_owned());
        f(s.vms.entry(socket.to_owned()).or_default())
    }

    fn status_of(
        vm: &Vm,
        unmounted: Option<&str>,
        problems: &[String],
        network: bool,
    ) -> guest::Status {
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
            "layers": VmConfig::load(&vm.config)
                .map(|c| boot::Boot::parse(&c.cmdline).0.layers)
                .unwrap_or_default(),
            "shares": shares,
            "problems": problems,
            "network": network.then(|| json!({"interface": "eth0", "address": "192.168.64.5/24"})),
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
        let (unmounted, problems, network) = {
            let mut st = lock(&self.state);
            let network = st.network_after == 0;
            if st.network_after != u32::MAX {
                st.network_after = st.network_after.saturating_sub(1);
            }
            (st.unmounted.clone(), st.problems.clone(), network)
        };
        Ok(Fake::status_of(
            &vm,
            unmounted.as_deref(),
            &problems,
            network,
        ))
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
        // A thaw sets the clock first.
        self.with(socket, "thaw", |vm| {
            vm.frozen = false;
            vm.clock_set = true;
        });
        Ok(serde_json::from_value(json!({"shares_ms": 900, "thaw_ms": 1})).unwrap())
    }

    fn poweroff(&self, socket: &Path) -> Result<()> {
        self.with(socket, "poweroff", |vm| {
            vm.running = false;
            vm.session = None;
        });
        Ok(())
    }

    fn set_clock(&self, socket: &Path, _: SystemTime) -> Result<()> {
        self.with(socket, "clock", |vm| {
            if vm.running {
                vm.clock_set = true;
                Ok(())
            } else {
                Err(refused("not running"))
            }
        })
    }

    fn seal(&self, socket: &Path) -> Result<Vec<String>> {
        let fails = std::mem::take(&mut lock(&self.state).seal_fails);
        let vm = self.with(socket, "seal", |vm| {
            vm.session = None;
            vm.clone()
        });
        if !vm.running {
            return Err(refused("not running"));
        }
        if fails {
            return Err(refused("can't freeze home: Device or resource busy"));
        }
        Ok(VmConfig::load(&vm.config)
            .unwrap()
            .disks
            .into_iter()
            .filter(|d| !d.read_only)
            .map(|d| d.id)
            .collect())
    }

    fn run(&self, socket: &Path, run: &proto::Run) -> Result<proto::Ran> {
        let mut st = lock(&self.state);
        st.calls.push("run".into());
        st.runs.push(run.clone());
        let vm = st.vms.get(socket).cloned().unwrap_or_default();
        if !vm.running {
            return Err(refused("not running"));
        }
        // A guest whose clock was never set fails as apt does there.
        if !vm.clock_set {
            return Ok(proto::Ran {
                code: Some(100),
                signal: None,
                timed_out: false,
                output: "W: OpenPGP signature verification failed: Not live until 2026-10-31\n\
                         E: The repository 'http://deb.debian.org/debian trixie InRelease' is \
                         not signed.\n"
                    .into(),
            });
        }
        if let Some(handler) = st.run_handler.clone() {
            drop(st);
            return Ok(handler(run, &VmConfig::load(&vm.config).unwrap()));
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
