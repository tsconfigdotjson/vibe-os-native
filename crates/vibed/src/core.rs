//! The core handlers: the one implementation the socket, and through it the
//! CLI, the menu bar and MCP, all call (PRD: One core, three doors). It owns
//! the registry, host limits, API keys, the `gh` login check, the event
//! stream, and finding the sessions agents run in. Agents themselves are in
//! [`crate::agents`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::Duration;

use tokio::sync::broadcast;
use vibe_core::keys;
use vibe_core::limits::{self, LimitsError};
use vibe_core::model::{
    Environment, EventKind, GhState, GithubRepo, LOCAL_HOST, Profile, ProjectSource,
    starter_profiles, validate_key_name, validate_name, validate_ref,
};
use vibe_core::paths::Layout;
use vibe_core::rpc::{
    ApiKey, ErrorCode, ErrorObject, Event, GhStatus, GhStatusParams, HostLimits, HostParams,
    KeyCreateParams, KeyCreated, KeyList, KeyParams, KeyVerifyParams, LimitsSetParams,
    ProfileCreateParams, ProfileList, ProfileParams, ProfileUpdateParams, Project,
    ProjectAddParams, ProjectList, ProjectParams, SessionAttach, SessionInfo, SessionList,
    SessionParams,
};
use vibe_core::store::{NewProject, Store, StoreError};
use vibe_core::time::rfc3339;

use crate::agents::{Admission, Backends, HostAdmission};
use crate::link::{Link, Timing};
use crate::remote::{Remote, Ssh};
use crate::sessions::{self, Probe, SessionSpec};
use crate::{gh, projects};

/// What `daemon.hello` reports about this daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonInfo {
    pub version: String,
    pub os: String,
    pub arch: String,
    pub pid: u32,
}

impl DaemonInfo {
    pub fn current() -> Self {
        DaemonInfo {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            pid: std::process::id(),
        }
    }
}

/// Seconds since the epoch, now.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The 256 random bits behind a new API key.
pub type Entropy = Arc<dyn Fn() -> io::Result<[u8; keys::KEY_BYTES]> + Send + Sync>;

/// This machine's Tailscale addresses now.
pub type TailscaleAddresses = Arc<dyn Fn() -> Vec<std::net::IpAddr> + Send + Sync>;

/// The free disk on the filesystem a path is on, or would be on; none
/// where that can't be told.
pub type FreeDisk = Arc<dyn Fn(&Path) -> Option<u64> + Send + Sync>;

/// Everything [`Core::open`] needs. Tests swap the clock, the randomness and
/// `gh` for fixed ones.
pub struct Config {
    pub info: DaemonInfo,
    /// This host's name for people; the hostname by default.
    pub label: String,
    pub layout: Layout,
    /// The `gh` to run.
    pub gh: OsString,
    /// The `vibe-shim` to start sessions with.
    pub shim: OsString,
    pub clock: Clock,
    pub entropy: Entropy,
    /// What runs agents, by environment.
    pub backends: Backends,
    /// Whether a new agent may start.
    pub admission: Arc<dyn Admission>,
    /// How short of memory the host is: the kernel's pressure stall
    /// information on Linux, by default.
    pub pressure: Arc<dyn crate::pressure::MemoryPressure>,
    /// The Linux toolbox: on a Mac that runs `linux` agents in VMs, none
    /// on a Linux host.
    pub toolbox: Option<Arc<dyn vibe_env::toolbox::ToolboxHost>>,
    /// Where MCP can be served on Tailscale: this machine's addresses in
    /// its ranges, by default.
    pub tailscale: TailscaleAddresses,
    /// How Linux hosts are reached: over SSH, by default.
    pub remote: Arc<dyn Remote>,
    /// How their links are timed.
    pub link_timing: Timing,
    /// What holds the host awake while agents are: an IOPM assertion on a
    /// Mac, by default.
    pub power: Arc<dyn crate::power::Power>,
    /// The free disk, for admission and snapshots: `statvfs`, by default.
    pub free_disk: FreeDisk,
    /// How the sleep watch is timed.
    pub sleep_timing: crate::sleep::Timing,
    /// How a Linux host serves the external address.
    pub external: crate::external::ExternalConfig,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("info", &self.info)
            .field("label", &self.label)
            .field("layout", &self.layout)
            .field("gh", &self.gh)
            .field("shim", &self.shim)
            .field("backends", &self.backends)
            .field("admission", &self.admission)
            .field("pressure", &self.pressure)
            .field("toolbox", &self.toolbox)
            .field("remote", &self.remote)
            .field("power", &self.power)
            .field("sleep_timing", &self.sleep_timing)
            .finish_non_exhaustive()
    }
}

