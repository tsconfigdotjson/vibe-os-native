//! The `linux` backend on the Mac: each agent in a micro-VM of its own
//! (PRD: Environments and profiles; What's inside `linux`; Sleep and wake).
//!
//! A compartment is the agent's `vm.json`, its writable disk and its own
//! Claude home, all in its directory, plus a snapshot while it sleeps:
//!
//! - **create** writes `vm.json` with the newest layers ([`Images`]), the
//!   agent's sparse writable disk (`vm_disk`), and the shares: the
//!   worktree and the project's `.git` at their host paths, the agent's
//!   Claude home at `~/.claude`, and the package caches unless
//!   `package_caches = false`. The home is seeded from the Linux Claude
//!   config, and `vibe-vm create` gives the VM the identity it keeps for
//!   life.
//! - **launch** boots the VM if it isn't running, taking the newest layers
//!   on the way (so a new Claude release is in the next fresh boot), and
//!   starts Claude in it through `vibe-guest` with the Claude token from
//!   the Keychain, which nothing writes down. A shim then holds its
//!   terminal through `vibe-vm`.
//! - **sleep** freezes Claude, then `vibe-vm` saves the whole VM and exits;
//!   **wake** restores it, deletes the snapshot and thaws Claude exactly
//!   where it was, on a new shim. A snapshot that won't restore is dropped,
//!   and the agent resumes with `claude --resume` in a fresh VM.
//! - The VM's vCPUs and RAM are fixed at boot, by the kernel. The worktree
//!   is measured on the host, and past `disk_hard` the agent is frozen
//!   until a wake (PRD: How `macos` is governed, Disk, applied here too).
//!
//! Everything that talks to `vibe-vm` and `vibe-guest` goes through
//! [`Machine`], so the policy here is tested without a Mac.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use vibe_core::claude_token::{TOKEN_ENV, TokenStore};
use vibe_core::limits::{self, HostLimits};
use vibe_core::paths::{Layout, private_dir};
use vibe_core::units::MIB;

use crate::claude::{CONFIG_DIR_ENV, ClaudeHome};
use crate::guest::{self, GuestState, boot, proto};
use crate::images::{Images, Layers};
use crate::tree::SessionTree;
use crate::vm::{self, Disk, Share, VmConfig, VmError};
use crate::{
    AgentSpec, Backend, Capabilities, Compartment, EnvError, Handle, Launch, Limit, Limiter,
    Limits, Proc, Result, Run, RunOutput, Sleeper, TerminalEnd, Usage, Woke,
};

/// The kernel command line before `vibe-guest`'s words.
pub const CMDLINE: &str = "console=hvc0 quiet";

/// Where the agent's Claude home is mounted in the guest.
pub const GUEST_CLAUDE_HOME: &str = "/home/vibe/.claude";

/// The package caches every `linux` agent shares, by tag: the host
/// directory under `~/VibeOS/cache/`, and where the tool looks in the guest.
pub const CACHES: [(&str, &str, &str); 5] = [
    ("cache-npm", "npm", "/home/vibe/.npm"),
    ("cache-pnpm", "pnpm", "/home/vibe/.local/share/pnpm/store"),
    (
        "cache-cargo",
        "cargo-registry",
        "/home/vibe/.cargo/registry",
    ),
    ("cache-cargo-git", "cargo-git", "/home/vibe/.cargo/git"),
    ("cache-pip", "pip", "/home/vibe/.cache/pip"),
];

/// How long a booting guest may take to answer.
const BOOT_PATIENCE: Duration = Duration::from_secs(60);

/// How long a restored guest may take to answer.
const RESTORE_PATIENCE: Duration = Duration::from_secs(30);

/// How long a power-off may take before the VM is stopped outright.
const POWEROFF_PATIENCE: Duration = Duration::from_secs(15);

/// How long a stop waits for the shim to exit on its own, once the VM has,
/// before it ends it.
const SHIM_PATIENCE: Duration = Duration::from_secs(3);

/// How often the worktree is measured against `disk_hard` (PRD: Disk).
const DISK_EVERY: Duration = Duration::from_secs(60);

