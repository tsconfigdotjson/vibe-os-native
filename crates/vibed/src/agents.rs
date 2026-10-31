//! Agents: one Claude Code in its own worktree and compartment, made and
//! deleted as a unit (PRD: Core concepts; Key user flows, Start an agent and
//! Delete). One ref names all three, and the branch too.
//!
//! A start, in order: admission; a fetch of `origin` and a worktree on a new
//! branch; the compartment, through the environment's backend; the
//! worktree's `.vibe/setup.sh`, run inside it; Claude's state pre-filled;
//! then Claude on the prompt, in a session of its own, held to its limits.
//! If any step fails, the ones before are undone. A delete refuses while the
//! worktree has uncommitted changes or unpushed commits, unless forced, and
//! otherwise removes everything VibeOS made for the agent but its branch.
//!
//! Policy here reads only the `vibe-env` traits and `Capabilities`, never
//! which backend it has.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, DirBuilder, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use vibe_core::limits::{self, HostLimits};
use vibe_core::model::{
    AgentState, Environment, EventKind, LOCAL_HOST, Mode, ref_from_bytes, validate_ref,
};
use vibe_core::rpc::{
    Agent, AgentDeleteParams, AgentList, AgentStartParams, ErrorCode, ErrorObject, HostParams,
};
use vibe_core::socket;
use vibe_core::store::{AgentRecord, StoreError};
use vibe_core::time::rfc3339;
use vibe_core::units::{GIB, MIB};
use vibe_env::{AgentSpec, Backend, Handle, Launch, Limits, Run, TerminalEnd, Woke};

use crate::core::{Core, error, invalid, lock, store_error};
use crate::projects::{git, git_within};
use crate::sessions::{self, Probe, SessionSpec};

/// How long `.vibe/setup.sh` may run before the start gives up on it.
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How long a fetch of `origin` may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Draws of randomness for a free ref before giving up.
const REF_TRIES: usize = 8;

/// The script a repo can have run in every new agent's compartment.
pub const SETUP_SCRIPT: &str = ".vibe/setup.sh";

/// Where its output is kept, in the agent's directory.
pub const SETUP_LOG: &str = "setup.log";

/// What git is set to in every agent: a first `git push` sets up its
/// branch's upstream, and git never collects garbage on its own, as several
/// agents share one `.git`; `vibed` runs `git maintenance` instead.
const AGENT_GIT_CONFIG: [(&str, &str); 3] = [
    ("push.autoSetupRemote", "true"),
    ("gc.auto", "0"),
    ("maintenance.auto", "false"),
];

/// The backends this host runs agents with, by environment. One without a
/// backend can't start agents yet.
#[derive(Clone, Default)]
pub struct Backends(HashMap<Environment, Arc<dyn Backend>>);

impl fmt::Debug for Backends {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.0.iter()).finish()
    }
}

impl Backends {
    pub fn new() -> Backends {
        Backends::default()
    }

    /// Runs `environment`'s agents with `backend`.
    pub fn with(mut self, environment: Environment, backend: Arc<dyn Backend>) -> Backends {
        self.0.insert(environment, backend);
        self
    }

    /// Runs every environment's agents with `backend`, as tests do.
    pub fn all(backend: Arc<dyn Backend>) -> Backends {
        Environment::ALL
            .iter()
            .fold(Backends::new(), |b, e| b.with(*e, Arc::clone(&backend)))
    }

    pub fn get(&self, environment: Environment) -> Option<Arc<dyn Backend>> {
        self.0.get(&environment).cloned()
    }
}

/// An agent that holds memory on the host, as admission counts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Awake {
    pub agent_ref: String,
    /// Its profile's memory, which it may grow to.
    pub memory: u64,
}

/// Decides whether a new agent may start (PRD: Host limits). Sleeping
/// agents early to make room comes with #6, through this.
pub trait Admission: Send + Sync + fmt::Debug {
    /// Whether an agent of `memory` bytes may start beside `awake`, or why
    /// not.
    fn admit(&self, memory: u64, limits: &HostLimits, awake: &[Awake]) -> Result<(), String>;
}

/// Admission by the host's memory: each awake agent reserves its profile's
/// memory, and `reserve_memory` is always left for the host; and by
/// `max_awake_agents`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostAdmission {
    /// The host's physical memory.
    pub total_memory: u64,
}

impl HostAdmission {
    /// For this machine.
    pub fn current() -> HostAdmission {
        // SAFETY: sysconf has no preconditions.
        let (pages, size) = unsafe {
            (
                libc::sysconf(libc::_SC_PHYS_PAGES),
                libc::sysconf(libc::_SC_PAGESIZE),
            )
        };
        let total = u64::try_from(pages)
            .unwrap_or(0)
            .saturating_mul(u64::try_from(size).unwrap_or(0));
        HostAdmission {
            total_memory: total,
        }
    }
}

impl Admission for HostAdmission {
    fn admit(&self, memory: u64, limits: &HostLimits, awake: &[Awake]) -> Result<(), String> {
        if awake.len() >= limits.max_awake_agents as usize {
            return Err(format!(
                "{} agents are awake already, the most this host takes (max_awake_agents)",
                awake.len()
            ));
        }
        let held: u64 = awake.iter().map(|a| a.memory).sum();
        let room = self
            .total_memory
            .saturating_sub(limits.reserve_memory)
            .saturating_sub(held);
        if memory > room {
            return Err(format!(
                "there's no room for an agent with {} of memory: this host has {}, keeps {} for \
                 itself (reserve_memory), and its awake agents hold {}",
                size(memory),
                size(self.total_memory),
                size(limits.reserve_memory),
                size(held)
            ));
        }
        Ok(())
    }
}

/// A size for people: `4G`, `1.5G`, `512M`.
fn size(bytes: u64) -> String {
    if bytes >= GIB {
        let tenths = (bytes * 10 + GIB / 2) / GIB;
        if tenths.is_multiple_of(10) {
            format!("{}G", tenths / 10)
        } else {
            format!("{}.{}G", tenths / 10, tenths % 10)
        }
    } else {
        format!("{}M", bytes.div_ceil(MIB))
    }
}