impl Config {
    /// The real thing: the system clock, the OS's randomness, the `gh`
    /// [`find_program`] finds, the `vibe-shim` [`find_shim`] finds, and
    /// admission by this machine's memory and its memory pressure. No
    /// backends: `vibed`'s `main` picks them for the machine it's on.
    pub fn new(info: DaemonInfo, layout: Layout) -> Self {
        let path = std::env::var_os("PATH");
        Config {
            label: hostname(),
            layout,
            gh: find_program("gh", path.as_deref()),
            shim: find_shim(std::env::current_exe().ok().as_deref(), path.as_deref()),
            clock: Arc::new(vibe_core::time::now),
            entropy: Arc::new(keys::random_bytes),
            backends: Backends::new(),
            admission: Arc::new(HostAdmission::current()),
            pressure: Arc::new(crate::pressure::HostPressure),
            toolbox: None,
            tailscale: Arc::new(vibe_mcp::net::tailscale_addresses),
            remote: Arc::new(Ssh::new()),
            link_timing: Timing::default(),
            power: Arc::new(crate::power::HostPower::default()),
            free_disk: Arc::new(crate::sleep::free_space),
            sleep_timing: crate::sleep::Timing::default(),
            external: crate::external::ExternalConfig {
                serve_here: info.os == "linux",
                ..crate::external::ExternalConfig::default()
            },
            info,
        }
    }
}

/// Where Homebrew puts programs, on Apple Silicon and on Intel. launchd
/// starts `vibed` with a `PATH` of only the system directories, so these are
/// searched too.
const EXTRA_BIN_DIRS: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];