/// What talks to `vibe-vm` and `vibe-guest` for the backend: [`Real`] on a
/// Mac, a fake in tests. Sockets are `run/<ref>.vm`.
pub trait Machine: Send + Sync + fmt::Debug {
    /// `vibe-vm create`: the VM's identity, written into `config`.
    fn create(&self, config: &Path) -> Result<()>;
    /// Boots the VM fresh in a detached `vibe-vm`, deleting any snapshot.
    fn boot(&self, config: &Path, socket: &Path, log: &Path) -> std::result::Result<(), VmError>;
    /// Restores it from its snapshot, which is then deleted.
    fn restore(&self, config: &Path, socket: &Path, log: &Path)
    -> std::result::Result<(), VmError>;
    /// Whether a `vibe-vm` runs the VM.
    fn running(&self, socket: &Path) -> bool;
    /// Saves the frozen VM to its snapshot; `vibe-vm` then exits.
    fn sleep(&self, socket: &Path, balloon_floor: u64) -> Result<()>;
    /// Stops the VM without saving, and waits for `vibe-vm` to go.
    fn stop(&self, socket: &Path) -> Result<()>;
    /// Waits up to `timeout` for `vibe-vm` to exit.
    fn wait_gone(&self, socket: &Path, timeout: Duration) -> bool;
    fn ready(&self, socket: &Path, timeout: Duration) -> Result<guest::Status>;
    fn status(&self, socket: &Path) -> Result<guest::Status>;
    fn launch(&self, socket: &Path, launch: &guest::Launch) -> Result<()>;
    fn processes(&self, socket: &Path) -> Result<guest::Processes>;
    fn freeze(&self, socket: &Path) -> Result<guest::Frozen>;
    fn thaw(&self, socket: &Path, now: SystemTime) -> Result<guest::Thawed>;
    /// Ends the agent, syncs and powers off; `vibe-vm` then exits.
    fn poweroff(&self, socket: &Path) -> Result<()>;
    /// Ends the command and leaves the writable disks clean, to be kept:
    /// the ids of those it sealed.
    fn seal(&self, socket: &Path) -> Result<Vec<String>>;
    /// Sets the guest's clock to `now`: a VM has none of its own to boot
    /// with, and TLS and apt's signatures fail on a wrong one.
    fn set_clock(&self, socket: &Path, now: SystemTime) -> Result<()>;
    /// Runs a command to completion in the guest, beside the agent's.
    fn run(&self, socket: &Path, run: &proto::Run) -> Result<proto::Ran>;
    /// Dials a port in the guest, for the shared services' relay.
    fn dial(&self, _socket: &Path, _port: u32) -> Result<std::os::unix::net::UnixStream> {
        Err(EnvError::new("this machine can't dial its guest"))
    }
}

/// How long dialing a port in the guest may take.
const DIAL_TIMEOUT: Duration = Duration::from_secs(2);

/// The real thing: `vibe-vm` and the guest behind it.
#[derive(Debug, Clone)]
pub struct Real {
    /// `vibe-vm`, signed with the virtualization entitlement.
    pub vibe_vm: PathBuf,
}

fn env_err(what: &str) -> impl FnOnce(guest::GuestError) -> EnvError + '_ {
    move |e| EnvError::new(format!("{what}: {e}"))
}

impl Machine for Real {
    fn dial(&self, socket: &Path, port: u32) -> Result<std::os::unix::net::UnixStream> {
        vm::connect_within(socket, port, DIAL_TIMEOUT)
            .map_err(|e| EnvError::new(format!("can't dial the guest: {e}")))
    }

    fn create(&self, config: &Path) -> Result<()> {
        vm::create(&self.vibe_vm, config)
            .map(drop)
            .map_err(|e| EnvError::new(format!("vibe-vm create: {e}")))
    }

    fn boot(&self, config: &Path, socket: &Path, log: &Path) -> std::result::Result<(), VmError> {
        vm::boot(&vm::Launch {
            vibe_vm: &self.vibe_vm,
            config,
            socket,
            log,
        })
        .map(drop)
    }

    fn restore(
        &self,
        config: &Path,
        socket: &Path,
        log: &Path,
    ) -> std::result::Result<(), VmError> {
        vm::restore(&vm::Launch {
            vibe_vm: &self.vibe_vm,
            config,
            socket,
            log,
        })
        .map(drop)
    }

    fn running(&self, socket: &Path) -> bool {
        vm::lock_held(socket)
    }

    fn sleep(&self, socket: &Path, balloon_floor: u64) -> Result<()> {
        vm::sleep(socket, None, Some(balloon_floor))
            .map_err(|e| EnvError::new(format!("the VM couldn't be saved: {e}")))?;
        // It replies once the VM is saved and stopped, and exits just after:
        // a wake must find it gone.
        if vm::wait_gone(socket, POWEROFF_PATIENCE) {
            Ok(())
        } else {
            Err(EnvError::new(
                "the VM was saved, but its vibe-vm didn't exit",
            ))
        }
    }

    fn stop(&self, socket: &Path) -> Result<()> {
        if let Err(e) = vm::stop(socket)
            && vm::lock_held(socket)
        {
            return Err(EnvError::new(format!("the VM won't stop: {e}")));
        }
        if vm::wait_gone(socket, POWEROFF_PATIENCE) {
            Ok(())
        } else {
            Err(EnvError::new("the VM's vibe-vm didn't exit"))
        }
    }