fn failed(message: impl Into<String>) -> ErrorObject {
    error(ErrorCode::CommandFailed, message)
}

fn internal(message: impl fmt::Display) -> ErrorObject {
    error(
        ErrorCode::InternalError,
        format!("Internal error: {message}"),
    )
}

/// Runs a backend call on a thread that may block.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> vibe_env::Result<T> + Send + 'static,
) -> Result<T, ErrorObject> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(internal)?
        .map_err(|e| failed(e.to_string()))
}

/// Claude's arguments for an agent: a new session on `prompt`, or with
/// `resume`, the session it had.
pub fn claude_args(a: &AgentRecord, prompt: Option<&str>, resume: bool) -> Vec<String> {
    let mut args = vec![
        if resume { "--resume" } else { "--session-id" }.to_owned(),
        a.session_id.clone(),
        "--model".into(),
        a.model.to_string(),
        "--effort".into(),
        a.reasoning.to_string(),
    ];
    match a.mode {
        Mode::Auto => args.extend(["--permission-mode".into(), "auto".into()]),
        Mode::Bypass => args.push("--dangerously-skip-permissions".into()),
    }
    if let Some(prompt) = prompt {
        // So a prompt starting with `-` is never taken for an option.
        args.extend(["--".into(), prompt.to_owned()]);
    }
    args
}

/// The environment every agent's commands get on top of the
/// compartment's: git's settings for agents, as `GIT_CONFIG_*`, which
/// change nothing in the shared `.git/config`.
pub fn agent_env() -> Vec<(String, String)> {
    let mut env = vec![(
        "GIT_CONFIG_COUNT".to_owned(),
        AGENT_GIT_CONFIG.len().to_string(),
    )];
    for (i, (key, value)) in AGENT_GIT_CONFIG.iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{i}"), (*key).to_owned()));
        env.push((format!("GIT_CONFIG_VALUE_{i}"), (*value).to_owned()));
    }
    env
}

/// A UUID, version 4, from 16 random bytes.
fn uuid(mut b: [u8; 16]) -> String {
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// `from`, unless it could be taken for an option or isn't a name.
fn check_from(from: &str) -> Result<(), ErrorObject> {
    if from.is_empty()
        || from.starts_with('-')
        || from.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(invalid(format!("`{from}` isn't a branch or commit")));
    }
    Ok(())
}

/// The commit a new agent's branch starts at: `from` (a branch, preferring
/// `origin`'s unless the local one has commits it hasn't, or any commit),
/// or `origin`'s default branch, or without an `origin` the project's
/// `HEAD`. `origin` is fetched first.
async fn base_commit(project: &Path, from: Option<&str>) -> Result<String, ErrorObject> {
    let remotes = git(project, &["remote"])
        .await?
        .map_err(|why| failed(format!("{} isn't a git repo: {why}", project.display())))?;
    let has_origin = remotes.lines().any(|r| r == "origin");
    if has_origin {
        // Without git's own maintenance after it: that waits for a quiet
        // period. Not pruning keeps the remote-tracking branches of merged
        // and deleted PRs, so their commits still count as pushed.
        git_within(
            project,
            &[
                "-c",
                "maintenance.auto=false",
                "fetch",
                "--quiet",
                "--no-prune",
                "origin",
            ],
            FETCH_TIMEOUT,
        )
        .await?
        .map_err(|why| failed(format!("can't fetch origin: {why}")))?;
    }
    let base = match from {
        None if has_origin => default_branch(project).await?,
        None => "HEAD".to_owned(),
        Some(from) => {
            check_from(from)?;
            let local = format!("refs/heads/{from}");
            let remote = format!("refs/remotes/origin/{from}");
            match (
                commit(project, &local).await?,
                commit(project, &remote).await?,
            ) {
                (Some(_), Some(_)) => {
                    let behind = git(project, &["merge-base", "--is-ancestor", &local, &remote])
                        .await?
                        .is_ok();
                    if behind { remote } else { local }
                }
                (Some(_), None) => local,
                (None, Some(_)) => remote,
                (None, None) => from.to_owned(),
            }
        }
    };
    commit(project, &base).await?.ok_or_else(|| {
        error(
            ErrorCode::NotFound,
            format!(
                "{} has no branch or commit {}",
                project.display(),
                from.unwrap_or(&base)
            ),
        )
    })
}

/// `origin`'s default branch, as its remote-tracking ref.
async fn default_branch(project: &Path) -> Result<String, ErrorObject> {
    let head = ["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"];
    if let Ok(r) = git(project, &head).await? {
        return Ok(r);
    }
    // A repo that was cloned long ago, or by hand, may not know it.
    git(project, &["remote", "set-head", "origin", "--auto"])
        .await?
        .map_err(|why| failed(format!("can't tell origin's default branch: {why}")))?;
    git(project, &head)
        .await?
        .map_err(|why| failed(format!("can't tell origin's default branch: {why}")))
}

/// The commit `rev` names, if it names one.
async fn commit(dir: &Path, rev: &str) -> Result<Option<String>, ErrorObject> {
    let spec = format!("{rev}^{{commit}}");
    Ok(git(dir, &["rev-parse", "--verify", "--quiet", &spec])
        .await?
        .ok())
}