/// The first executable `name` on `path`, then in [`EXTRA_BIN_DIRS`]; or
/// just `name` if there's none, so running it fails with a clear error.
pub fn find_program(name: &str, path: Option<&OsStr>) -> OsString {
    let dirs = path
        .map(|p| std::env::split_paths(p).collect::<Vec<_>>())
        .unwrap_or_default();
    dirs.iter()
        .map(PathBuf::as_path)
        .chain(EXTRA_BIN_DIRS.iter().map(Path::new))
        .map(|d| d.join(name))
        .find(|p| {
            std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .map(PathBuf::into_os_string)
        .unwrap_or_else(|| name.into())
}

/// `vibe-shim` beside `exe`, as it's installed next to `vibed`; or else
/// the one [`find_program`] finds.
pub fn find_shim(exe: Option<&Path>, path: Option<&OsStr>) -> OsString {
    let beside = exe.and_then(Path::parent).map(|d| d.join("vibe-shim"));
    match beside {
        Some(p) if std::fs::metadata(&p).is_ok_and(|m| m.is_file()) => p.into_os_string(),
        _ => find_program("vibe-shim", path),
    }
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length, and gethostname writes at
    // most that many bytes.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    match rc {
        0 if len > 0 => String::from_utf8_lossy(&buf[..len]).into_owned(),
        _ => "this host".into(),
    }
}

#[derive(Debug)]
pub enum OpenError {
    /// Another `vibed` is using this state directory.
    AlreadyRunning {
        dir: PathBuf,
    },
    Io {
        what: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Store(StoreError),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::AlreadyRunning { dir } => {
                write!(f, "another vibed is already using {}", dir.display())
            }
            OpenError::Io { what, path, source } => {
                write!(f, "can't {what} {}: {source}", path.display())
            }
            OpenError::Store(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for OpenError {}

/// Events kept for a subscriber that's slow to read them.
const EVENT_BACKLOG: usize = 1024;

/// How long `Core::open` waits for the state lock before it takes it for
/// another `vibed`'s.
const LOCK_PATIENCE: Duration = Duration::from_secs(3);

#[derive(Debug)]
struct EventBus {
    tx: broadcast::Sender<Event>,
    /// The last `seq` given out. Held while sending, so events go out in
    /// `seq` order.
    last: Mutex<u64>,
}

pub struct Core {
    info: DaemonInfo,
    label: String,
    pub(crate) layout: Layout,
    gh: OsString,
    pub(crate) shim: OsString,
    clock: Clock,
    entropy: Entropy,
    pub(crate) backends: Backends,
    pub(crate) admission: Arc<dyn Admission>,
    pub(crate) pressure: Arc<dyn crate::pressure::MemoryPressure>,
    /// When each agent asleep went to sleep, since this `vibed` started, and
    /// in which order: memory pressure ends the one asleep longest first.
    pub(crate) asleep_since: Mutex<crate::pressure::Sleeps>,
    /// Agents memory pressure paused, and when: it ends each after a grace
    /// period if the pressure lasts, and lets it go if it eases.
    pub(crate) pressure_paused: Mutex<HashMap<String, std::time::Instant>>,
    /// What the sleep watch keeps of each agent between looks.
    pub(crate) sleep_watch: Mutex<crate::sleep::Watch>,
    /// Held by a sleep that saves to disk, from making room for its
    /// snapshot until it's saved.
    pub(crate) snapshotting: tokio::sync::Mutex<()>,
    pub(crate) sleep_timing: crate::sleep::Timing,
    pub(crate) power: Arc<dyn crate::power::Power>,
    pub(crate) free_disk: FreeDisk,
    /// Refs being started, deleted or resumed, one thing at a time each.
    pub(crate) agents_busy: Mutex<HashSet<String>>,
    /// One edit of Claude's state at a time: agents may share its file.
    pub(crate) claude_lock: Arc<Mutex<()>>,
    /// One admission at a time, so two starts can't both take the last room.
    pub(crate) admitting: tokio::sync::Mutex<()>,
    /// The memory of starts admitted but not yet running, by a token each.
    pub(crate) starting: Mutex<HashMap<u64, u64>>,
    /// Each agent's transcript, as the status engine follows it.
    pub(crate) tails: Mutex<HashMap<String, Arc<Mutex<vibe_status::Tail>>>>,
    /// Where each agent's transcript was at its last send over MCP: what
    /// `vibe_wait` waits for an answer after. None yet means its start's
    /// prompt was the last.
    pub(crate) sends: Mutex<HashMap<String, vibe_status::Mark>>,
    /// Held while a send types into an agent, so two can't interleave.
    pub(crate) typing: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// One fetch at a time per project, as concurrent fetches of the same
    /// refs fail on each other's locks.
    pub(crate) fetching: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    store: Mutex<Store>,
    /// One `host.toml` edit at a time.
    pub(crate) limits_lock: Mutex<()>,
    /// Names of projects being added. A clone can take minutes; meanwhile
    /// no other add may take its name or its directory, but other adds go
    /// ahead.
    adding: Mutex<HashSet<String>>,
    events: EventBus,
    gh_status: Mutex<GhStatus>,
    pub(crate) toolbox: Option<Arc<dyn vibe_env::toolbox::ToolboxHost>>,
    /// What the toolbox is doing, for `toolbox.status`.
    pub(crate) toolbox_state: Mutex<vibe_core::model::ToolboxState>,
    /// Held for one toolbox operation at a time.
    pub(crate) toolbox_op: tokio::sync::Mutex<()>,
    /// This machine's Tailscale addresses, for MCP.
    pub(crate) tailscale: TailscaleAddresses,
    /// Tells the MCP listeners to look at their settings again now.
    pub(crate) mcp_kick: Arc<tokio::sync::Notify>,
    /// Whether MCP is served on loopback now, for `hosts.list`.
    pub(crate) mcp_serving: AtomicBool,
    /// How Linux hosts are reached.
    pub(crate) remote: Arc<dyn Remote>,
    /// Each Linux host's link, by its name.
    pub(crate) links: Mutex<HashMap<String, Arc<Link>>>,
    pub(crate) link_timing: Timing,
    /// Names of hosts being added: an install takes a while.
    pub(crate) adding_hosts: Mutex<HashSet<String>>,
    /// This core, for the tasks it starts, once [`Core::spawn_links`] has
    /// run: links run only then.
    pub(crate) me: OnceLock<Weak<Core>>,
    /// The runtime those tasks run on, as a call may come from a thread
    /// outside it.
    pub(crate) runtime: OnceLock<tokio::runtime::Handle>,
    /// How a Linux host serves the external address.
    pub(crate) external: crate::external::ExternalConfig,
    /// On a Linux host, its public endpoint while it's the external address.
    pub(crate) public: Mutex<Option<crate::external::Public>>,
    /// On a Linux host, the Mac behind it, at the tunnel's end.
    pub(crate) peer: crate::external::MacPeer,
    /// On a Mac, the tunnel to the Linux host that's its external address.
    pub(crate) tunnel: Mutex<Option<crate::tunnel::Tunnel>>,
    /// On a Mac, the tunnel's socket, served while a host is the external
    /// address.
    pub(crate) tunnel_socket_served:
        Mutex<Option<(tokio::task::JoinHandle<()>, vibe_core::socket::SocketGuard)>>,
    /// On a Mac, when a host last came through the tunnel.
    pub(crate) tunnel_seen: Mutex<Option<std::time::Instant>>,
    _state_lock: File,
}

impl std::fmt::Debug for Core {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Core")
            .field("info", &self.info)
            .field("layout", &self.layout)
            .finish_non_exhaustive()
    }
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A handler that panicked mid-way leaves nothing half-done that matters
    // here: the store's transactions roll back on their own.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn error(code: ErrorCode, message: impl Into<String>) -> ErrorObject {
    ErrorObject::new(code, message)
}

pub(crate) fn store_error(e: StoreError) -> ErrorObject {
    match e {
        StoreError::NotFound(m) => error(ErrorCode::NotFound, m),
        StoreError::Conflict(m) => error(ErrorCode::Conflict, m),
        other => error(ErrorCode::InternalError, format!("Internal error: {other}")),
    }
}

pub(crate) fn limits_error(e: LimitsError) -> ErrorObject {
    match e {
        LimitsError::Invalid { .. } => error(ErrorCode::HostConfigInvalid, e.to_string()),
        LimitsError::Rejected(m) => error(ErrorCode::InvalidParams, format!("Invalid params: {m}")),
        LimitsError::Io { .. } => error(ErrorCode::InternalError, format!("Internal error: {e}")),
    }
}

pub(crate) fn invalid(message: impl std::fmt::Display) -> ErrorObject {
    error(
        ErrorCode::InvalidParams,
        format!("Invalid params: {message}"),
    )
}

impl Core {
    /// Opens the state directory, private to this user, and the registry in
    /// it, and records this host. Only one `vibed` may use a state directory
    /// at a time.
    pub fn open(config: Config) -> Result<Core, OpenError> {
        let dir = config.layout.data_dir();
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|source| OpenError::Io {
                what: "create",
                path: dir.clone(),
                source,
            })?;
        make_private(&dir)?;
        let lock_path = dir.join("vibed.lock");
        let state_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|source| OpenError::Io {
                what: "open",
                path: lock_path.clone(),
                source,
            })?;
        // A vibed that just stopped may not have let go yet in every
        // process: a child forked before it closed the lock, and not yet
        // replaced by its program, holds the lock too until it is. So the
        // lock gets a moment before it counts as another vibed's.
        let deadline = std::time::Instant::now() + LOCK_PATIENCE;
        loop {
            match state_lock.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(TryLockError::WouldBlock) => return Err(OpenError::AlreadyRunning { dir }),
                Err(TryLockError::Error(source)) => {
                    return Err(OpenError::Io {
                        what: "lock",
                        path: lock_path,
                        source,
                    });
                }
            }
        }
        let mut store = Store::open(&config.layout.db()).map_err(OpenError::Store)?;
        store
            .ensure_host(LOCAL_HOST, &config.info.os, &config.label, (config.clock)())
            .map_err(OpenError::Store)?;
        let (tx, _) = broadcast::channel(EVENT_BACKLOG);
        let tunnel_end = config.layout.run_dir().join(crate::external::TUNNEL_END);
        let core = Core {
            info: config.info,
            label: config.label,
            layout: config.layout,
            gh: config.gh,
            shim: config.shim,
            clock: config.clock,
            entropy: config.entropy,
            backends: config.backends,
            admission: config.admission,
            pressure: config.pressure,
            asleep_since: Mutex::new(crate::pressure::Sleeps::default()),
            pressure_paused: Mutex::new(HashMap::new()),
            sleep_watch: Mutex::new(crate::sleep::Watch::default()),
            snapshotting: tokio::sync::Mutex::new(()),
            sleep_timing: config.sleep_timing,
            power: config.power,
            free_disk: config.free_disk,
            agents_busy: Mutex::new(HashSet::new()),
            claude_lock: Arc::new(Mutex::new(())),
            admitting: tokio::sync::Mutex::new(()),
            starting: Mutex::new(HashMap::new()),
            fetching: Mutex::new(HashMap::new()),
            tails: Mutex::new(HashMap::new()),
            sends: Mutex::new(HashMap::new()),
            typing: Mutex::new(HashMap::new()),
            store: Mutex::new(store),
            limits_lock: Mutex::new(()),
            adding: Mutex::new(HashSet::new()),
            events: EventBus {
                tx,
                last: Mutex::new(0),
            },
            gh_status: Mutex::new(GhStatus {
                state: GhState::Unknown,
                account: None,
                detail: None,
                checked_at: None,
            }),
            toolbox: config.toolbox,
            toolbox_state: Mutex::new(vibe_core::model::ToolboxState::Closed),
            toolbox_op: tokio::sync::Mutex::new(()),
            tailscale: config.tailscale,
            mcp_kick: Arc::new(tokio::sync::Notify::new()),
            mcp_serving: AtomicBool::new(false),
            remote: config.remote,
            links: Mutex::new(HashMap::new()),
            link_timing: config.link_timing,
            adding_hosts: Mutex::new(HashSet::new()),
            me: OnceLock::new(),
            runtime: OnceLock::new(),
            peer: crate::external::MacPeer::new(tunnel_end),
            external: config.external,
            public: Mutex::new(None),
            tunnel: Mutex::new(None),
            tunnel_socket_served: Mutex::new(None),
            tunnel_seen: Mutex::new(None),
            _state_lock: state_lock,
        };
        core.load_pressure_pauses();
        core.load_links().map_err(|e| {
            OpenError::Store(StoreError::Conflict(format!(
                "can't read the Linux hosts: {}",
                e.message
            )))
        })?;
        Ok(core)
    }

    pub fn info(&self) -> &DaemonInfo {
        &self.info
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub(crate) fn now(&self) -> i64 {
        (self.clock)()
    }

    pub(crate) fn store(&self) -> MutexGuard<'_, Store> {
        lock(&self.store)
    }

    /// 32 random bytes, from the same source as API keys.
    pub(crate) fn random(&self) -> Result<[u8; keys::KEY_BYTES], ErrorObject> {
        (self.entropy)().map_err(|e| {
            error(
                ErrorCode::InternalError,
                format!("Internal error: no randomness: {e}"),
            )
        })
    }

    pub(crate) fn random_16(&self) -> Result<[u8; 16], ErrorObject> {
        let bytes = self.random()?;
        Ok(std::array::from_fn(|i| bytes[i]))
    }

    /// The host a call is for, when it's this one: absent means this one.
    /// A call for a Linux host goes over its link before it gets here
    /// ([`Core::forward`]).
    pub(crate) fn host(&self, host: Option<&str>) -> Result<&'static str, ErrorObject> {
        match host {
            None | Some(LOCAL_HOST) => Ok(LOCAL_HOST),
            Some(other) => Err(error(ErrorCode::NotFound, format!("no host named {other}"))),
        }
    }

    // Events.

    /// An event of `kind` on this host, now, for [`Core::publish`] to number.
    pub fn event(&self, kind: EventKind) -> Event {
        Event {
            seq: 0,
            at: rfc3339(self.now()),
            host: LOCAL_HOST.into(),
            kind,
            project: None,
            agent: None,
            state: None,
            limit: None,
            message: None,
            status: None,
        }
    }

    /// Numbers `event` and sends it to every subscriber.
    pub fn publish(&self, mut event: Event) {
        let mut last = lock(&self.events.last);
        *last += 1;
        event.seq = *last;
        // No subscribers is fine.
        let _ = self.events.tx.send(event);
    }

    /// A new subscription, and the `seq` of the last event before it.
    /// The `seq` of the last event published.
    pub fn last_event_seq(&self) -> u64 {
        *lock(&self.events.last)
    }

    pub fn subscribe(&self) -> (u64, broadcast::Receiver<Event>) {
        let last = lock(&self.events.last);
        (*last, self.events.tx.subscribe())
    }

    /// What a subscriber gets in place of the `missed` events it was too
    /// slow for, the last of which was `last_missed`. It takes that `seq`, so
    /// the subscriber's `seq`s still only go up.
    pub fn lagged(&self, missed: u64, last_missed: u64) -> Event {
        let mut e = self.event(EventKind::Lagged);
        e.seq = last_missed;
        e.message = Some(format!("missed {missed} events; read everything again"));
        e
    }

    // Projects.

    pub fn projects(&self, p: HostParams) -> Result<ProjectList, ErrorObject> {
        let host = self.host(p.host.as_deref())?;
        let projects = self.store().projects(host).map_err(store_error)?;
        Ok(ProjectList { projects })
    }

    /// Clones a GitHub repo, or registers a local one, and gives it the
    /// host's starter profiles.
    pub async fn add_project(&self, p: ProjectAddParams) -> Result<Project, ErrorObject> {
        let host = self.host(p.host.as_deref())?;
        let _reserved;
        let new = match (p.repo, p.path) {
            (Some(repo), None) => {
                let repo = GithubRepo::parse(&repo).map_err(invalid)?;
                let name = p.name.unwrap_or_else(|| repo.name.clone());
                _reserved = self.reserve_project(host, &name)?;
                let dest = self.layout.projects_dir().join(&name);
                let path = projects::github_clone(&self.gh, &repo, &dest).await?;
                NewProject {
                    host: host.into(),
                    name,
                    source: ProjectSource::Github,
                    repo: Some(repo.to_string()),
                    path,
                }
            }
            (None, Some(path)) => {
                let path = projects::local_repo(&path).await?;
                let name = match p.name {
                    Some(name) => name,
                    None => PathBuf::from(&path)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                };
                _reserved = self.reserve_project(host, &name)?;
                NewProject {
                    host: host.into(),
                    name,
                    source: ProjectSource::Path,
                    repo: None,
                    path,
                }
            }
            _ => return Err(invalid("give exactly one of `repo` and `path`")),
        };
        let profiles = starter_profiles(&self.info.os);
        let project = self
            .store()
            .add_project(&new, &profiles, self.now())
            .map_err(store_error)?;
        crate::log!("vibed: added project {} at {}", project.name, project.path);
        let mut e = self.event(EventKind::ProjectAdded);
        e.project = Some(project.name.clone());
        self.publish(e);
        Ok(project)
    }

    /// Holds `name` for a new project until the returned value is dropped.
    /// Fails early, before any cloning, if `name` can't be a new project.
    fn reserve_project(&self, host: &str, name: &str) -> Result<Reserved<'_>, ErrorObject> {
        validate_name("project", name).map_err(invalid)?;
        match self.store().project(host, name) {
            Ok(_) => {
                return Err(error(
                    ErrorCode::Conflict,
                    format!("a project named {name} already exists on {host}"),
                ));
            }
            Err(StoreError::NotFound(_)) => {}
            Err(e) => return Err(store_error(e)),
        }
        if !lock(&self.adding).insert(name.to_owned()) {
            return Err(error(
                ErrorCode::Conflict,
                format!("a project named {name} is being added already"),
            ));
        }
        Ok(Reserved {
            core: self,
            name: name.to_owned(),
        })
    }

    /// Forgets a project and its profiles; its files stay on disk.
    pub fn remove_project(&self, p: ProjectParams) -> Result<Project, ErrorObject> {
        let host = self.host(p.host.as_deref())?;
        let project = self
            .store()
            .remove_project(host, &p.project)
            .map_err(store_error)?;
        crate::log!(
            "vibed: removed project {}; {} is left as it was",
            project.name,
            project.path
        );
        let mut e = self.event(EventKind::ProjectRemoved);
        e.project = Some(project.name.clone());
        self.publish(e);
        Ok(project)
    }

    // Profiles.

    pub fn profiles(&self, p: ProjectParams) -> Result<ProfileList, ErrorObject> {
        let host = self.host(p.host.as_deref())?;
        let profiles = self
            .store()
            .profiles(host, &p.project)
            .map_err(store_error)?;
        Ok(ProfileList { profiles })
    }

    pub fn create_profile(&self, p: ProfileCreateParams) -> Result<Profile, ErrorObject> {
        let host = self.host(p.host.as_deref())?;
        let mut profile =
            Profile::with_defaults(p.name, p.environment.unwrap_or(Environment::Linux));
        profile.model = p.model.unwrap_or(profile.model);
        profile.reasoning = p.reasoning.unwrap_or(profile.reasoning);
        profile.mode = p.mode.unwrap_or(profile.mode);
        profile.memory = p.memory.unwrap_or(profile.memory);
        profile.validate(&self.info.os).map_err(invalid)?;
        let profile = self
            .store()
            .create_profile(host, &p.project, &profile)
            .map_err(store_error)?;
        self.profiles_changed(&p.project);
        Ok(profile)
    }

    pub fn update_profile(&self, p: ProfileUpdateParams) -> Result<Profile, ErrorObject> {
        let host = self.host(p.host.as_deref())?;
        let mut store = self.store();
        let mut profile = store
            .profile(host, &p.project, &p.profile)
            .map_err(store_error)?;
        profile.name = p.name.unwrap_or(profile.name);
        profile.environment = p.environment.unwrap_or(profile.environment);
        profile.model = p.model.unwrap_or(profile.model);
        profile.reasoning = p.reasoning.unwrap_or(profile.reasoning);
        profile.mode = p.mode.unwrap_or(profile.mode);
        profile.memory = p.memory.unwrap_or(profile.memory);
        profile.validate(&self.info.os).map_err(invalid)?;
        let profile = store
            .replace_profile(host, &p.project, &p.profile, &profile)
            .map_err(store_error)?;
        drop(store);
        self.profiles_changed(&p.project);
        Ok(profile)
    }

    pub fn delete_profile(&self, p: ProfileParams) -> Result<Profile, ErrorObject> {
        let host = self.host(p.host.as_deref())?;
        let profile = self
            .store()
            .delete_profile(host, &p.project, &p.profile)
            .map_err(store_error)?;
        self.profiles_changed(&p.project);
        Ok(profile)
    }

    fn profiles_changed(&self, project: &str) {
        let mut e = self.event(EventKind::ProfilesChanged);
        e.project = Some(project.to_owned());
        self.publish(e);
    }

    // Host limits.

    /// The limits as `host.toml` has them now, hand edits included.
    pub fn limits(&self, p: HostParams) -> Result<HostLimits, ErrorObject> {
        self.host(p.host.as_deref())?;
        limits::load(&self.layout.host_config()).map_err(limits_error)
    }

    pub fn set_limits(&self, p: LimitsSetParams) -> Result<HostLimits, ErrorObject> {
        self.host(p.host.as_deref())?;
        let changes = p.changes();
        if changes.is_empty() {
            return Err(invalid("give at least one setting to change"));
        }
        let _one_at_a_time = lock(&self.limits_lock);
        let limits = limits::update(&self.layout.host_config(), &changes).map_err(limits_error)?;
        let summary: Vec<String> = changes.iter().map(|(k, v)| format!("{k}={v}")).collect();
        crate::log!("vibed: host limits changed: {}", summary.join(", "));
        self.publish(self.event(EventKind::LimitsChanged));
        Ok(limits)
    }

    // API keys.

    pub fn keys(&self) -> Result<KeyList, ErrorObject> {
        let keys = self.store().keys().map_err(store_error)?;
        Ok(KeyList { keys })
    }

    pub fn create_key(&self, p: KeyCreateParams) -> Result<KeyCreated, ErrorObject> {
        validate_key_name(&p.name).map_err(invalid)?;
        let bytes = self.random()?;
        let secret = keys::encode(&bytes);
        let key = self
            .store()
            .create_key(
                &p.name,
                &keys::hash(&secret),
                &keys::shown_prefix(&secret),
                self.now(),
            )
            .map_err(store_error)?;
        crate::log!("vibed: created API key {} ({}…)", key.name, key.prefix);
        self.publish(self.event(EventKind::KeysChanged));
        self.push_keys();
        Ok(KeyCreated { key, secret })
    }

    pub fn revoke_key(&self, p: KeyParams) -> Result<ApiKey, ErrorObject> {
        let key = self.store().revoke_key(&p.name).map_err(store_error)?;
        crate::log!("vibed: revoked API key {} ({}…)", key.name, key.prefix);
        self.publish(self.event(EventKind::KeysChanged));
        self.push_keys();
        Ok(key)
    }

    /// The key `secret` is, marked as used now, or `None` if it isn't a
    /// current key. MCP (#12) checks every call with this.
    pub fn verify_key(&self, secret: &str) -> Result<Option<ApiKey>, ErrorObject> {
        if !keys::is_key(secret) {
            return Ok(None);
        }
        self.store()
            .use_key(&keys::hash(secret), self.now())
            .map_err(store_error)
    }

    pub fn verify_key_call(&self, p: KeyVerifyParams) -> Result<ApiKey, ErrorObject> {
        self.verify_key(&p.secret)?
            .ok_or_else(|| error(ErrorCode::NotFound, "that key is unknown or revoked"))
    }

    // GitHub login.

    pub async fn gh_status(&self, p: GhStatusParams) -> GhStatus {
        if p.refresh == Some(true) {
            self.check_gh().await
        } else {
            lock(&self.gh_status).clone()
        }
    }

    /// Runs `gh auth status` now, and publishes a `gh.status` event if the
    /// result changed.
    pub async fn check_gh(&self) -> GhStatus {
        let check = gh::check(&self.gh).await;
        let status = GhStatus {
            state: check.state,
            account: check.account,
            detail: Some(check.detail),
            checked_at: Some(rfc3339(self.now())),
        };
        let changed = {
            let mut last = lock(&self.gh_status);
            let changed = (last.state, &last.account) != (status.state, &status.account);
            *last = status.clone();
            changed
        };
        if changed {
            crate::log!(
                "vibed: gh: {} ({})",
                status.state,
                status.detail.as_deref().unwrap_or("")
            );
            let mut e = self.event(EventKind::GhStatus);
            e.state = Some(status.state.to_string());
            e.message = status.detail.clone();
            self.publish(e);
        }
        status
    }

    /// Checks `gh` now and then every `every`, for as long as the runtime
    /// runs.
    pub fn spawn_gh_checks(self: &Arc<Self>, every: Duration) -> tokio::task::JoinHandle<()> {
        let core = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(every);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticks.tick().await;
                core.check_gh().await;
            }
        })
    }

    /// Logs a warning if `host.toml` is unreadable, so a hand edit gone
    /// wrong shows up at startup rather than at the first `limits.get`.
    pub fn warn_about_limits(&self) {
        if let Err(e) = limits::load(&self.layout.host_config()) {
            crate::log!("vibed: warning: {e}");
        }
    }

    /// The label this host goes by.
    pub fn label(&self) -> &str {
        &self.label
    }

    // Sessions.

    /// What `vibe attach` names an agent's window: `<ref> · <profile>`, or
    /// the ref alone when there's no agent by that ref.
    fn titles(&self, host: &str) -> Result<HashMap<String, String>, ErrorObject> {
        let agents = self.store().agents(host).map_err(store_error)?;
        Ok(agents
            .into_iter()
            .map(|a| {
                let title = format!("{} · {}", a.agent_ref, a.profile);
                (a.agent_ref, title)
            })
            .collect())
    }

    /// Asks the shim behind every socket in `run/` what it holds, and
    /// removes sockets left by shims that are gone.
    async fn scan_sessions(&self) -> Vec<(String, Probe)> {
        // All at once: a stuck shim costs one probe's timeout, not one each.
        let mut probes = tokio::task::JoinSet::new();
        for r in sessions::socket_refs(&self.layout.run_dir()) {
            let path = self.layout.session_socket(&r);
            probes.spawn(async move {
                let probe = sessions::probe(&path).await;
                (r, path, probe)
            });
        }
        let mut found = Vec::new();
        while let Some(joined) = probes.join_next().await {
            let Ok((r, path, probe)) = joined else {
                continue;
            };
            match &probe {
                Probe::Gone => match sessions::remove_stale(&path) {
                    Ok(true) => crate::log!("vibed: removed the stale socket of session {r}"),
                    Ok(false) => {}
                    Err(e) => crate::log!("vibed: can't remove {}: {e}", path.display()),
                },
                Probe::Broken(why) => crate::log!("vibed: session {r} isn't answering: {why}"),
                Probe::Running(_) => {}
            }
            found.push((r, probe));
        }
        found.sort_by(|a, b| a.0.cmp(&b.0));
        found
    }

    /// Finds the sessions that kept running while `vibed` was down, as on
    /// every start (PRD: Restarts), and returns how many there are.
    pub async fn adopt_sessions(&self) -> usize {
        self.undo_cut_short_starts().await;
        self.asleep_after_reboot().await;
        let mut adopted = 0;
        for (r, probe) in self.scan_sessions().await {
            if let Probe::Running(status) = probe {
                adopted += 1;
                crate::log!(
                    "vibed: re-adopted session {r}: shim pid {}, command pid {}, {} attached",
                    status.pid,
                    status.child_pid,
                    status.clients.len()
                );
            }
        }
        self.hold_agents_to_limits().await;
        adopted
    }

    /// A host's sessions: those running, then those that exited and left a
    /// record, merged by ref.
    pub async fn sessions(&self, p: HostParams) -> Result<SessionList, ErrorObject> {
        let host = self.host(p.host.as_deref())?;
        let titles = self.titles(host)?;
        let title = |r: &str| titles.get(r).cloned().unwrap_or_else(|| r.to_owned());
        let mut found = BTreeMap::new();
        for (r, probe) in self.scan_sessions().await {
            let info = match probe {
                Probe::Running(status) => sessions::running(host, title(&r), &status),
                // It's there, so it's running, even if it isn't answering.
                Probe::Broken(_) => SessionInfo {
                    host: host.to_owned(),
                    agent: r.clone(),
                    state: vibe_core::model::SessionState::Running,
                    title: title(&r),
                    clients: 0,
                    ttys: Vec::new(),
                    exit: None,
                },
                Probe::Gone => continue,
            };
            found.insert(r, info);
        }
        for r in sessions::record_refs(&self.layout.agents_dir()) {
            if found.contains_key(&r) {
                continue;
            }
            if let Some(record) = sessions::read_record(&self.layout, &r) {
                let info = sessions::exited(host, &r, title(&r), record.exit);
                found.insert(r, info);
            }
        }
        Ok(SessionList {
            sessions: found.into_values().collect(),
        })
    }

    /// Where to attach to a running session. The client then talks to the
    /// shim itself, so a `vibed` restart doesn't touch it.
    pub async fn attach_session(&self, p: SessionParams) -> Result<SessionAttach, ErrorObject> {
        let host = self.host(p.host.as_deref())?;
        validate_ref(&p.agent).map_err(invalid)?;
        let socket = self.layout.session_socket(&p.agent);
        // An attach wakes an agent asleep, or frozen by memory pressure
        // (PRD: Going to sleep; How a click becomes a Terminal window),
        // exactly where it was, or with `claude --resume` where it can't
        // be. One going to sleep right now is waited for, then woken.
        let agent = self
            .store()
            .agent(host, &p.agent)
            .ok()
            .filter(|a| !a.starting);
        if agent.is_some() {
            self.wait_not_busy(&p.agent).await;
            let a = self.store().agent(host, &p.agent).map_err(store_error)?;
            let frozen = a.state == vibe_core::model::AgentState::Asleep
                || (a.state == vibe_core::model::AgentState::Paused
                    && lock(&self.pressure_paused).contains_key(&p.agent));
            if frozen {
                self.resume_agent(Some(host), &p.agent).await?;
                crate::log!("vibed: {} was {}, and an attach woke it", p.agent, a.state);
            }
            // The client connects in a moment; until then, and for its
            // debounce after, it's not idle.
            lock(&self.sleep_watch).woke(&p.agent);
        }
        match sessions::probe(&socket).await {
            Probe::Running(status) => {
                let title = self
                    .titles(host)?
                    .remove(&p.agent)
                    .unwrap_or_else(|| p.agent.clone());
                Ok(SessionAttach {
                    host: host.to_owned(),
                    agent: p.agent,
                    title,
                    socket: socket.to_string_lossy().into_owned(),
                    ttys: sessions::ttys(&status),
                })
            }
            Probe::Broken(why) => Err(error(
                ErrorCode::InternalError,
                format!("Internal error: session {} isn't answering: {why}", p.agent),
            )),
            Probe::Gone => {
                let _ = sessions::remove_stale(&socket);
                Err(match sessions::read_record(&self.layout, &p.agent) {
                    Some(record) => error(
                        ErrorCode::NotFound,
                        format!(
                            "session {} isn't running: its command {} at {}",
                            p.agent,
                            sessions::describe_exit(&record.exit),
                            record.exit.at
                        ),
                    ),
                    None => error(
                        ErrorCode::NotFound,
                        format!("no session {} on {host}", p.agent),
                    ),
                })
            }
        }
    }

    /// Starts a session: a detached `vibe-shim` running `spec`'s command on
    /// a new terminal. Agents (#4) start Claude this way.
    pub async fn start_session(&self, spec: SessionSpec) -> Result<SessionInfo, ErrorObject> {
        validate_ref(&spec.agent).map_err(invalid)?;
        let pid = sessions::spawn_shim(&self.shim, &self.layout, &spec)
            .await
            .map_err(|m| error(ErrorCode::CommandFailed, m))?;
        crate::log!("vibed: started session {} (shim pid {pid})", spec.agent);
        let title = self
            .titles(LOCAL_HOST)?
            .remove(&spec.agent)
            .unwrap_or_else(|| spec.agent.clone());
        match sessions::probe(&self.layout.session_socket(&spec.agent)).await {
            Probe::Running(status) => Ok(sessions::running(LOCAL_HOST, title, &status)),
            // It started, and already ended. The shim writes its record
            // before it removes its socket, so the record is there.
            Probe::Gone => match sessions::read_record(&self.layout, &spec.agent) {
                Some(record) => Ok(sessions::exited(
                    LOCAL_HOST,
                    &spec.agent,
                    title,
                    record.exit,
                )),
                None => Err(error(
                    ErrorCode::InternalError,
                    format!(
                        "Internal error: session {} started, then vanished",
                        spec.agent
                    ),
                )),
            },
            Probe::Broken(why) => Err(error(
                ErrorCode::InternalError,
                format!(
                    "Internal error: session {} isn't answering: {why}",
                    spec.agent
                ),
            )),
        }
    }
}