    fn wait_gone(&self, socket: &Path, timeout: Duration) -> bool {
        vm::wait_gone(socket, timeout)
    }

    fn ready(&self, socket: &Path, timeout: Duration) -> Result<guest::Status> {
        guest::wait_ready(socket, timeout).map_err(env_err("vibe-guest didn't answer"))
    }

    fn status(&self, socket: &Path) -> Result<guest::Status> {
        guest::status(socket).map_err(env_err("vibe-guest's status"))
    }

    fn launch(&self, socket: &Path, launch: &guest::Launch) -> Result<()> {
        guest::launch(socket, launch)
            .map(drop)
            .map_err(env_err("vibe-guest couldn't start it"))
    }

    fn processes(&self, socket: &Path) -> Result<guest::Processes> {
        guest::processes(socket).map_err(env_err("vibe-guest's processes"))
    }

    fn freeze(&self, socket: &Path) -> Result<guest::Frozen> {
        guest::freeze(socket).map_err(env_err("vibe-guest couldn't freeze the agent"))
    }

    fn thaw(&self, socket: &Path, now: SystemTime) -> Result<guest::Thawed> {
        guest::thaw(socket, now, None).map_err(env_err("vibe-guest couldn't thaw the agent"))
    }

    fn poweroff(&self, socket: &Path) -> Result<()> {
        guest::poweroff(socket).map_err(env_err("vibe-guest couldn't power off"))
    }

    fn run(&self, socket: &Path, run: &proto::Run) -> Result<proto::Ran> {
        guest::run(socket, run).map_err(env_err("vibe-guest couldn't run it"))
    }

    fn set_clock(&self, socket: &Path, now: SystemTime) -> Result<()> {
        guest::set_clock(socket, now)
            .map(drop)
            .map_err(env_err("vibe-guest couldn't set the clock"))
    }

    fn seal(&self, socket: &Path) -> Result<Vec<String>> {
        guest::seal(socket)
            .map(|s| s.disks)
            .map_err(env_err("vibe-guest couldn't seal the disks"))
    }
}

/// Where the Claude token comes from, for each launch.
pub type TokenSource = Arc<dyn Fn() -> Result<String> + Send + Sync>;

/// The token `vibe setup-token` kept in the Keychain.
pub fn keychain_token() -> TokenSource {
    Arc::new(|| match TokenStore::current().load() {
        Ok(Some(token)) => Ok(token),
        Ok(None) => Err(EnvError::new(
            "linux agents have no Claude login yet: run `vibe setup-token`",
        )),
        Err(e) => Err(EnvError::new(format!(
            "can't read the Claude login for linux agents: {e}"
        ))),
    })
}

/// The `linux` backend on the Mac.
pub struct LinuxVm {
    layout: Layout,
    images: Images,
    machine: Arc<dyn Machine>,
    token: TokenSource,
    /// What's run as Claude Code in the guest: the managed layer's.
    claude: String,
    host_cpus: usize,
    disk_every: Duration,
    supervisors: Mutex<HashMap<String, Supervisor>>,
    /// Held to pause and to wake, so a supervisor can't freeze an agent a
    /// wake has just thawed.
    gate: Arc<Mutex<()>>,
}

impl fmt::Debug for LinuxVm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinuxVm")
            .field("images", &self.images)
            .field("machine", &self.machine)
            .field("claude", &self.claude)
            .finish_non_exhaustive()
    }
}

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

impl LinuxVm {
    /// The backend for this Mac: VMs through `vibe_vm`, the Claude token
    /// from the Keychain.
    pub fn new(layout: Layout, vibe_vm: PathBuf) -> LinuxVm {
        LinuxVm::with_machine(layout, Arc::new(Real { vibe_vm }), keychain_token())
    }

    pub fn with_machine(layout: Layout, machine: Arc<dyn Machine>, token: TokenSource) -> LinuxVm {
        LinuxVm {
            images: Images::new(layout.linux_dir()),
            layout,
            machine,
            token,
            claude: "claude".into(),
            host_cpus: std::thread::available_parallelism().map_or(1, |n| n.get()),
            disk_every: DISK_EVERY,
            supervisors: Mutex::new(HashMap::new()),
            gate: Arc::new(Mutex::new(())),
        }
    }

    /// Runs `command` as Claude Code instead of the managed layer's
    /// `claude`, as the conformance suite runs its stand-in.
    pub fn with_claude(mut self, command: impl Into<String>) -> LinuxVm {
        self.claude = command.into();
        self
    }

    /// Measures worktrees this often instead of every minute.
    pub fn with_disk_check(mut self, every: Duration) -> LinuxVm {
        self.disk_every = every;
        self
    }

    pub fn images(&self) -> &Images {
        &self.images
    }