/// Why deleting this agent would lose work, if it would: uncommitted
/// changes in its worktree, or commits on its branch (or its worktree's
/// `HEAD`) that no remote has. A repo without remotes has nowhere to push,
/// so there it's commits no other branch has.
async fn unsaved_work(
    project: &Path,
    worktree: &Path,
    branch: &str,
) -> Result<Option<String>, ErrorObject> {
    let mut revs = Vec::new();
    if worktree.is_dir() {
        // Every untracked file and submodule change, whatever the repo's
        // config hides from `git status`.
        let status = [
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ];
        match git(worktree, &status).await? {
            Ok(out) if !out.is_empty() => {
                let n = out.lines().count();
                return Ok(Some(format!(
                    "{} has {n} uncommitted change{}",
                    worktree.display(),
                    if n == 1 { "" } else { "s" }
                )));
            }
            Ok(_) => {}
            Err(why) => {
                return Ok(Some(format!(
                    "can't tell whether {} has uncommitted changes: {why}",
                    worktree.display()
                )));
            }
        }
        if let Some(head) = commit(worktree, "HEAD").await? {
            revs.push(head);
        }
    }
    let local = format!("refs/heads/{branch}");
    if commit(project, &local).await?.is_some() {
        revs.push(local);
    }
    let remotes = git(project, &["remote"]).await?.unwrap_or_default();
    let exclude = format!("--exclude={branch}");
    for rev in revs {
        let mut args = vec!["rev-list", "--count", &rev, "--not"];
        if remotes.is_empty() {
            args.extend([exclude.as_str(), "--branches"]);
        } else {
            args.push("--remotes");
        }
        let count = git(project, &args)
            .await?
            .map_err(|why| failed(format!("can't count unpushed commits: {why}")))?;
        let n: u64 = count.trim().parse().unwrap_or(0);
        if n > 0 {
            let what = match (remotes.is_empty(), n) {
                (true, 1) => "1 commit no other branch has".to_owned(),
                (true, n) => format!("{n} commits no other branch has"),
                (false, 1) => "1 commit that isn't pushed".to_owned(),
                (false, n) => format!("{n} commits that aren't pushed"),
            };
            return Ok(Some(format!("{branch} has {what}")));
        }
    }
    Ok(None)
}

/// Removes a worktree and what git keeps about it, and its project's
/// directory of worktrees once that's empty.
async fn remove_worktree(project: &Path, worktree: &Path) -> Result<(), ErrorObject> {
    let path = worktree.to_string_lossy();
    // Twice forced: even with changes, and even if it's locked.
    let _ = git(
        project,
        &["worktree", "remove", "--force", "--force", &path],
    )
    .await?;
    match fs::remove_dir_all(worktree) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            return Err(failed(format!("can't remove {path}: {e}")));
        }
        _ => {}
    }
    let _ = git(project, &["worktree", "prune"]).await?;
    if let Some(parent) = worktree.parent() {
        let _ = fs::remove_dir(parent);
    }
    Ok(())
}

/// Removes `path`, a file or a directory, if it's there.
fn remove(path: &Path) -> io::Result<()> {
    let result = match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) => Err(e),
    };
    match result {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Removes a session's socket and its lock, unless a shim holds the lock.
fn remove_session_files(socket_path: &Path) -> io::Result<()> {
    let lock_path = socket::lock_path(socket_path);
    let lock = match OpenOptions::new().write(true).open(&lock_path) {
        Ok(f) => Some(f),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if let Some(lock) = &lock {
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::other(format!(
                    "a session is still running on {}",
                    socket_path.display()
                )));
            }
            Err(TryLockError::Error(e)) => return Err(e),
        }
    }
    // Removed here, holding the lock: `remove_stale` would take the lock
    // again on a file of its own, and find it held, by us.
    if fs::symlink_metadata(socket_path).is_ok_and(|m| m.file_type().is_socket()) {
        remove(socket_path)?;
    }
    if lock.is_some() {
        remove(&lock_path)?;
    }
    Ok(())
}

/// What a start has done so far, to undo if a later step fails.
#[derive(Default)]
struct Undo {
    project: PathBuf,
    worktree: Option<PathBuf>,
    branch: Option<String>,
    agent_dir: Option<PathBuf>,
    compartment: Option<(Arc<dyn Backend>, Handle)>,
    record: Option<String>,
}

impl Core {
    fn backend(&self, environment: Environment) -> Result<Arc<dyn Backend>, ErrorObject> {
        self.backends.get(environment).ok_or_else(|| {
            error(
                ErrorCode::Conflict,
                format!("this host can't run {environment} agents yet"),
            )
        })
    }

    fn agent_info(&self, a: &AgentRecord, status: &vibe_status::Status) -> Agent {
        Agent {
            host: a.host.clone(),
            agent: a.agent_ref.clone(),
            project: a.project.clone(),
            profile: a.profile.clone(),
            environment: a.environment,
            state: a.state,
            worktree: a.worktree.clone(),
            branch: a.branch.clone(),
            session_id: a.session_id.clone(),
            created_at: rfc3339(a.created_at),
            attach: format!("vibe attach {}", a.agent_ref),
            status: status.to_rpc(),
        }
    }

    pub(crate) fn handle_of(a: &AgentRecord) -> Result<Handle, ErrorObject> {
        serde_json::from_str(&a.compartment)
            .map_err(|e| internal(format!("{}'s compartment can't be read: {e}", a.agent_ref)))
    }