/// A project name held by an add in progress.
struct Reserved<'a> {
    core: &'a Core,
    name: String,
}

impl Drop for Reserved<'_> {
    fn drop(&mut self) {
        lock(&self.core.adding).remove(&self.name);
    }
}

/// Makes `dir` private to this user if it's this user's, and refuses it if
/// it's someone else's: it holds the registry and the API key hashes.
fn make_private(dir: &Path) -> Result<(), OpenError> {
    let io = |what, source| OpenError::Io {
        what,
        path: dir.to_owned(),
        source,
    };
    let meta = std::fs::metadata(dir).map_err(|e| io("read", e))?;
    // SAFETY: geteuid has no preconditions and can't fail.
    if meta.uid() != unsafe { libc::geteuid() } {
        return Err(io(
            "use",
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("it belongs to uid {}", meta.uid()),
            ),
        ));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| io("make private", e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programs_are_found_on_path_first() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        for d in [&a, &b] {
            std::fs::create_dir(d).unwrap();
        }
        std::fs::write(a.join("tool"), "").unwrap(); // not executable
        std::fs::write(b.join("tool"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(b.join("tool"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([&a, &b]).unwrap();
        assert_eq!(
            find_program("tool", Some(&path)),
            b.join("tool").into_os_string()
        );
        assert_eq!(
            find_program("vibe-no-such-tool", Some(&path)),
            "vibe-no-such-tool"
        );
        assert_eq!(find_program("vibe-no-such-tool", None), "vibe-no-such-tool");
    }

    #[test]
    fn the_shim_beside_vibed_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let (bin, other) = (tmp.path().join("bin"), tmp.path().join("other"));
        for d in [&bin, &other] {
            std::fs::create_dir(d).unwrap();
        }
        std::fs::write(other.join("vibe-shim"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            other.join("vibe-shim"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let path = std::env::join_paths([&other]).unwrap();
        let vibed = bin.join("vibed");
        // Nothing beside vibed: the one on PATH.
        assert_eq!(
            find_shim(Some(&vibed), Some(&path)),
            other.join("vibe-shim").into_os_string()
        );
        std::fs::write(bin.join("vibe-shim"), "").unwrap();
        assert_eq!(
            find_shim(Some(&vibed), Some(&path)),
            bin.join("vibe-shim").into_os_string()
        );
        assert_eq!(find_shim(None, None), "vibe-shim");
    }
}