    fn socket(&self, h: &Handle) -> PathBuf {
        self.layout.vm_socket(&h.agent_ref)
    }

    fn config_path(h: &Handle) -> PathBuf {
        h.agent_dir.join("vm.json")
    }

    fn vm_log(h: &Handle) -> PathBuf {
        h.agent_dir.join("vibe-vm.log")
    }

    fn snapshot(&self, h: &Handle) -> PathBuf {
        self.layout.snapshot(&h.agent_ref)
    }

    fn host_limits(&self) -> Result<HostLimits> {
        limits::load(&self.layout.host_config()).map_err(|e| EnvError::new(e.to_string()))
    }

    /// The VM's configuration for `spec`, on `layers`.
    fn config(&self, spec: &AgentSpec, layers: &Layers, host: &HostLimits) -> Result<VmConfig> {
        let (_, managed) = layers.managed.clone().ok_or_else(|| {
            EnvError::new(
                "Claude Code for linux agents isn't downloaded yet: vibed fetches it when it \
                 starts, and daily",
            )
        })?;
        let dir = &spec.agent_dir;
        let path = |p: &Path| p.to_string_lossy().into_owned();
        let mut shares = vec![
            (
                "worktree".to_owned(),
                spec.worktree.clone(),
                path(&spec.worktree),
            ),
            ("git".to_owned(), spec.git_dir.clone(), path(&spec.git_dir)),
            (
                "claude".to_owned(),
                dir.join("claude"),
                GUEST_CLAUDE_HOME.to_owned(),
            ),
        ];
        if host.package_caches {
            for (tag, name, at) in CACHES {
                shares.push((tag.into(), self.layout.cache_dir().join(name), at.into()));
            }
        }
        let mut config = VmConfig {
            version: vm::CONFIG_VERSION,
            agent: spec.agent_ref.clone(),
            cpus: vm::vcpus(spec.limits.cpu_cores, self.host_cpus),
            memory: spec.limits.memory / MIB * MIB,
            kernel: layers.base.kernel.clone(),
            initrd: Some(layers.base.initrd.clone()),
            cmdline: String::new(),
            machine_id: None,
            mac: None,
            disks: Vec::new(),
            shares: shares
                .iter()
                .map(|(tag, host, _)| Share {
                    tag: tag.clone(),
                    path: host.clone(),
                    read_only: false,
                })
                .collect(),
            console_log: Some(dir.join("console.log")),
            snapshot: self.layout.snapshot(&spec.agent_ref),
        };
        let boot = boot::Boot {
            layers: Vec::new(),
            upper: Some("agent".into()),
            home: None,
            sudo: false,
            shares: shares
                .iter()
                .map(|(tag, _, at)| boot::Share {
                    tag: tag.clone(),
                    path: at.clone(),
                })
                .collect(),
            // SAFETY: getuid and getgid have no memory effects.
            uid: Some(unsafe { libc::getuid() }),
            gid: Some(unsafe { libc::getgid() }),
        };
        set_layers(&mut config, boot, layers, &managed, &dir.join("disk.img"))?;
        Ok(config)
    }

    /// Before a fresh boot: the newest layers into `vm.json`, keeping the
    /// VM's identity, size and shares. A snapshot depends on the disks it
    /// was saved with, so this never runs while there's one.
    fn take_newest_layers(&self, h: &Handle) -> Result<()> {
        let path = LinuxVm::config_path(h);
        let mut config = VmConfig::load(&path)
            .map_err(|e| EnvError::new(format!("{}'s VM: {e}", h.agent_ref)))?;
        let layers = self.images.layers()?;
        let Some((_, managed)) = layers.managed.clone() else {
            return Ok(());
        };
        let (boot, _) = boot::Boot::parse(&config.cmdline);
        let agent = config
            .disks
            .iter()
            .find(|d| !d.read_only)
            .map(|d| d.path.clone())
            .unwrap_or_else(|| h.agent_dir.join("disk.img"));
        config.kernel = layers.base.kernel.clone();
        config.initrd = Some(layers.base.initrd.clone());
        set_layers(&mut config, boot, &layers, &managed, &agent)?;
        config
            .save(&path)
            .map_err(|e| EnvError::new(format!("can't update {}: {e}", path.display())))
    }