    /// Holds `agent_ref` for a start, delete or resume until dropped, so
    /// two never run on one agent at once.
    fn hold(&self, agent_ref: &str) -> Result<Held<'_>, ErrorObject> {
        if !lock(&self.agents_busy).insert(agent_ref.to_owned()) {
            return Err(error(
                ErrorCode::Conflict,
                format!("{agent_ref} is being started or deleted already"),
            ));
        }
        Ok(Held {
            core: self,
            agent_ref: agent_ref.to_owned(),
        })
    }

    /// The agents whose sessions run, with what they reserve.
    async fn awake(&self, host: &str) -> Result<Vec<Awake>, ErrorObject> {
        let agents = self.store().agents(host).map_err(store_error)?;
        let mut awake = Vec::new();
        for a in agents {
            if self.session_runs(&a.agent_ref).await {
                awake.push(Awake {
                    agent_ref: a.agent_ref,
                    memory: a.memory,
                });
            }
        }
        Ok(awake)
    }

    pub(crate) async fn session_runs(&self, agent_ref: &str) -> bool {
        !matches!(
            sessions::probe(&self.layout.session_socket(agent_ref)).await,
            Probe::Gone
        )
    }

    /// A ref for a new agent: `name`, or three random words, that no agent,
    /// branch, worktree or agent directory has.
    async fn pick_ref(
        &self,
        host: &str,
        project_name: &str,
        project: &Path,
        name: Option<&str>,
    ) -> Result<(String, Held<'_>), ErrorObject> {
        let free = |r: String| async move {
            let taken = match self.store().agent(host, &r) {
                Ok(_) => true,
                Err(StoreError::NotFound(_)) => false,
                Err(e) => return Err(store_error(e)),
            };
            let branch = format!("refs/heads/{r}");
            Ok(!taken
                && commit(project, &branch).await?.is_none()
                && fs::symlink_metadata(self.layout.worktree(project_name, &r)).is_err()
                && fs::symlink_metadata(self.layout.agent_dir(&r)).is_err())
        };
        if let Some(name) = name {
            validate_ref(name).map_err(invalid)?;
            let ok = git(project, &["check-ref-format", "--branch", name])
                .await?
                .is_ok();
            if !ok {
                return Err(invalid(format!("`{name}` can't be a git branch")));
            }
            let held = self.hold(name)?;
            if !free(name.to_owned()).await? {
                return Err(error(
                    ErrorCode::Conflict,
                    format!(
                        "{name} is taken: an agent, branch or worktree has it already; pick \
                         another name, or start `from` that branch"
                    ),
                ));
            }
            return Ok((name.to_owned(), held));
        }
        // Each draw of randomness makes ten candidates, three bytes each.
        for _ in 0..REF_TRIES {
            let bytes = self.random()?;
            for b in bytes.as_chunks::<3>().0 {
                let r = ref_from_bytes(*b);
                let Ok(held) = self.hold(&r) else { continue };
                if free(r.clone()).await? {
                    return Ok((r, held));
                }
            }
        }
        Err(error(
            ErrorCode::Conflict,
            "no free ref found; give the agent a name",
        ))
    }

    // The core handlers.

    /// Starts an agent (PRD: Key user flows, Start an agent).
    pub async fn start_agent(&self, p: AgentStartParams) -> Result<Agent, ErrorObject> {
        let host = self.host(p.host.as_deref())?;
        if p.prompt.trim().is_empty() {
            return Err(invalid("give the agent a prompt"));
        }
        let project = self
            .store()
            .project(host, &p.project)
            .map_err(store_error)?;
        let profile = self
            .store()
            .profile(host, &p.project, &p.profile)
            .map_err(store_error)?;
        let backend = self.backend(profile.environment)?;
        let host_limits =
            limits::load(&self.layout.host_config()).map_err(|e| failed(e.to_string()))?;
        let project_path = PathBuf::from(&project.path);
        if let Some(from) = &p.from {
            check_from(from)?;
        }

        // 1. Admission. Starts admitted earlier and still under way count as
        // awake, and so does this one until its session runs.
        let _reserved = {
            let _one_at_a_time = self.admitting.lock().await;
            let mut awake = self.awake(host).await?;
            awake.extend(lock(&self.starting).values().map(|memory| Awake {
                agent_ref: String::new(),
                memory: *memory,
            }));
            self.admission
                .admit(profile.memory, &host_limits, &awake)
                .map_err(|why| {
                    error(ErrorCode::Conflict, format!("can't start an agent: {why}"))
                })?;
            self.reserve(profile.memory)
        };

        // The worktree, on a new branch.
        let base = {
            let fetching = Arc::clone(
                lock(&self.fetching)
                    .entry(project.name.clone())
                    .or_default(),
            );
            let _one_fetch = fetching.lock().await;
            base_commit(&project_path, p.from.as_deref()).await?
        };
        let (agent_ref, _held) = self
            .pick_ref(host, &project.name, &project_path, p.name.as_deref())
            .await?;
        let mut undo = Undo {
            project: project_path.clone(),
            ..Undo::default()
        };
        let result = self
            .start_steps(
                host,
                &p,
                &profile,
                &backend,
                &host_limits,
                &agent_ref,
                &base,
                &mut undo,
            )
            .await;
        match result {
            Ok(agent) => Ok(agent),
            Err(e) => {
                crate::log!("vibed: starting {agent_ref} failed: {}", e.message);
                self.undo_start(host, undo).await;
                Err(e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_steps(
        &self,
        host: &str,
        p: &AgentStartParams,
        profile: &vibe_core::model::Profile,
        backend: &Arc<dyn Backend>,
        host_limits: &HostLimits,
        agent_ref: &str,
        base: &str,
        undo: &mut Undo,
    ) -> Result<Agent, ErrorObject> {
        let project_path = undo.project.clone();
        let planned = self.layout.worktree(&p.project, agent_ref);
        let parent = planned.parent().unwrap_or(Path::new("/")).to_owned();
        fs::create_dir_all(&parent)
            .map_err(|e| failed(format!("can't create {}: {e}", parent.display())))?;
        // The path Claude will see as its cwd, symlinks resolved.
        let worktree = fs::canonicalize(&parent)
            .map_err(|e| failed(format!("can't find {}: {e}", parent.display())))?
            .join(agent_ref);
        let agent_dir = self.layout.agent_dir(agent_ref);

        // The record first, so that if vibed dies before the start is done,
        // the next one knows what to undo.
        let mut record = AgentRecord {
            host: host.to_owned(),
            agent_ref: agent_ref.to_owned(),
            project: p.project.clone(),
            profile: profile.name.clone(),
            environment: profile.environment,
            model: profile.model,
            reasoning: profile.reasoning,
            mode: profile.mode,
            memory: profile.memory,
            worktree: worktree.to_string_lossy().into_owned(),
            branch: agent_ref.to_owned(),
            session_id: uuid(self.random_16()?),
            shim_socket: Some(
                self.layout
                    .session_socket(agent_ref)
                    .to_string_lossy()
                    .into_owned(),
            ),
            state: AgentState::Busy,
            created_at: self.now(),
            compartment: "{}".into(),
            starting: true,
        };
        self.store().add_agent(&record).map_err(store_error)?;
        undo.record = Some(agent_ref.to_owned());

        let path = worktree.to_string_lossy().into_owned();
        undo.worktree = Some(worktree.clone());
        // git can make the branch and then fail to check it out, so it's
        // undone either way; `pick_ref` found no branch of this name.
        undo.branch = Some(agent_ref.to_owned());
        git(
            &project_path,
            &[
                "worktree",
                "add",
                "--quiet",
                "--no-track",
                "-b",
                agent_ref,
                &path,
                base,
            ],
        )
        .await?
        .map_err(|why| failed(format!("can't make the worktree: {why}")))?;
        let git_dir = git(
            &worktree,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?
        .map_err(|why| failed(format!("can't find the project's .git: {why}")))?;

        undo.agent_dir = Some(agent_dir.clone());
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&agent_dir)
            .map_err(|e| failed(format!("can't create {}: {e}", agent_dir.display())))?;

        // 2. The compartment.
        let limits = Limits::new(profile.memory, host_limits);
        let spec = AgentSpec {
            agent_ref: agent_ref.to_owned(),
            project: p.project.clone(),
            worktree: worktree.clone(),
            git_dir: PathBuf::from(git_dir),
            agent_dir: agent_dir.clone(),
            limits,
        };
        let mut handle = {
            let (b, spec) = (Arc::clone(backend), spec.clone());
            blocking(move || b.create(&spec)).await?
        };
        undo.compartment = Some((Arc::clone(backend), handle.clone()));
        record.compartment = serde_json::to_string(&handle).map_err(internal)?;
        self.store()
            .set_agent_compartment(host, agent_ref, &record.compartment)
            .map_err(store_error)?;

        // 3. The repo's own setup, inside the compartment.
        if worktree.join(SETUP_SCRIPT).is_file() {
            self.run_setup(backend, &handle, &worktree).await?;
        }

        // 4. Claude opens straight to its prompt.
        let claude = handle.claude.clone();
        let (trust, bypass) = (worktree.clone(), profile.mode == Mode::Bypass);
        let claude_lock = Arc::clone(&self.claude_lock);
        blocking(move || {
            let _one_at_a_time = lock(&claude_lock);
            claude
                .prefill(&trust, bypass)
                .map_err(|e| vibe_env::EnvError::new(format!("can't pre-fill Claude's state: {e}")))
        })
        .await?;

        // 5. Claude, on the prompt, held by a shim and to its limits. If a
        // shim started and a later step failed, the undo must end its
        // session, so it gets the handle as the launch left it.
        let launched = self
            .launch(backend, &mut handle, &record, Some(&p.prompt), false)
            .await;
        undo.compartment = Some((Arc::clone(backend), handle.clone()));
        launched?;
        self.store()
            .finish_agent_start(host, agent_ref)
            .map_err(store_error)?;
        crate::log!(
            "vibed: started agent {agent_ref} ({}, {}) in {}",
            p.project,
            profile.name,
            worktree.display()
        );
        let status = self.agent_status(&mut record);
        let agent = self.agent_info(&record, &status);
        let mut e = self.event(EventKind::AgentAdded);
        e.project = Some(p.project.clone());
        e.agent = Some(agent_ref.to_owned());
        e.state = Some(AgentState::Busy.to_string());
        self.publish(e);
        Ok(agent)
    }

    /// Runs the worktree's `.vibe/setup.sh` in the compartment, and keeps
    /// what it said in the agent's directory.
    async fn run_setup(
        &self,
        backend: &Arc<dyn Backend>,
        handle: &Handle,
        worktree: &Path,
    ) -> Result<(), ErrorObject> {
        let run = Run {
            argv: vec!["/bin/sh".into(), SETUP_SCRIPT.into()],
            env: agent_env(),
            cwd: worktree.to_owned(),
            timeout: SETUP_TIMEOUT,
        };
        let out = {
            let (b, h) = (Arc::clone(backend), handle.clone());
            blocking(move || b.run(&h, &run)).await?
        };
        let log = handle.agent_dir.join(SETUP_LOG);
        if let Err(e) = fs::write(&log, &out.output) {
            crate::log!("vibed: can't keep {}: {e}", log.display());
        }
        if out.success() {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&out.output);
        let last = text
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty())
            .map(|l| format!(": {}", l.chars().take(300).collect::<String>()))
            .unwrap_or_default();
        let how = match (out.timed_out, out.code, out.signal) {
            (true, _, _) => format!("didn't finish in {}s", SETUP_TIMEOUT.as_secs()),
            (_, Some(code), _) => format!("exited with code {code}"),
            (_, None, Some(signal)) => format!("was ended by signal {signal}"),
            _ => "failed".into(),
        };
        Err(failed(format!("{SETUP_SCRIPT} {how}{last}")))
    }

    /// Launches Claude in the agent's compartment and starts a shim on its
    /// terminal, then holds it to its limits. Records the session.
    async fn launch(
        &self,
        backend: &Arc<dyn Backend>,
        handle: &mut Handle,
        record: &AgentRecord,
        prompt: Option<&str>,
        resume: bool,
    ) -> Result<(), ErrorObject> {
        let launch = Launch {
            args: claude_args(record, prompt, resume),
            env: agent_env(),
            cwd: PathBuf::from(&record.worktree),
        };
        let end = {
            let (b, h) = (Arc::clone(backend), handle.clone());
            blocking(move || b.launch(&h, &launch)).await?
        };
        self.hold_terminal(backend, handle, record, &end).await
    }

    /// Starts a shim on `end`, records it as the agent's session, and holds
    /// the agent to its limits.
    async fn hold_terminal(
        &self,
        backend: &Arc<dyn Backend>,
        handle: &mut Handle,
        record: &AgentRecord,
        end: &TerminalEnd,
    ) -> Result<(), ErrorObject> {
        let spec = match end {
            TerminalEnd::Pty { command, cwd, env } => SessionSpec {
                agent: record.agent_ref.clone(),
                command: command.iter().map(Into::into).collect(),
                cwd: cwd.clone(),
                env: env.iter().map(|(k, v)| (k.into(), v.into())).collect(),
                size: None,
                vm: None,
            },
            TerminalEnd::Vm { socket } => SessionSpec {
                agent: record.agent_ref.clone(),
                command: Vec::new(),
                cwd: PathBuf::from(&record.worktree),
                env: Vec::new(),
                size: None,
                vm: Some(socket.clone()),
            },
        };
        let pid = sessions::spawn_shim(&self.shim, &self.layout, &spec)
            .await
            .map_err(failed)?;
        handle.session = Some(pid);
        let compartment = serde_json::to_string(&*handle).map_err(internal)?;
        self.store()
            .set_agent_compartment(&record.host, &record.agent_ref, &compartment)
            .map_err(store_error)?;
        let host_limits =
            limits::load(&self.layout.host_config()).map_err(|e| failed(e.to_string()))?;
        let limits = Limits::new(record.memory, &host_limits);
        let (b, h) = (Arc::clone(backend), handle.clone());
        blocking(move || b.apply(&h, &limits)).await?;
        crate::log!(
            "vibed: {}'s session started (shim pid {pid})",
            record.agent_ref
        );
        Ok(())
    }

    /// Undoes what a failed start did, newest first. Nothing in it may
    /// fail the start further; what can't be undone is logged.
    async fn undo_start(&self, host: &str, undo: Undo) {
        let what = undo.branch.clone().unwrap_or_default();
        if let Some((backend, handle)) = undo.compartment {
            let claude_lock = Arc::clone(&self.claude_lock);
            let r = blocking(move || {
                let _one_at_a_time = lock(&claude_lock);
                backend.delete(&handle)
            })
            .await;
            if let Err(e) = r {
                crate::log!("vibed: undoing {what}: can't delete its compartment: {e}");
            }
        }
        if let Some(r) = &undo.record {
            self.forget_status(r);
            let _ = self.store().remove_agent(host, r);
            let _ = remove_session_files(&self.layout.session_socket(r));
        }
        if let Some(dir) = &undo.agent_dir
            && let Err(e) = remove(dir)
        {
            crate::log!("vibed: undoing {what}: can't remove {}: {e}", dir.display());
        }
        if let Some(worktree) = &undo.worktree
            && let Err(e) = remove_worktree(&undo.project, worktree).await
        {
            crate::log!("vibed: undoing {what}: {}", e.message);
        }
        if let Some(branch) = &undo.branch {
            let _ = git(&undo.project, &["branch", "-D", branch]).await;
        }
    }

    /// A host's agents, oldest first, each with what its transcript says
    /// and in the state that and its session give: one whose session has
    /// gone is `exited`.
    pub async fn agents(&self, p: HostParams) -> Result<AgentList, ErrorObject> {
        let host = self.host(p.host.as_deref())?;
        let records = self.store().agents(host).map_err(store_error)?;
        let mut agents = Vec::new();
        for mut a in records {
            let status = self.agent_status(&mut a);
            // One being started, deleted or resumed is left as it's
            // recorded until that's done.
            if lock(&self.agents_busy).contains(&a.agent_ref) {
                agents.push(self.agent_info(&a, &status));
                continue;
            }
            let state = self.observed_state(&a, &status).await;
            if state != a.state {
                self.set_state(&mut a, state);
            }
            agents.push(self.agent_info(&a, &status));
        }
        Ok(AgentList { agents })
    }

    pub(crate) fn set_state(&self, a: &mut AgentRecord, state: AgentState) {
        match self.store().set_agent_state(&a.host, &a.agent_ref, state) {
            Ok(true) => {
                a.state = state;
                let mut e = self.event(EventKind::AgentState);
                e.project = Some(a.project.clone());
                e.agent = Some(a.agent_ref.clone());
                e.state = Some(state.to_string());
                self.publish(e);
            }
            Ok(false) => a.state = state,
            Err(e) => crate::log!("vibed: can't record {}'s state: {e}", a.agent_ref),
        }
    }

    /// Brings an agent whose processes are gone back: `claude --resume`,
    /// in its compartment, on a new shim (PRD: Failures). One that runs is
    /// left as it is. `vibe_send` (#12) and wakes (#6) come through here.
    pub async fn resume_agent(
        &self,
        host: Option<&str>,
        agent_ref: &str,
    ) -> Result<Agent, ErrorObject> {
        let host = self.host(host)?;
        let _held = self.hold(agent_ref)?;
        let mut record = self.store().agent(host, agent_ref).map_err(store_error)?;
        if self.session_runs(agent_ref).await {
            let status = self.agent_status(&mut record);
            return Ok(self.agent_info(&record, &status));
        }
        // The session it's in now, after a `/clear`, is the one to resume.
        self.agent_status(&mut record);
        let backend = self.backend(record.environment)?;
        let mut handle = Core::handle_of(&record)?;
        // What's left of its tree, if anything, ends: it has no terminal.
        if handle.session.is_some() {
            let (b, h) = (Arc::clone(&backend), handle.clone());
            match blocking(move || b.wake(&h)).await? {
                Woke::Resume => {}
                Woke::Exact | Woke::Reattach(_) => {
                    let (b, h) = (Arc::clone(&backend), handle.clone());
                    blocking(move || b.stop(&h)).await?;
                }
            }
        }
        let _ = remove_session_files(&self.layout.session_socket(agent_ref));
        let worktree = PathBuf::from(&record.worktree);
        // A session that never wrote a transcript has nothing to resume, so
        // it starts again under the same id.
        let resume = handle
            .claude
            .transcript(&worktree, &record.session_id)
            .is_file();
        self.launch(&backend, &mut handle, &record, None, resume)
            .await?;
        crate::log!(
            "vibed: {agent_ref} {}",
            if resume {
                "resumed its session"
            } else {
                "had no transcript, so it started again"
            }
        );
        let status = self.agent_status(&mut record);
        self.set_state(&mut record, status.state());
        Ok(self.agent_info(&record, &status))
    }

    /// Deletes an agent and everything VibeOS made for it but its branch
    /// (PRD: Key user flows, Delete).
    pub async fn delete_agent(&self, p: AgentDeleteParams) -> Result<Agent, ErrorObject> {
        let host = self.host(p.host.as_deref())?;
        validate_ref(&p.agent).map_err(invalid)?;
        let _held = self.hold(&p.agent)?;
        let mut record = self.store().agent(host, &p.agent).map_err(store_error)?;
        // What it was doing, as its transcript is about to go.
        let status = self.agent_status(&mut record);
        let project = self
            .store()
            .project(host, &record.project)
            .map_err(store_error)?;
        let force = p.force == Some(true);
        // Without its backend, or a handle it can read, only a forced
        // delete goes ahead: it ends the session its shim leads, and removes
        // the rest of what VibeOS made, but can't reach the compartment.
        let compartment = match (self.backend(record.environment), Core::handle_of(&record)) {
            (Ok(b), Ok(h)) => Some((b, h)),
            (Err(e), _) | (_, Err(e)) if !force => return Err(e),
            (_, _) => None,
        };
        let (project_path, worktree) = (
            PathBuf::from(&project.path),
            PathBuf::from(&record.worktree),
        );
        let refuse = |why: String| {
            error(
                ErrorCode::Conflict,
                format!(
                    "{} has unsaved work: {why}. Commit and push it, or delete with force",
                    p.agent
                ),
            )
        };
        if !force && let Some(why) = unsaved_work(&project_path, &worktree, &record.branch).await? {
            return Err(refuse(why));
        }
        match &compartment {
            Some((backend, handle)) => {
                let (b, h) = (Arc::clone(backend), handle.clone());
                blocking(move || b.stop(&h)).await?;
            }
            None => {
                let socket = self.layout.session_socket(&p.agent);
                if let Probe::Running(status) = sessions::probe(&socket).await {
                    let tree = vibe_env::tree::SessionTree::new(status.pid);
                    tokio::task::spawn_blocking(move || tree.end(Duration::from_secs(5)))
                        .await
                        .map_err(internal)?;
                }
            }
        }
        // It may have saved something between the check and the stop.
        if !force && let Some(why) = unsaved_work(&project_path, &worktree, &record.branch).await? {
            let mut stopped = record.clone();
            self.set_state(&mut stopped, AgentState::Exited);
            return Err(refuse(format!(
                "{why} (it was stopped to be deleted; a send resumes it)"
            )));
        }
        if let Some((backend, handle)) = compartment {
            let claude_lock = Arc::clone(&self.claude_lock);
            blocking(move || {
                let _one_at_a_time = lock(&claude_lock);
                backend.delete(&handle)
            })
            .await?;
        }
        remove_worktree(&project_path, &worktree).await?;
        remove_session_files(&self.layout.session_socket(&p.agent))
            .map_err(|e| failed(format!("can't remove {}'s socket: {e}", p.agent)))?;
        let agent_dir = self.layout.agent_dir(&p.agent);
        remove(&agent_dir)
            .map_err(|e| failed(format!("can't remove {}: {e}", agent_dir.display())))?;
        self.store()
            .remove_agent(host, &p.agent)
            .map_err(store_error)?;
        self.forget_status(&p.agent);
        crate::log!(
            "vibed: deleted agent {}{}; its branch {} is kept",
            p.agent,
            if force { ", forced" } else { "" },
            record.branch
        );
        let mut e = self.event(EventKind::AgentDeleted);
        e.project = Some(record.project.clone());
        e.agent = Some(p.agent.clone());
        self.publish(e);
        let mut gone = record;
        gone.state = AgentState::Exited;
        Ok(self.agent_info(&gone, &status))
    }

    /// Undoes every start that a crash or a kill of `vibed` cut short, as
    /// a failed start is undone (PRD: Failures). Such an agent's start never
    /// answered, so it was never anyone's to keep. Called when `vibed`
    /// starts, before anything else can start one.
    pub async fn undo_cut_short_starts(&self) -> usize {
        let Ok(agents) = self.store().agents(LOCAL_HOST) else {
            return 0;
        };
        let mut undone = 0;
        for a in agents.into_iter().filter(|a| a.starting) {
            let Ok(_held) = self.hold(&a.agent_ref) else {
                continue;
            };
            let Ok(project) = self.store().project(LOCAL_HOST, &a.project) else {
                continue;
            };
            let compartment = match (self.backend(a.environment), Core::handle_of(&a)) {
                (Ok(b), Ok(h)) => Some((b, h)),
                _ => None,
            };
            // A shim it started, if any, ends with it, handle or not.
            if let Probe::Running(status) =
                sessions::probe(&self.layout.session_socket(&a.agent_ref)).await
            {
                let tree = vibe_env::tree::SessionTree::new(status.pid);
                let _ = tokio::task::spawn_blocking(move || tree.end(Duration::from_secs(5))).await;
            }
            crate::log!("vibed: {}'s start was cut short; undoing it", a.agent_ref);
            let worktree = PathBuf::from(&a.worktree);
            let undo = Undo {
                project: PathBuf::from(&project.path),
                worktree: worktree.exists().then_some(worktree),
                branch: Some(a.branch.clone()),
                agent_dir: Some(self.layout.agent_dir(&a.agent_ref)),
                compartment,
                record: Some(a.agent_ref.clone()),
            };
            self.undo_start(LOCAL_HOST, undo).await;
            undone += 1;
        }
        undone
    }

    /// Holds every agent whose session runs to its limits again, as a
    /// restarted `vibed` must: a supervisor's hold doesn't outlive the
    /// process that had it. Called once sessions are adopted.
    pub async fn hold_agents_to_limits(&self) {
        let Ok(agents) = self.store().agents(LOCAL_HOST) else {
            return;
        };
        let Ok(host_limits) = limits::load(&self.layout.host_config()) else {
            crate::log!("vibed: host.toml can't be read, so agents' limits aren't held");
            return;
        };
        for a in agents {
            if !self.session_runs(&a.agent_ref).await {
                continue;
            }
            let (Ok(backend), Ok(handle)) = (self.backend(a.environment), Core::handle_of(&a))
            else {
                continue;
            };
            let limits = Limits::new(a.memory, &host_limits);
            if let Err(e) = blocking(move || backend.apply(&handle, &limits)).await {
                crate::log!(
                    "vibed: can't hold {} to its limits: {}",
                    a.agent_ref,
                    e.message
                );
            }
        }
    }

    /// Runs `git maintenance` on a project none of whose agents is awake,
    /// as agents never collect garbage themselves.
    pub async fn maintain_if_quiet(&self, host: &str, project_name: &str, project: &Path) {
        let Ok(agents) = self.store().agents(host) else {
            return;
        };
        // Not while a start is under way, nor while any of its agents is
        // awake or being started, deleted or resumed.
        if !lock(&self.starting).is_empty() {
            return;
        }
        for a in agents.iter().filter(|a| a.project == project_name) {
            if lock(&self.agents_busy).contains(&a.agent_ref)
                || self.session_runs(&a.agent_ref).await
            {
                return;
            }
        }
        // In the foreground: this is a background task already, and a
        // detached gc would outlive it unseen.
        let maintain = [
            "-c",
            "gc.autoDetach=false",
            "-c",
            "maintenance.autoDetach=false",
            "maintenance",
            "run",
            "--auto",
            "--quiet",
        ];
        match git_within(project, &maintain, FETCH_TIMEOUT).await {
            Ok(Ok(_)) => {}
            Ok(Err(why)) => crate::log!("vibed: git maintenance on {project_name}: {why}"),
            Err(e) => crate::log!("vibed: git maintenance on {project_name}: {}", e.message),
        }
    }

    /// Runs [`Core::maintain_if_quiet`] on every project now and then.
    pub fn spawn_maintenance(self: &Arc<Self>, every: Duration) -> tokio::task::JoinHandle<()> {
        let core = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(every);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Not at startup, when everything is busy coming back.
            ticks.tick().await;
            loop {
                ticks.tick().await;
                let projects = core.store().projects(LOCAL_HOST).unwrap_or_default();
                for p in projects {
                    core.maintain_if_quiet(LOCAL_HOST, &p.name, Path::new(&p.path))
                        .await;
                }
            }
        })
    }
}

