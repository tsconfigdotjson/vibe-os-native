//! The core handlers: the one implementation the socket, and through it the
//! CLI, the menu bar and MCP, all call (PRD: One core, three doors). It owns
//! the registry, host limits, API keys, the `gh` login check and the event
//! stream.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::broadcast;
use vibe_core::keys;
use vibe_core::limits::{self, LimitsError};
use vibe_core::model::{
    Environment, EventKind, GhState, GithubRepo, LOCAL_HOST, Profile, ProjectSource,
    starter_profiles, validate_key_name, validate_name,
};
use vibe_core::paths::Layout;
use vibe_core::rpc::{
    ApiKey, ErrorCode, ErrorObject, Event, GhStatus, GhStatusParams, HostLimits, HostList,
    HostParams, KeyCreateParams, KeyCreated, KeyList, KeyParams, KeyVerifyParams, LimitsSetParams,
    ProfileCreateParams, ProfileList, ProfileParams, ProfileUpdateParams, Project,
    ProjectAddParams, ProjectList, ProjectParams,
};
use vibe_core::store::{NewProject, Store, StoreError};
use vibe_core::time::rfc3339;

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

/// Everything [`Core::open`] needs. Tests swap the clock, the randomness and
/// `gh` for fixed ones.
pub struct Config {
    pub info: DaemonInfo,
    /// This host's name for people; the hostname by default.
    pub label: String,
    pub layout: Layout,
    /// The `gh` to run.
    pub gh: OsString,
    pub clock: Clock,
    pub entropy: Entropy,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("info", &self.info)
            .field("label", &self.label)
            .field("layout", &self.layout)
            .field("gh", &self.gh)
            .finish_non_exhaustive()
    }
}

impl Config {
    /// The real thing: the system clock, the OS's randomness and the `gh`
    /// [`find_program`] finds.
    pub fn new(info: DaemonInfo, layout: Layout) -> Self {
        Config {
            info,
            label: hostname(),
            layout,
            gh: find_program("gh", std::env::var_os("PATH").as_deref()),
            clock: Arc::new(vibe_core::time::now),
            entropy: Arc::new(keys::random_bytes),
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
    layout: Layout,
    gh: OsString,
    clock: Clock,
    entropy: Entropy,
    store: Mutex<Store>,
    /// One `host.toml` edit at a time.
    limits_lock: Mutex<()>,
    /// Names of projects being added. A clone can take minutes; meanwhile
    /// no other add may take its name or its directory, but other adds go
    /// ahead.
    adding: Mutex<HashSet<String>>,
    events: EventBus,
    gh_status: Mutex<GhStatus>,
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

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A handler that panicked mid-way leaves nothing half-done that matters
    // here: the store's transactions roll back on their own.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn error(code: ErrorCode, message: impl Into<String>) -> ErrorObject {
    ErrorObject::new(code, message)
}

fn store_error(e: StoreError) -> ErrorObject {
    match e {
        StoreError::NotFound(m) => error(ErrorCode::NotFound, m),
        StoreError::Conflict(m) => error(ErrorCode::Conflict, m),
        other => error(ErrorCode::InternalError, format!("Internal error: {other}")),
    }
}

fn limits_error(e: LimitsError) -> ErrorObject {
    match e {
        LimitsError::Invalid { .. } => error(ErrorCode::HostConfigInvalid, e.to_string()),
        LimitsError::Rejected(m) => error(ErrorCode::InvalidParams, format!("Invalid params: {m}")),
        LimitsError::Io { .. } => error(ErrorCode::InternalError, format!("Internal error: {e}")),
    }
}

fn invalid(message: impl std::fmt::Display) -> ErrorObject {
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
        match state_lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(OpenError::AlreadyRunning { dir }),
            Err(TryLockError::Error(source)) => {
                return Err(OpenError::Io {
                    what: "lock",
                    path: lock_path,
                    source,
                });
            }
        }
        let mut store = Store::open(&config.layout.db()).map_err(OpenError::Store)?;
        store
            .ensure_host(LOCAL_HOST, &config.info.os, &config.label, (config.clock)())
            .map_err(OpenError::Store)?;
        let (tx, _) = broadcast::channel(EVENT_BACKLOG);
        Ok(Core {
            info: config.info,
            label: config.label,
            layout: config.layout,
            gh: config.gh,
            clock: config.clock,
            entropy: config.entropy,
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
            _state_lock: state_lock,
        })
    }

    pub fn info(&self) -> &DaemonInfo {
        &self.info
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        lock(&self.store)
    }

    /// The host a call is for. Absent means this one. Calls for other hosts
    /// are forwarded once #16 links them; until then there are none.
    fn host(&self, host: Option<&str>) -> Result<&'static str, ErrorObject> {
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

    // Hosts.

    pub fn hosts(&self) -> Result<HostList, ErrorObject> {
        let hosts = self.store().hosts().map_err(store_error)?;
        Ok(HostList { hosts })
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
        let bytes = (self.entropy)().map_err(|e| {
            error(
                ErrorCode::InternalError,
                format!("Internal error: no randomness: {e}"),
            )
        })?;
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
        Ok(KeyCreated { key, secret })
    }

    pub fn revoke_key(&self, p: KeyParams) -> Result<ApiKey, ErrorObject> {
        let key = self.store().revoke_key(&p.name).map_err(store_error)?;
        crate::log!("vibed: revoked API key {} ({}…)", key.name, key.prefix);
        self.publish(self.event(EventKind::KeysChanged));
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
}