    /// Boots the VM if no `vibe-vm` runs it, and waits for the guest.
    fn ensure_running(&self, h: &Handle) -> Result<()> {
        let socket = self.socket(h);
        if self.machine.running(&socket) {
            return Ok(());
        }
        self.take_newest_layers(h)?;
        self.machine
            .boot(&LinuxVm::config_path(h), &socket, &LinuxVm::vm_log(h))
            .map_err(|e| EnvError::new(format!("{}'s VM won't boot: {e}", h.agent_ref)))?;
        let status = self.machine.ready(&socket, BOOT_PATIENCE)?;
        // Without its layers the guest runs from its initrd, with no Claude.
        if status.layers.is_empty() {
            let _ = self.machine.stop(&socket);
            return Err(EnvError::new(format!(
                "{}'s VM couldn't stack its disks: {}",
                h.agent_ref,
                status.problems.join("; ")
            )));
        }
        // A share that didn't mount leaves an empty directory where it
        // should be: Claude would work there unseen, and write its
        // transcript to the agent's own disk.
        let config = VmConfig::load(&LinuxVm::config_path(h))
            .map_err(|e| EnvError::new(format!("{}'s VM: {e}", h.agent_ref)))?;
        let missing: Vec<&str> = config
            .shares
            .iter()
            .map(|s| s.tag.as_str())
            .filter(|tag| !status.shares.iter().any(|m| m.tag == *tag))
            .collect();
        if !missing.is_empty() {
            let _ = self.machine.stop(&socket);
            return Err(EnvError::new(format!(
                "{}'s VM couldn't mount {}: {}",
                h.agent_ref,
                missing.join(", "),
                status.problems.join("; ")
            )));
        }
        // Before anything runs in it: `.vibe/setup.sh` comes before the
        // launch, which sets the clock too.
        if let Err(e) = self.machine.set_clock(&socket, SystemTime::now()) {
            let _ = self.machine.stop(&socket);
            return Err(e);
        }
        Ok(())
    }

    fn unsupervise(&self, h: &Handle) {
        let _gate = lock(&self.gate);
        lock(&self.supervisors).remove(&h.agent_ref);
    }

    fn paused(&self, h: &Handle) -> Option<Limit> {
        lock(&self.supervisors)
            .get(&h.agent_ref)
            .and_then(|s| *lock(&s.paused))
    }

    /// Waits up to `timeout` for the shim on the agent's terminal to exit,
    /// as it does a moment after the VM's stream goes. True once it has, or
    /// if there's none.
    fn shim_gone_within(h: &Handle, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while LinuxVm::shim_alive(h) {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    /// Whether the shim that held the agent's terminal is still there.
    fn shim_alive(h: &Handle) -> bool {
        h.session.is_some_and(|sid| {
            let tree = SessionTree::new(sid);
            tree.is_shims() && tree.all().iter().any(|p| p.pid == sid)
        })
    }

    fn remove_lock(&self, h: &Handle) {
        let socket = self.socket(h);
        if !self.machine.running(&socket) {
            let mut lock = socket.into_os_string();
            lock.push(".lock");
            let _ = fs::remove_file(PathBuf::from(lock));
        }
    }
}

/// Puts `layers` into `config`: the read-only disks, then the agent's own,
/// and the command line that stacks them, managed layer on top.
fn set_layers(
    config: &mut VmConfig,
    mut boot: boot::Boot,
    layers: &Layers,
    managed: &Path,
    agent_disk: &Path,
) -> Result<()> {
    let mut disks = vec![("base", layers.base.disk.clone())];
    if let Some(t) = &layers.template {
        disks.push(("template", t.clone()));
    }
    disks.push(("managed", managed.to_owned()));
    if let Some(home) = &layers.home {
        disks.push(("home", home.clone()));
    }
    // Top first: the managed layer, the home, the template, the base.
    let order = ["managed", "home", "template", "base"];
    boot.layers = order
        .iter()
        .filter(|id| disks.iter().any(|(d, _)| d == *id))
        .map(|id| (*id).to_owned())
        .collect();
    boot.upper = Some("agent".into());
    config.disks = disks
        .into_iter()
        .map(|(id, path)| Disk {
            id: id.into(),
            path,
            read_only: true,
        })
        .chain([Disk {
            id: "agent".into(),
            path: agent_disk.to_owned(),
            read_only: false,
        }])
        .collect();
    config.cmdline = boot
        .append_to(CMDLINE)
        .map_err(|e| EnvError::new(format!("the VM's command line: {e}")))?;
    Ok(())
}

fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(EnvError::new(format!(
            "can't remove {}: {e}",
            path.display()
        ))),
        _ => Ok(()),
    }
}

/// The bytes in `dir` on disk, not following symlinks.
fn disk_usage(dir: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let mut total = 0;
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for e in entries.filter_map(|e| e.ok()) {
            let Ok(meta) = e.path().symlink_metadata() else {
                continue;
            };
            total += meta.blocks() * 512;
            if meta.is_dir() {
                stack.push(e.path());
            }
        }
    }
    total
}