/// Memory reserved for a start until it's dropped.
pub(crate) struct Reserved<'a> {
    core: &'a Core,
    token: u64,
}

impl Drop for Reserved<'_> {
    fn drop(&mut self) {
        lock(&self.core.starting).remove(&self.token);
    }
}

impl Core {
    fn reserve(&self, memory: u64) -> Reserved<'_> {
        let mut starting = lock(&self.starting);
        let token = (0..).find(|t| !starting.contains_key(t)).unwrap_or(0);
        starting.insert(token, memory);
        Reserved { core: self, token }
    }
}

/// An agent held by a start, delete or resume.
pub(crate) struct Held<'a> {
    core: &'a Core,
    agent_ref: String,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        lock(&self.core.agents_busy).remove(&self.agent_ref);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vibe_core::model::{Model, Reasoning};

    fn record(mode: Mode) -> AgentRecord {
        AgentRecord {
            host: LOCAL_HOST.into(),
            agent_ref: "quiet-amber-otter".into(),
            project: "web".into(),
            profile: "p".into(),
            environment: Environment::Linux,
            model: Model::Sonnet,
            reasoning: Reasoning::Xhigh,
            mode,
            memory: 4 * GIB,
            worktree: "/w".into(),
            branch: "quiet-amber-otter".into(),
            session_id: "0b5c3f6e-0000-4000-8000-000000000000".into(),
            shim_socket: None,
            state: AgentState::Busy,
            created_at: 0,
            compartment: "{}".into(),
            starting: false,
        }
    }

    #[test]
    fn claude_gets_the_profile_and_the_prompt_as_an_argument() {
        assert_eq!(
            claude_args(&record(Mode::Auto), Some("-fix the build"), false),
            [
                "--session-id",
                "0b5c3f6e-0000-4000-8000-000000000000",
                "--model",
                "sonnet",
                "--effort",
                "xhigh",
                "--permission-mode",
                "auto",
                "--",
                "-fix the build"
            ]
        );
        assert_eq!(
            claude_args(&record(Mode::Bypass), None, true),
            [
                "--resume",
                "0b5c3f6e-0000-4000-8000-000000000000",
                "--model",
                "sonnet",
                "--effort",
                "xhigh",
                "--dangerously-skip-permissions"
            ]
        );
    }

    #[test]
    fn agents_get_git_settings_without_touching_the_repo() {
        let env: HashMap<_, _> = agent_env().into_iter().collect();
        assert_eq!(env["GIT_CONFIG_COUNT"], "3");
        let pairs: Vec<_> = (0..3)
            .map(|i| {
                (
                    env[&format!("GIT_CONFIG_KEY_{i}")].as_str(),
                    env[&format!("GIT_CONFIG_VALUE_{i}")].as_str(),
                )
            })
            .collect();
        assert_eq!(pairs, AGENT_GIT_CONFIG);
    }

    #[test]
    fn uuids_are_version_4() {
        let id = uuid([0xff; 16]);
        assert_eq!(id, "ffffffff-ffff-4fff-bfff-ffffffffffff");
        assert_eq!(uuid([0; 16]), "00000000-0000-4000-8000-000000000000");
    }

    #[test]
    fn froms_are_never_options() {
        for ok in ["main", "feature/x", "v1.2", "0b5c3f6"] {
            assert!(check_from(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-x", "--upload-pack=evil", "a b", "a\nb"] {
            assert!(check_from(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn admission_by_memory_and_count() {
        let a = HostAdmission {
            total_memory: 32 * GIB,
        };
        let limits = HostLimits::default();
        let awake = |n: usize| -> Vec<Awake> {
            (0..n)
                .map(|i| Awake {
                    agent_ref: format!("a{i}"),
                    memory: 4 * GIB,
                })
                .collect()
        };
        // 32G less 4G kept: seven 4G agents fit, as the PRD says.
        assert_eq!(a.admit(4 * GIB, &limits, &awake(6)), Ok(()));
        let err = a.admit(4 * GIB, &limits, &awake(7)).unwrap_err();
        assert_eq!(
            err,
            "there's no room for an agent with 4G of memory: this host has 32G, keeps 4G for \
             itself (reserve_memory), and its awake agents hold 28G"
        );
        let few = HostLimits {
            max_awake_agents: 2,
            ..limits
        };
        let err = a.admit(GIB, &few, &awake(2)).unwrap_err();
        assert!(err.contains("max_awake_agents"), "{err}");
        assert_eq!(size(1536 * MIB), "1.5G");
        assert_eq!(size(512 * MIB), "512M");
    }
}