/// A signal's number on Linux, from the name the guest reports.
fn linux_signal(name: &str) -> Option<i32> {
    let names = [
        "SIGHUP", "SIGINT", "SIGQUIT", "SIGILL", "SIGTRAP", "SIGABRT", "SIGBUS", "SIGFPE",
        "SIGKILL", "SIGUSR1", "SIGSEGV", "SIGUSR2", "SIGPIPE", "SIGALRM", "SIGTERM",
    ];
    names.iter().position(|n| *n == name).map(|i| i as i32 + 1)
}

impl Compartment for LinuxVm {
    fn create(&self, spec: &AgentSpec) -> Result<Handle> {
        let host = self.host_limits()?;
        let layers = self.images.layers()?;
        let config = self.config(spec, &layers, &host)?;
        let home = ClaudeHome::own(spec.agent_dir.join("claude"));
        private_dir(&home.dir)
            .map_err(|e| EnvError::new(format!("can't create {}: {e}", home.dir.display())))?;
        home.seed(&self.layout.linux_claude_dir())
            .and_then(|()| home.mark_managed())
            .map_err(|e| EnvError::new(format!("can't set up the agent's Claude home: {e}")))?;
        // The caches, like the agent's own directories, are this user's
        // alone, whatever the umask; the worktree and `.git` are as they are.
        for share in &config.shares {
            let made = if share.tag.starts_with("cache-") {
                private_dir(&share.path)
            } else {
                fs::create_dir_all(&share.path)
            };
            made.map_err(|e| EnvError::new(format!("can't create {}: {e}", share.path.display())))?;
        }
        // Snapshots hold all of a VM's memory, the Claude token included.
        if let Some(parent) = config.snapshot.parent() {
            private_dir(parent)
                .map_err(|e| EnvError::new(format!("can't create {}: {e}", parent.display())))?;
        }
        let disk = spec.agent_dir.join("disk.img");
        vm::create_disk(&disk, host.vm_disk)
            .map_err(|e| EnvError::new(format!("the agent's disk: {e}")))?;
        let path = spec.agent_dir.join("vm.json");
        config
            .save(&path)
            .map_err(|e| EnvError::new(format!("can't write {}: {e}", path.display())))?;
        self.machine.create(&path)?;
        Ok(Handle {
            agent_ref: spec.agent_ref.clone(),
            agent_dir: spec.agent_dir.clone(),
            worktree: spec.worktree.clone(),
            claude: home,
            session: None,
        })
    }

    fn run(&self, h: &Handle, r: &Run) -> Result<RunOutput> {
        if r.argv.is_empty() {
            return Err(EnvError::new("nothing to run"));
        }
        self.ensure_running(h)?;
        // Beside Claude, if it runs: vibe-guest runs it and says how it went.
        let ran = self.machine.run(
            &self.socket(h),
            &proto::Run {
                command: r.argv.clone(),
                env: r.env.iter().cloned().collect(),
                cwd: Some(r.cwd.to_string_lossy().into_owned()),
                timeout_ms: (r.timeout.as_millis() as u64).max(1),
            },
        )?;
        Ok(RunOutput {
            code: ran.code,
            signal: ran.signal.as_deref().and_then(linux_signal),
            timed_out: ran.timed_out,
            output: ran.output.into_bytes(),
        })
    }

    fn launch(&self, h: &Handle, launch: &Launch) -> Result<TerminalEnd> {
        let token = (self.token)()?;
        h.claude
            .mark_managed()
            .map_err(|e| EnvError::new(format!("can't set up Claude's state: {e}")))?;
        self.ensure_running(h)?;
        let socket = self.socket(h);
        let mut command = vec![self.claude.clone()];
        command.extend(launch.args.iter().cloned());
        let mut env: std::collections::BTreeMap<String, String> =
            launch.env.iter().cloned().collect();
        env.insert(CONFIG_DIR_ENV.into(), GUEST_CLAUDE_HOME.into());
        // The managed layer keeps Claude current (PRD: Exceptions).
        env.insert("DISABLE_AUTOUPDATER".into(), "1".into());
        // Nor is it where Claude's own installer would put it, which Claude
        // otherwise warns about (PRD: Spike results).
        env.insert("DISABLE_INSTALLATION_CHECKS".into(), "1".into());
        env.insert(TOKEN_ENV.into(), token);
        self.machine.launch(
            &socket,
            &guest::Launch {
                command,
                env,
                cwd: Some(launch.cwd.to_string_lossy().into_owned()),
                size: None,
                time_ms: Some(guest::millis(SystemTime::now())),
            },
        )?;
        Ok(TerminalEnd::Vm { socket })
    }

    fn services_program(&self) -> Option<String> {
        Some(vibe_relay::local::GUEST_PROGRAM.to_owned())
    }

    fn services_unsupported(&self, h: &Handle) -> Result<Option<String>> {
        // What counts is the guest this VM really runs: a template built on an
        // older base goes on booting that base's.
        self.ensure_running(h)?;
        let status = self.machine.status(&self.socket(h))?;
        Ok((!status.services).then(|| {
            format!(
                "its VM's guest, vibe-guest {}, is too old for shared services: Rebuild Template \
                 in Settings → Linux Toolbox moves new agents to a base image that has them",
                status.version
            )
        }))
    }

    fn services_relay(&self, h: &Handle) -> Result<Option<std::os::unix::net::UnixStream>> {
        let socket = self.socket(h);
        if !self.machine.running(&socket) || !self.machine.status(&socket)?.services {
            return Ok(None);
        }
        self.machine
            .dial(&socket, vibe_relay::frame::VSOCK_PORT)
            .map(Some)
    }

    fn processes(&self, h: &Handle) -> Result<Vec<Proc>> {
        let socket = self.socket(h);
        if !self.machine.running(&socket) {
            return Ok(Vec::new());
        }
        let frozen = self
            .machine
            .status(&socket)
            .map(|s| s.frozen)
            .unwrap_or(false);
        let procs = self.machine.processes(&socket)?;
        // The guest says when each started by its uptime; a Proc says it in
        // the host's time, as the transcript does.
        let now = guest::millis(SystemTime::now());
        let uptime = procs.uptime_ms;
        let started = |at: u64| now.saturating_sub(uptime.saturating_sub(at));
        Ok(procs
            .processes
            .into_iter()
            .map(|p| Proc {
                pid: p.pid,
                ppid: p.ppid,
                name: p.name,
                args: p.args,
                stopped: frozen || p.state == "T",
                started_ms: started(p.started_ms),
                cpu_ms: p.cpu_ms,
                memory: p.rss,
            })
            .collect())
    }

    fn stop(&self, h: &Handle) -> Result<()> {
        self.unsupervise(h);
        let socket = self.socket(h);
        if self.machine.running(&socket) {
            let powered_off = self.machine.poweroff(&socket).is_ok()
                && self.machine.wait_gone(&socket, POWEROFF_PATIENCE);
            if !powered_off {
                self.machine.stop(&socket)?;
            }
        }
        // Its shim goes too (Compartment::stop): on its own once the VM's
        // stream has, or it's ended. Until then its socket is still served,
        // and a delete can't remove it.
        if !LinuxVm::shim_gone_within(h, SHIM_PATIENCE)
            && let Some(sid) = h.session
            && !SessionTree::new(sid).end(Duration::from_secs(2))
        {
            return Err(EnvError::new(format!(
                "{}'s shim (pid {sid}) didn't end",
                h.agent_ref
            )));
        }
        // Asleep, its processes are in the snapshot: they end with it.
        remove_file(&self.snapshot(h))
    }

    fn delete(&self, h: &Handle) -> Result<()> {
        self.stop(h)?;
        for name in ["disk.img", "vm.json", "console.log", "vibe-vm.log"] {
            remove_file(&h.agent_dir.join(name))?;
        }
        remove_file(&h.agent_dir.join("vm.json.tmp"))?;
        h.claude
            .forget(&h.worktree)
            .map_err(|e| EnvError::new(format!("can't remove Claude's state: {e}")))?;
        self.remove_lock(h);
        Ok(())
    }
}

impl Limiter for LinuxVm {
    /// The VM's vCPUs and RAM were fixed at boot; this holds its worktree
    /// to `disk_hard`.
    fn apply(&self, h: &Handle, limits: &Limits) -> Result<()> {
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(Mutex::new(None));
        let _gate = lock(&self.gate);
        {
            let (stop, paused, gate) = (
                Arc::clone(&stop),
                Arc::clone(&paused),
                Arc::clone(&self.gate),
            );
            let (machine, socket, worktree) = (
                Arc::clone(&self.machine),
                self.socket(h),
                h.worktree.clone(),
            );
            let (hard, every) = (limits.disk_hard, self.disk_every);
            std::thread::Builder::new()
                .name(format!("disk {}", h.agent_ref))
                .spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        if disk_usage(&worktree) > hard {
                            let _gate = lock(&gate);
                            if stop.load(Ordering::SeqCst) {
                                return;
                            }
                            if lock(&paused).is_none()
                                && machine.running(&socket)
                                && machine.freeze(&socket).is_ok()
                            {
                                *lock(&paused) = Some(Limit::Disk);
                            }
                        }
                        let until = Instant::now() + every;
                        while Instant::now() < until && !stop.load(Ordering::SeqCst) {
                            std::thread::sleep(every.min(Duration::from_millis(50)));
                        }
                    }
                })?;
        }
        lock(&self.supervisors).insert(h.agent_ref.clone(), Supervisor { stop, paused });
        Ok(())
    }

    fn usage(&self, h: &Handle) -> Result<Usage> {
        let procs = self.processes(h)?;
        Ok(Usage {
            memory: procs.iter().map(|p| p.memory).sum(),
            cpu_ms: procs.iter().map(|p| p.cpu_ms).sum(),
            processes: procs.len() as u32,
            paused: self.paused(h),
            ..Usage::default()
        })
    }
}

impl Sleeper for LinuxVm {
    fn sleep(&self, h: &Handle) -> Result<()> {
        let socket = self.socket(h);
        if !self.machine.running(&socket) {
            return Err(EnvError::new(format!("{} isn't running", h.agent_ref)));
        }
        // Frozen already if a limit paused it; a second freeze is harmless.
        let frozen = self.machine.freeze(&socket)?;
        if let Err(e) = self
            .machine
            .sleep(&socket, guest::balloon_floor(frozen.memory))
        {
            // vibe-vm resumes a VM it couldn't save; so does the agent.
            if self.machine.running(&socket) {
                let _ = self.machine.thaw(&socket, SystemTime::now());
            }
            return Err(e);
        }
        // Its limits are held again when it wakes on a new shim.
        self.unsupervise(h);
        // The shim on its terminal goes once vibe-vm has: until then, a wake
        // would find its session still running, or its socket still taken.
        if !LinuxVm::shim_gone_within(h, POWEROFF_PATIENCE) {
            return Err(EnvError::new(format!(
                "{} is saved, but its shim (pid {}) didn't exit",
                h.agent_ref,
                h.session.unwrap_or_default()
            )));
        }
        Ok(())
    }

    fn wake(&self, h: &Handle) -> Result<Woke> {
        let socket = self.socket(h);
        let end = TerminalEnd::Vm {
            socket: socket.clone(),
        };
        if self.machine.running(&socket) {
            let status = self.machine.status(&socket)?;
            if status.frozen {
                let _gate = lock(&self.gate);
                self.machine.thaw(&socket, SystemTime::now())?;
                if let Some(s) = lock(&self.supervisors).get(&h.agent_ref) {
                    *lock(&s.paused) = None;
                }
            }
            return Ok(match status.state {
                GuestState::Running if LinuxVm::shim_alive(h) => Woke::Exact,
                // Claude goes on in the VM; a new shim reconnects to it.
                GuestState::Running => Woke::Reattach(end),
                GuestState::Idle | GuestState::Exited => Woke::Resume,
            });
        }
        if !self.snapshot(h).exists() {
            return Ok(Woke::Resume);
        }
        match self
            .machine
            .restore(&LinuxVm::config_path(h), &socket, &LinuxVm::vm_log(h))
        {
            Ok(()) => {}
            Err(e) if e.code() == Some(vm::ErrorCode::RestoreFailed) => {
                // PRD: Failures. The snapshot goes, and the agent resumes
                // with `claude --resume` in a fresh VM.
                remove_file(&self.snapshot(h))?;
                return Ok(Woke::Resume);
            }
            Err(e) => {
                return Err(EnvError::new(format!(
                    "{}'s VM won't restore: {e}",
                    h.agent_ref
                )));
            }
        }
        self.machine.ready(&socket, RESTORE_PATIENCE)?;
        self.machine.thaw(&socket, SystemTime::now())?;
        Ok(Woke::Reattach(end))
    }

    /// What the guest really uses now, plus its headroom: the balloon
    /// hands the rest back before the save, so that's about the snapshot.
    fn sleep_size(&self, h: &Handle) -> Result<Option<u64>> {
        let socket = self.socket(h);
        if !self.machine.running(&socket) {
            return Err(EnvError::new(format!("{} isn't running", h.agent_ref)));
        }
        let status = self.machine.status(&socket)?;
        Ok(Some(guest::balloon_floor(status.memory)))
    }

    fn saved(&self, h: &Handle) -> Option<crate::Saved> {
        let meta = fs::metadata(self.snapshot(h))
            .ok()
            .filter(fs::Metadata::is_file)?;
        Some(crate::Saved {
            bytes: meta.len(),
            at: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        })
    }

    /// Its VM is in the snapshot alone, so it goes with it; a wake then
    /// boots fresh, for `claude --resume`.
    fn drop_saved(&self, h: &Handle) -> Result<()> {
        if self.machine.running(&self.socket(h)) {
            return Err(EnvError::new(format!(
                "{}'s VM is running, so its snapshot isn't its own to drop",
                h.agent_ref
            )));
        }
        remove_file(&self.snapshot(h))
    }
}

impl Backend for LinuxVm {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            sleep_frees_memory: true,
            exact_after_reboot: true,
            kernel_limits: true,
            file_containment: true,
            // A restore with a real Claude's memory: 0.5 to 4 s (PRD).
            wake_cost: Duration::from_secs(4),
        }
    }
}

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod tests;
