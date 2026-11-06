//! Agents: one Claude Code or Pi in its own worktree and compartment, made and
//! deleted as a unit (PRD: Core concepts; Key user flows, Start an agent and
//! Delete). One ref names all three, and the branch too.
//!
//! A start, in order: a check that the host has disk to spare and room
//! under `max_awake_agents` (never memory: an agent's memory is a ceiling,
//! not a reservation, #47); a fetch of `origin` and a worktree on a new
//! branch; the compartment, through the environment's backend; the
//! worktree's `.vibe/setup.sh`, run inside it; its state pre-filled; then the
//! agent on the prompt, in a session of its own, held to its limits.
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
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use vibe_core::limits::{self, HostLimits};
use vibe_core::model::{
    AgentKind, AgentState, Environment, EventKind, LOCAL_HOST, Mode, PiTarget, ref_from_bytes,
    validate_new_ref, validate_ref,
};
use vibe_core::rpc::{
    Agent, AgentDeleteParams, AgentList, AgentParams, AgentStartParams, ErrorCode, ErrorObject,
    HostParams,
};
use vibe_core::socket;
use vibe_core::store::{AgentRecord, StoreError};
use vibe_core::time::rfc3339;
use vibe_core::units::{GIB, MIB};
use vibe_env::{AgentSpec, Backend, Handle, Launch, Limits, Run, TerminalEnd, Woke};
use vibe_relay::local;

use crate::core::{Core, error, invalid, lock, store_error};
use crate::projects::{GIT_CONFIG_FROM_ENV, git, git_within, git_without};
use crate::services::injection;
use crate::sessions::{self, Probe, SessionSpec};
use crate::sleep::Early;

/// How long `.vibe/setup.sh` may run before the start gives up on it.
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How long a send or an attach waits for a sleep, a wake or a delete
/// under way on its agent: a VM's save or restore takes seconds.
pub const BUSY_PATIENCE: Duration = Duration::from_secs(60);

/// How long a fetch of `origin` may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Draws of randomness for a free ref before giving up.
const REF_TRIES: usize = 8;

/// What Interrupt presses: Escape, which stops the turn in Claude and in Pi.
const ESCAPE: &[u8] = b"\x1b";

/// The script a repo can have run in every new agent's compartment.
pub const SETUP_SCRIPT: &str = ".vibe/setup.sh";

/// Where its output is kept, in the agent's directory.
pub const SETUP_LOG: &str = "setup.log";

/// In an agent's directory, the prompt it started on, for as long as it has no
/// transcript: dead before then, it starts again on it, and doesn't come back idle
/// with what it was asked gone. Its transcript has it from there on, and a transcript
/// lost later must not set an old task going again, so it goes as soon as there's one
/// ([`Core::with_tail`]).
pub(crate) const START_PROMPT: &str = "start-prompt";

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

/// Below this much free disk, no agent starts (PRD: Failures, Low disk).
pub const MIN_FREE_DISK: u64 = 10 * GIB;

/// Why another agent can't be awake beside `awake` others, if it can't:
/// `max_awake_agents` are awake. Memory is never a reason (#47): each
/// agent's memory is a ceiling it's held to, not a reservation, and the
/// host swaps if its agents use more than it has.
pub fn too_many_awake(awake: usize, limits: &HostLimits) -> Option<String> {
    (awake >= limits.max_awake_agents as usize).then(|| {
        format!(
            "{awake} agent{} awake already, the most this host takes (max_awake_agents)",
            if awake == 1 { " is" } else { "s are" }
        )
    })
}

/// A size for people: `4G`, `1.5G`, `512M`.
pub(crate) fn size(bytes: u64) -> String {
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
pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> vibe_env::Result<T> + Send + 'static,
) -> Result<T, ErrorObject> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(internal)?
        .map_err(|e| failed(e.to_string()))
}

/// Claude's arguments for an agent: a new session on `prompt`, or with
/// `resume`, the session it had.
///
/// `offered` is how the shared services are offered it, which goes ahead of the
/// flags that follow, as `--mcp-config` takes as many values as it can find.
pub fn claude_args(
    a: &AgentRecord,
    prompt: Option<&str>,
    resume: bool,
    offered: &[String],
) -> Vec<String> {
    let mut args = vec![
        if resume { "--resume" } else { "--session-id" }.to_owned(),
        a.session_id.clone(),
    ];
    args.extend_from_slice(offered);
    args.extend([
        "--model".into(),
        a.model.to_string(),
        "--effort".into(),
        a.reasoning.to_string(),
    ]);
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

/// An agent's arguments, whichever it runs: a new session on `prompt`, or with
/// `resume`, the session it had. Pi's one `--session-id` does both.
pub fn agent_args(
    a: &AgentRecord,
    prompt: Option<&str>,
    resume: bool,
    offered: &[String],
) -> Vec<String> {
    match a.agent {
        AgentKind::Claude => claude_args(a, prompt, resume, offered),
        AgentKind::Pi => vibe_env::pi::args(
            &a.session_id,
            a.pi_model.as_deref(),
            a.reasoning.as_str(),
            prompt,
        ),
    }
}

/// Keeps `prompt` to start `a` on again, if it dies with no transcript. What Pi does
/// itself writes no transcript at all, so a `!` command would run at every restart for
/// as long as nobody asked it anything: that's not kept. Not being able to keep it costs
/// no more than that: it's said in the log, and the agent starts.
fn keep_start_prompt(a: &AgentRecord, handle: &Handle, prompt: &str) {
    if a.agent == AgentKind::Pi && crate::talk::pi_takes_itself(prompt) {
        return;
    }
    let path = handle.agent_dir.join(START_PROMPT);
    // Whole or not at all, as the host going down is what this is kept for, and part
    // of a prompt would be a different task.
    if let Err(e) = write_private(&path, prompt.as_bytes()) {
        crate::log!("vibed: can't keep {}: {e}", path.display());
    }
}

/// Writes a file only this user can read, whole or not at all.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("new");
    let mut f = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(&tmp, path)
}

/// The prompt to start `handle`'s agent on again, if there's one kept.
fn kept_start_prompt(handle: &Handle) -> Option<String> {
    let path = handle.agent_dir.join(START_PROMPT);
    match fs::read_to_string(&path) {
        Ok(prompt) => Some(prompt),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            crate::log!("vibed: can't read {}: {e}", path.display());
            None
        }
    }
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
/// A socket with no lock beside it was never a shim's, so it's kept: on a
/// Linux host, `run/mac.sock` with no lock is sshd's, for an older Mac's `ssh -R`,
/// even for an agent named `mac` (issue #45).
pub(crate) fn remove_session_files(socket_path: &Path) -> io::Result<()> {
    let lock_path = socket::lock_path(socket_path);
    let lock = match OpenOptions::new().write(true).open(&lock_path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
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
    // Removed here, holding the lock: `remove_stale` would take the lock
    // again on a file of its own, and find it held, by us.
    if fs::symlink_metadata(socket_path).is_ok_and(|m| m.file_type().is_socket()) {
        remove(socket_path)?;
    }
    remove(&lock_path)
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
    pub(crate) fn backend(
        &self,
        environment: Environment,
    ) -> Result<Arc<dyn Backend>, ErrorObject> {
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
            kind: a.agent,
            environment: a.environment,
            state: a.state,
            worktree: a.worktree.clone(),
            branch: a.branch.clone(),
            session_id: a.session_id.clone(),
            created_at: rfc3339(a.created_at),
            attach: format!("vibe attach {}", a.agent_ref),
            memory: a.memory,
            memory_used: None,
            status: status.to_rpc(),
        }
    }

    /// [`Core::agent_info`] with the memory it uses now, for an agent in
    /// `state` whose session may run. Its backend measures it; one that
    /// can't is left without.
    async fn live_info(&self, a: &AgentRecord, status: &vibe_status::Status) -> Agent {
        let mut info = self.agent_info(a, status);
        if matches!(a.state, AgentState::Exited | AgentState::Asleep) {
            return info;
        }
        if let (Ok(backend), Ok(handle)) = (self.backend(a.environment), Core::handle_of(a)) {
            info.memory_used = blocking(move || backend.usage(&handle))
                .await
                .ok()
                .map(|u| u.memory);
        }
        info
    }

    pub(crate) fn handle_of(a: &AgentRecord) -> Result<Handle, ErrorObject> {
        serde_json::from_str(&a.compartment)
            .map_err(|e| internal(format!("{}'s compartment can't be read: {e}", a.agent_ref)))
    }

    /// Waits, up to [`BUSY_PATIENCE`], while `agent_ref` is being put to
    /// sleep, woken or deleted, so a send or an attach that comes meanwhile
    /// finds it as that leaves it. Says whether it's free now.
    pub(crate) async fn wait_not_busy(&self, agent_ref: &str) -> bool {
        let deadline = std::time::Instant::now() + BUSY_PATIENCE;
        while self.is_busy(agent_ref) {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        true
    }

    /// Holds `agent_ref` for a start, delete or resume until dropped, so
    /// two never run on one agent at once.
    pub(crate) fn hold(&self, agent_ref: &str) -> Result<Held<'_>, ErrorObject> {
        if !lock(&self.agents_busy).insert(agent_ref.to_owned()) {
            return Err(error(
                ErrorCode::Conflict,
                format!("{agent_ref} is busy being started, woken, put to sleep or deleted"),
            ));
        }
        Ok(Held {
            core: self,
            agent_ref: agent_ref.to_owned(),
        })
    }

    /// The agents awake, by ref, but `except`: those whose sessions run
    /// and aren't asleep (frozen), and the toolbox while it's open.
    async fn awake(&self, host: &str, except: &str) -> Result<Vec<String>, ErrorObject> {
        let agents = self.store().agents(host).map_err(store_error)?;
        let mut awake = Vec::new();
        let toolbox = *lock(&self.toolbox_state);
        if !matches!(
            toolbox,
            vibe_core::model::ToolboxState::Closed | vibe_core::model::ToolboxState::Unavailable
        ) && !self
            .toolbox_moving
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            awake.push(vibe_env::toolbox::TOOLBOX_REF.to_owned());
        }
        for a in agents {
            if a.agent_ref != except
                && a.state != AgentState::Asleep
                && self.session_runs(&a.agent_ref).await
            {
                awake.push(a.agent_ref);
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
            validate_new_ref(name).map_err(invalid)?;
            if name == vibe_env::toolbox::TOOLBOX_REF {
                return Err(invalid(format!(
                    "`{name}` is the Linux toolbox's; pick another name"
                )));
            }
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
        // Pi's there first, installed now if it isn't. It can take a while, so this
        // comes before anything's held: a removal, or room among the agents awake.
        // What's wrong with the profile is said below.
        let wanted = self.store().profile(host, &p.project, &p.profile);
        if let Ok(profile) = wanted
            && profile.agent == AgentKind::Pi
        {
            let backend = self.backend(profile.environment)?;
            self.pi_ready(&backend).await?;
        }
        // The project stays until the start is done: a removal waits for
        // it, and a start waits for a removal under way, then finds no
        // project.
        let project_use = self.project_use(&p.project);
        let _in_use = project_use.read().await;
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

        // 1. Room for it: disk to spare, and under `max_awake_agents`.
        // Never memory, nor memory pressure (#47): its profile's memory is
        // the most it may use, not what it holds, and pressure is only
        // warned of (`hosts.list`).
        let disk = self.layout.worktrees_dir();
        if let Some(free) = (self.free_disk)(&disk)
            && free < MIN_FREE_DISK
        {
            return Err(error(
                ErrorCode::Conflict,
                format!(
                    "can't start an agent: the disk has only {} free, and new agents need {} \
                     free. Delete agents you're done with, or free up some space",
                    size(free),
                    size(MIN_FREE_DISK)
                ),
            ));
        }
        // Past `max_awake_agents`, an agent still inside its idle debounce
        // sleeps early, idle longest first; then it's refused. Starts
        // admitted earlier and still under way count as awake, and so does
        // this one until its session runs. Starts never queue (PRD: Host
        // limits).
        let _admitted = self.admit(host, "", "start an agent").await?;

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
        // Its prompt is what a wait waits on first, not a send to an agent
        // that had its name before.
        lock(&self.sends).remove(&agent_ref);
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
            shared_services: profile.shared_services,
            agent: profile.agent,
            pi_model: profile.pi_model.clone(),
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
            agent: profile.agent,
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

        // Pi with nothing to answer with would only sit there: said now,
        // before the repo's setup. What this can't tell, the screen says later
        // (`vibe_status::pi_error_on_screen`).
        if let vibe_env::AgentHome::Pi(home) = &handle.home {
            self.check_pi_can_answer(home, backend, &handle, &record)
                .await?;
        }

        // 3. The repo's own setup, inside the compartment.
        if worktree.join(SETUP_SCRIPT).is_file() {
            self.run_setup(backend, &handle, &worktree).await?;
        }

        // 4. It opens straight to its prompt.
        let home = handle.home.clone();
        let (trust, bypass) = (worktree.clone(), profile.mode == Mode::Bypass);
        let claude_lock = Arc::clone(&self.claude_lock);
        blocking(move || {
            let _one_at_a_time = lock(&claude_lock);
            home.prefill(&trust, bypass)
                .map_err(|e| vibe_env::EnvError::new(format!("can't pre-fill its state: {e}")))
        })
        .await?;

        // 5. The agent, on the prompt, held by a shim and to its limits. If a
        // shim started and a later step failed, the undo must end its
        // session, so it gets the handle as the launch left it.
        keep_start_prompt(&record, &handle, &p.prompt);
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
        let status = self.agent_status(&mut record, self.idle_debounce()).await;
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

    /// Refuses a Pi agent that couldn't answer, and says how to sign in: VibeOS
    /// holds no key and picks no provider (PRD: Who gets which credentials).
    /// With a model named, Pi says itself, where it can be run; otherwise, and
    /// in a VM, it's what can be told from its files.
    async fn check_pi_can_answer(
        &self,
        home: &vibe_env::PiHome,
        backend: &Arc<dyn Backend>,
        handle: &Handle,
        record: &AgentRecord,
    ) -> Result<(), ErrorObject> {
        let env_file = vibe_env::pi::read_env_file(&self.layout.pi_env_file()).map_err(failed)?;
        let model = record.pi_model.as_deref();
        let mut said = None;
        if let Some(model) = model {
            let env = self.launch_env(record)?;
            let (b, h, m) = (Arc::clone(backend), handle.clone(), model.to_owned());
            said = blocking(move || Ok(b.pi_auth_check(&h, &env, &m))).await?;
        }
        let missing = match said {
            Some(vibe_env::pi::AuthCheck::Ready) => None,
            Some(vibe_env::pi::AuthCheck::NotReady(why)) => Some(why),
            None => {
                let (home, model) = (home.clone(), model.map(str::to_owned));
                blocking(move || Ok(home.login_missing(model.as_deref(), &env_file))).await?
            }
        };
        let Some(missing) = missing else {
            return Ok(());
        };
        // The app offers that sign-in from what's here (`PiSignInOffer`). Whoever
        // else reads this has the words.
        let vm = backend.capabilities().file_containment;
        let provider = model
            .and_then(|m| m.split_once('/'))
            .map(|(provider, _)| provider);
        let at_a_terminal = if vm {
            "in `vibe toolbox`, `pi` and /login"
        } else {
            "`pi` and /login"
        };
        let who = provider.unwrap_or("one");
        let how = format!(
            "Sign in to {who} in the VibeOS app, in Settings → Agents (at a terminal, \
             {at_a_terminal})"
        );
        let shared = if vm {
            "Every linux Pi agent gets that one login"
        } else {
            "Every Pi agent here shares that one login"
        };
        let mut refused = error(
            ErrorCode::Conflict,
            format!("{missing}, so it couldn't answer. {how}. {shared}"),
        );
        refused.data = Some(serde_json::json!({
            "pi_sign_in": {
                "provider": provider,
                "target": if vm { PiTarget::Vm } else { PiTarget::Native },
            }
        }));
        Err(refused)
    }

    /// What a Pi agent gets in its environment on top of the compartment's:
    /// git's settings, and what the owner set on purpose in `pi.env`. Claude
    /// gets git's settings alone.
    fn launch_env(&self, record: &AgentRecord) -> Result<Vec<(String, String)>, ErrorObject> {
        let mut env = agent_env();
        if record.agent == AgentKind::Pi {
            env.extend(vibe_env::pi::read_env_file(&self.layout.pi_env_file()).map_err(failed)?);
        }
        Ok(env)
    }

    /// Launches its coding agent in the agent's compartment and starts a shim
    /// on its terminal, then holds it to its limits. Records the session.
    async fn launch(
        &self,
        backend: &Arc<dyn Backend>,
        handle: &mut Handle,
        record: &AgentRecord,
        prompt: Option<&str>,
        resume: bool,
    ) -> Result<(), ErrorObject> {
        // A resume may find it's gone since.
        if record.agent == AgentKind::Pi {
            self.pi_ready(backend).await?;
        }
        let mut services = self.shared_services(backend, record);
        if services.is_some() {
            let (b, h) = (Arc::clone(backend), handle.clone());
            // They're optional, so not being able to tell costs them and no more:
            // if the compartment's really in trouble, the launch below says so.
            let why = match blocking(move || b.services_unsupported(&h)).await {
                Ok(why) => why,
                Err(e) => Some(format!("can't tell if its guest has them: {}", e.message)),
            };
            if let Some(why) = why {
                crate::log!("vibed: {} gets no shared services: {why}", record.agent_ref);
                services = None;
            }
        }
        let auto_allowed = self.services_auto_allowed();
        let offered = injection(record.agent, services.as_deref(), &auto_allowed);
        // VibeOS's own last, so a line of `pi.env` can't turn them on, off or
        // somewhere else.
        let mut env = self.launch_env(record)?;
        env.push((local::AGENT_ENV.to_owned(), record.agent_ref.clone()));
        if services.is_some() {
            env.extend(self.services_socket_env(backend));
        }
        env.extend(offered.env);
        let launch = Launch {
            agent: record.agent,
            args: agent_args(record, prompt, resume, &offered.args),
            env,
            cwd: PathBuf::from(&record.worktree),
        };
        // A Pi with nothing to do reads as at rest from here until it writes
        // again, whatever its transcript left open.
        if record.agent == AgentKind::Pi {
            crate::status::note_pi_at_rest(handle, prompt.is_none());
        }
        let end = {
            let (b, h) = (Arc::clone(backend), handle.clone());
            blocking(move || b.launch(&h, &launch)).await?
        };
        self.pi_sync_copy(record, handle).await;
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
            TerminalEnd::Pty {
                command,
                cwd,
                env,
                clean_env,
            } => SessionSpec {
                agent: record.agent_ref.clone(),
                command: command.iter().map(Into::into).collect(),
                cwd: cwd.clone(),
                env: env.iter().map(|(k, v)| (k.into(), v.into())).collect(),
                clean_env: *clean_env,
                size: None,
                vm: None,
            },
            TerminalEnd::Vm { socket } => SessionSpec {
                agent: record.agent_ref.clone(),
                command: Vec::new(),
                cwd: PathBuf::from(&record.worktree),
                env: Vec::new(),
                clean_env: false,
                size: None,
                vm: Some(socket.clone()),
            },
        };
        let pid = sessions::spawn_shim(&self.shim, &self.layout, &spec)
            .await
            .map_err(failed)?;
        handle.session = Some(pid);
        lock(&self.started_at).insert(record.agent_ref.clone(), std::time::Instant::now());
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
        let debounce = self.idle_debounce();
        let mut agents = Vec::new();
        for mut a in records {
            let status = self.agent_status(&mut a, debounce).await;
            // One being started, deleted or resumed is left as it's
            // recorded until that's done.
            if lock(&self.agents_busy).contains(&a.agent_ref) {
                agents.push(self.agent_info(&a, &status));
                continue;
            }
            let state = self.observed_state(&a, &status).await;
            if state != a.state {
                // Only from what it was read in: a sleep or a wake may
                // have finished meanwhile.
                let was = a.state;
                self.set_state_if(&mut a, was, state);
            }
            agents.push(self.live_info(&a, &status).await);
        }
        Ok(AgentList { agents })
    }

    /// Presses Escape in an agent's terminal, which stops Claude's turn
    /// (PRD: Menu bar, Right-click). Like a read, it never wakes one.
    pub async fn interrupt_agent(&self, p: AgentParams) -> Result<Agent, ErrorObject> {
        let host = self.host(p.host.as_deref())?;
        validate_ref(&p.agent).map_err(invalid)?;
        let mut record = self.store().agent(host, &p.agent).map_err(store_error)?;
        let not_running = || {
            error(
                ErrorCode::Conflict,
                format!("{} isn't running, so there's nothing to interrupt", p.agent),
            )
        };
        if record.state == AgentState::Asleep {
            return Err(not_running());
        }
        let socket = self.layout.session_socket(&p.agent);
        match sessions::send_input(&socket, ESCAPE).await {
            Ok(()) => {}
            Err(_) if !self.session_runs(&p.agent).await => return Err(not_running()),
            Err(e) => {
                return Err(internal(format!("can't reach {}'s session: {e}", p.agent)));
            }
        }
        crate::log!("vibed: interrupted {}", p.agent);
        let status = self.agent_status(&mut record, self.idle_debounce()).await;
        // As agents.list does: one being started, woken or deleted isn't
        // measured.
        if lock(&self.agents_busy).contains(&p.agent) {
            return Ok(self.agent_info(&record, &status));
        }
        Ok(self.live_info(&record, &status).await)
    }

    /// [`Core::set_state`], but only if the agent's state is still `was`;
    /// otherwise `a` takes the state it's in now. Says whether it set it.
    pub(crate) fn set_state_if(
        &self,
        a: &mut AgentRecord,
        was: AgentState,
        state: AgentState,
    ) -> bool {
        // The store's lock is held from the look to the change, so nothing
        // else records a state in between.
        let mut store = self.store();
        match store.agent(&a.host, &a.agent_ref) {
            Ok(now) if now.state == was => {}
            Ok(now) => {
                a.state = now.state;
                return false;
            }
            Err(_) => return false,
        }
        let changed = store.set_agent_state(&a.host, &a.agent_ref, state);
        drop(store);
        self.note_state(a, state, changed);
        true
    }

    pub(crate) fn set_state(&self, a: &mut AgentRecord, state: AgentState) {
        let changed = self.store().set_agent_state(&a.host, &a.agent_ref, state);
        self.note_state(a, state, changed);
    }

    fn note_state(
        &self,
        a: &mut AgentRecord,
        state: AgentState,
        changed: Result<bool, StoreError>,
    ) {
        if matches!(state, AgentState::Asleep | AgentState::Paused) {
            lock(&self.asleep_since).frozen(&a.agent_ref, self.now());
        } else {
            lock(&self.asleep_since).running(&a.agent_ref);
        }
        match changed {
            Ok(true) => {
                crate::log!("vibed: {} is {state} now, was {}", a.agent_ref, a.state);
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

    /// Wakes an agent: exactly where it was, or with its processes gone,
    /// `claude --resume` in its compartment, on a new shim (PRD: Failures).
    /// One paused, by a limit or memory pressure, is held to the limits
    /// there are now and woken; any other that runs is left as it is. A
    /// send, an attach and a due `/loop` wakeup all come through here.
    pub async fn resume_agent(
        &self,
        host: Option<&str>,
        agent_ref: &str,
    ) -> Result<Agent, ErrorObject> {
        let host = self.host(host)?;
        let _held = self.hold(agent_ref)?;
        let record = self.store().agent(host, agent_ref).map_err(store_error)?;
        // One that isn't awake must fit under `max_awake_agents` like a
        // start: an idle agent sleeps early to make room, or it's refused.
        // Never for memory (#47).
        let _admitted;
        let runs = self.session_runs(agent_ref).await;
        if !runs || record.state == AgentState::Asleep {
            _admitted = self.admit(host, agent_ref, "wake").await?;
        }
        let woke = self.resume_steps(agent_ref, record).await;
        if woke.is_ok() {
            lock(&self.sleep_watch).woke(agent_ref);
            self.pressure_unpause(agent_ref);
        }
        woke
    }

    /// Admits one more awake agent for `what` (`agent_ref`, or a new
    /// agent when it's empty) under `max_awake_agents`: agents still
    /// inside their idle debounce sleep early, idle longest first, to make
    /// room; then it's refused. Starts and wakes never queue (PRD: Host
    /// limits). It counts as awake until the returned value is dropped.
    async fn admit(
        &self,
        host: &str,
        agent_ref: &str,
        what: &str,
    ) -> Result<Admitted<'_>, ErrorObject> {
        let host_limits =
            limits::load(&self.layout.host_config()).map_err(|e| failed(e.to_string()))?;
        let _one_at_a_time = self.admitting.lock().await;
        let mut slept = Vec::new();
        loop {
            let awake = self.awake(host, agent_ref).await?.len() + *lock(&self.starting);
            let Some(refusal) = too_many_awake(awake, &host_limits) else {
                return Ok(self.count_starting());
            };
            let why = if agent_ref.is_empty() {
                "early, to make room for a new agent".to_owned()
            } else {
                format!("early, to make room for {agent_ref} to wake")
            };
            match self.sleep_one_early(Early::Any, &why).await {
                Some(r) => slept.push(r),
                None => {
                    let tried = match slept.len() {
                        0 => "No agent is idle, to go to sleep and make room".into(),
                        _ => format!(
                            "{} went to sleep to make room, and no other agent is idle",
                            slept.join(", ")
                        ),
                    };
                    let who = if agent_ref.is_empty() {
                        String::new()
                    } else {
                        format!(" {agent_ref}")
                    };
                    return Err(error(
                        ErrorCode::Conflict,
                        format!("can't {what}{who}: {refusal}. {tried}"),
                    ));
                }
            }
        }
    }

    async fn resume_steps(
        &self,
        agent_ref: &str,
        mut record: AgentRecord,
    ) -> Result<Agent, ErrorObject> {
        if self.session_runs(agent_ref).await {
            // Paused by a limit, it runs again under the limits there are
            // now, raised or not: one it's still over pauses it again. One
            // asleep with its session still there (frozen) is thawed the
            // same way.
            if matches!(record.state, AgentState::Paused | AgentState::Asleep) {
                let backend = self.backend(record.environment)?;
                let handle = Core::handle_of(&record)?;
                let host_limits =
                    limits::load(&self.layout.host_config()).map_err(|e| failed(e.to_string()))?;
                let limits = Limits::new(record.memory, &host_limits);
                let (b, h) = (Arc::clone(&backend), handle.clone());
                blocking(move || b.apply(&h, &limits)).await?;
                let (b, h) = (Arc::clone(&backend), handle.clone());
                blocking(move || b.wake(&h)).await?;
                self.pi_sync_copy(&record, &handle).await;
                crate::log!("vibed: {agent_ref} resumed from being {}", record.state);
                let status = self.agent_status(&mut record, self.idle_debounce()).await;
                let state = self.running_state(agent_ref, &status).await;
                self.set_state(&mut record, state);
                return Ok(self.agent_info(&record, &status));
            }
            let status = self.agent_status(&mut record, self.idle_debounce()).await;
            return Ok(self.agent_info(&record, &status));
        }
        // The session it's in now, after a `/clear`, is the one to resume.
        self.agent_status(&mut record, self.idle_debounce()).await;
        let backend = self.backend(record.environment)?;
        let mut handle = Core::handle_of(&record)?;
        if handle.session.is_some() {
            let (b, h) = (Arc::clone(&backend), handle.clone());
            match blocking(move || b.wake(&h)).await? {
                Woke::Resume => {}
                // Claude is still where it was, in a VM that slept or
                // whose shim died: a new shim takes its terminal (PRD:
                // Failures, A shim dies).
                Woke::Reattach(end) => {
                    let _ = remove_session_files(&self.layout.session_socket(agent_ref));
                    self.hold_terminal(&backend, &mut handle, &record, &end)
                        .await?;
                    crate::log!("vibed: {agent_ref} woke where it was");
                    let status = self.agent_status(&mut record, self.idle_debounce()).await;
                    let state = self.running_state(agent_ref, &status).await;
                    self.set_state(&mut record, state);
                    return Ok(self.agent_info(&record, &status));
                }
                // What's left of its tree ends: it has no terminal.
                Woke::Exact => {
                    let (b, h) = (Arc::clone(&backend), handle.clone());
                    blocking(move || b.stop(&h)).await?;
                }
            }
        }
        let _ = remove_session_files(&self.layout.session_socket(agent_ref));
        let worktree = PathBuf::from(&record.worktree);
        // A session with no transcript has nothing to resume, so it starts
        // again under the same id, on the prompt it never got to: Claude's
        // before its first answer, Pi's before its first message.
        let resume = handle.home.has_transcript(&worktree, &record.session_id);
        let prompt = if resume {
            None
        } else {
            kept_start_prompt(&handle)
        };
        self.launch(&backend, &mut handle, &record, prompt.as_deref(), resume)
            .await?;
        crate::log!(
            "vibed: {agent_ref} {}",
            match (resume, &prompt) {
                (true, _) => "resumed its session",
                (false, Some(_)) => "had no transcript, so it started again on its prompt",
                (false, None) => "had no transcript, so it started again",
            }
        );
        let status = self.agent_status(&mut record, self.idle_debounce()).await;
        let state = self.running_state(agent_ref, &status).await;
        self.set_state(&mut record, state);
        Ok(self.agent_info(&record, &status))
    }

    /// Puts a running agent to sleep through its backend's `Sleeper`: frozen,
    /// or saved to disk, as the backend does it. It's `asleep` until a send
    /// or an attach wakes it. The idle rules decide when ([`crate::sleep`]);
    /// this is for asking.
    pub async fn sleep_agent(
        &self,
        host: Option<&str>,
        agent_ref: &str,
    ) -> Result<Agent, ErrorObject> {
        self.sleep_for(host, agent_ref, "asked to", false).await
    }

    /// [`Core::sleep_agent`], saying `why` in the log. A backend that
    /// sleeps to disk gets room for the snapshot first, or the agent stays
    /// awake (PRD: Sleep and wake; Failures, Low disk); one such sleep at a
    /// time, so they can't share out the same room. An `auto`matic sleep,
    /// one vibed decided on, holds off a send for as long as it takes, and
    /// is called off if a send is typing or a client has attached.
    pub(crate) async fn sleep_for(
        &self,
        host: Option<&str>,
        agent_ref: &str,
        why: &str,
        auto: bool,
    ) -> Result<Agent, ErrorObject> {
        let host = self.host(host)?;
        let _held = self.hold(agent_ref)?;
        let _typing = if auto {
            let refuse = |why: &str| {
                error(
                    ErrorCode::Conflict,
                    format!("{agent_ref} stays awake: {why}"),
                )
            };
            let guard = self
                .typing(agent_ref)
                .try_lock_owned()
                .map_err(|_| refuse(crate::sleep::TYPING))?;
            if let Probe::Running(s) = sessions::probe(&self.layout.session_socket(agent_ref)).await
                && !s.clients.is_empty()
            {
                return Err(refuse(crate::sleep::ATTACHED));
            }
            Some(guard)
        } else {
            None
        };
        let mut record = self.store().agent(host, agent_ref).map_err(store_error)?;
        if !self.session_runs(agent_ref).await {
            return Err(error(
                ErrorCode::Conflict,
                format!("{agent_ref} isn't running, so it can't sleep"),
            ));
        }
        let backend = self.backend(record.environment)?;
        let handle = Core::handle_of(&record)?;
        let _one_snapshot_at_a_time = if backend.capabilities().sleep_frees_memory {
            Some(self.snapshotting.lock().await)
        } else {
            None
        };
        self.make_room_for_sleep(agent_ref, &backend, &handle)
            .await
            .map_err(|why| {
                error(
                    ErrorCode::Conflict,
                    format!("{agent_ref} can't sleep: {why}"),
                )
            })?;
        blocking(move || backend.sleep(&handle)).await?;
        // Paused by a limit, it stays paused: that's what a wake must clear.
        if record.state != AgentState::Paused {
            self.set_state(&mut record, AgentState::Asleep);
        }
        crate::log!("vibed: {agent_ref} is asleep ({why})");
        let status = self.agent_status(&mut record, self.idle_debounce()).await;
        Ok(self.agent_info(&record, &status))
    }

    /// After a reboot, every agent is `asleep` (PRD: Key user flows,
    /// Reboot): its processes are gone, and a send or an attach brings it
    /// back, from its snapshot where it has one, and otherwise with
    /// `claude --resume`. A reboot is a new boot ID since `vibed` last ran:
    /// Linux's, or macOS's boot session UUID. Returns how many it put to
    /// sleep.
    pub async fn asleep_after_reboot(&self) -> usize {
        let Some(boot) = boot_id() else {
            return 0;
        };
        let file = self.layout.data_dir().join(BOOT_FILE);
        let keep = || {
            if let Err(e) = fs::write(&file, &boot) {
                crate::log!("vibed: can't keep {}: {e}", file.display());
            }
        };
        let before = fs::read_to_string(&file).ok();
        if before.as_deref().is_none_or(|b| b.trim() == boot.trim()) {
            keep();
            return 0;
        }
        // Kept only once they're marked, so a failure here is tried again
        // at the next start.
        let Ok(agents) = self.store().agents(LOCAL_HOST) else {
            return 0;
        };
        let mut asleep = 0;
        for mut a in agents {
            if a.starting || a.state == AgentState::Asleep || self.session_runs(&a.agent_ref).await
            {
                continue;
            }
            self.set_state(&mut a, AgentState::Asleep);
            asleep += 1;
        }
        keep();
        if asleep > 0 {
            crate::log!(
                "vibed: this host restarted, so its {asleep} agent{} asleep; a send or an attach resumes each",
                if asleep == 1 { " is" } else { "s are" }
            );
        }
        asleep
    }

    /// Deletes an agent and everything VibeOS made for it but its branch
    /// (PRD: Key user flows, Delete).
    pub async fn delete_agent(&self, p: AgentDeleteParams) -> Result<Agent, ErrorObject> {
        let host = self.host(p.host.as_deref())?;
        validate_ref(&p.agent).map_err(invalid)?;
        let _held = self.hold(&p.agent)?;
        let mut record = self.store().agent(host, &p.agent).map_err(store_error)?;
        // What it was doing, as its transcript is about to go.
        let status = self.agent_status(&mut record, self.idle_debounce()).await;
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
        // Not the daemon's own socket, for an agent named `vibed`.
        let socket = self.layout.session_socket(&p.agent);
        if socket != self.layout.socket() {
            remove_session_files(&socket)
                .map_err(|e| failed(format!("can't remove {}'s socket: {e}", p.agent)))?;
        }
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
        if *lock(&self.starting) > 0 {
            return;
        }
        // Nor while it's being removed; and it holds off a removal until
        // it's done.
        let project_use = self.project_use(project_name);
        let Ok(_in_use) = project_use.try_read() else {
            return;
        };
        if !project.is_dir() {
            return;
        }
        for a in agents.iter().filter(|a| a.project == project_name) {
            if lock(&self.agents_busy).contains(&a.agent_ref)
                || self.session_runs(&a.agent_ref).await
            {
                return;
            }
        }
        clear_stale_maintenance_lock(project_name, project).await;
        // In the foreground: this is a background task already, and a
        // detached gc would outlive it unseen. `gc.autoDetach` keeps gc there
        // on gits before 2.47. `maintenance.autoDetach` does nothing here,
        // as only commands that start maintenance themselves read it, and
        // it's harmless.
        //
        // Without hooks: this runs on the host, and `linux` agents can write
        // to `.git`. Repo config that names a command isn't covered.
        //
        // The tasks by name, as what `run --auto` does by default depends on
        // git's version: gc until 2.54, then geometric-repack, which ignores
        // `gc.auto`, with a threshold that 2.56 raised. gc and commit-graph
        // are in every git with `maintenance`, at the same thresholds:
        // `gc.auto` and `gc.autoPackLimit`, and
        // `maintenance.commit-graph.auto`. Neither depends on the order they
        // run in.
        //
        // Not with settings in the environment: a `vibed` started from an agent's
        // terminal has the `gc.auto=0` agents get, as they leave this to `vibed`, and
        // gc would quietly never run. What's in a git config file is someone's choice,
        // and stands.
        let maintain = [
            "-c",
            "gc.autoDetach=false",
            "-c",
            "maintenance.autoDetach=false",
            "-c",
            "core.hooksPath=/dev/null",
            "maintenance",
            "run",
            "--task=gc",
            "--task=commit-graph",
            "--auto",
            "--quiet",
        ];
        match git_without(project, &maintain, FETCH_TIMEOUT, &GIT_CONFIG_FROM_ENV).await {
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

/// Removes the lock of a `git maintenance` run that was killed, at its
/// timeout or when vibed shut down, so couldn't remove it itself. With
/// `--auto`, git takes a lock that's there to mean another run is under way,
/// and quietly does nothing, for good. vibed's own run can't be older than
/// its timeout. Someone else's could be, and then only overlaps with this
/// one, which git allows for: gc has its own lock, `gc.pid`. A lock dated
/// ahead of now counts the same way, as a clock that's been set back leaves
/// one, and no run under way is that far off.
async fn clear_stale_maintenance_lock(project_name: &str, project: &Path) {
    // Where the lock is from any checkout, linked ones too. It can be
    // relative to the checkout.
    let path = ["rev-parse", "--git-path", "objects/maintenance.lock"];
    let Ok(Ok(path)) = git(project, &path).await else {
        return;
    };
    let lock = project.join(path);
    // The link itself if it's one: `.git` is agents' to write to.
    let stale = fs::symlink_metadata(&lock)
        .and_then(|m| m.modified())
        .is_ok_and(|at| at.elapsed().unwrap_or_else(|ahead| ahead.duration()) > FETCH_TIMEOUT);
    if !stale {
        return;
    }
    match fs::remove_file(&lock) {
        Ok(()) => crate::log!(
            "vibed: git maintenance on {project_name}: removed a stale {}",
            lock.display()
        ),
        Err(e) => crate::log!(
            "vibed: git maintenance on {project_name}: can't remove a stale {}: {e}",
            lock.display()
        ),
    }
}

/// Where `vibed` keeps the boot ID it last ran under, in its state
/// directory.
pub const BOOT_FILE: &str = "boot_id";

/// This boot's ID: Linux's, or macOS's boot session UUID.
fn boot_id() -> Option<String> {
    #[cfg(target_os = "macos")]
    let id = {
        let mut buf = [0u8; 64];
        let mut len = buf.len();
        // SAFETY: the buffer is valid for `len` bytes, which sysctl updates.
        let rc = unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return None;
        }
        let end = buf[..len.min(buf.len())]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(len.min(buf.len()));
        String::from_utf8_lossy(&buf[..end]).into_owned()
    };
    #[cfg(not(target_os = "macos"))]
    let id = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let id = id.trim();
    (!id.is_empty()).then(|| format!("{id}\n"))
}

/// A start or wake admitted and under way, counted as awake until it's
/// dropped.
pub(crate) struct Admitted<'a> {
    core: &'a Core,
}

impl Drop for Admitted<'_> {
    fn drop(&mut self) {
        *lock(&self.core.starting) -= 1;
    }
}

impl Core {
    fn count_starting(&self) -> Admitted<'_> {
        *lock(&self.starting) += 1;
        Admitted { core: self }
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
            shared_services: true,
            agent: AgentKind::Claude,
            pi_model: None,
        }
    }

    #[test]
    fn the_shared_services_go_where_no_other_flag_is_swallowed_by_them() {
        let offered = vec!["--mcp-config".to_owned(), "{\"mcpServers\":{}}".to_owned()];
        let args = claude_args(&record(Mode::Auto), Some("-fix the build"), false, &offered);
        assert_eq!(
            args,
            [
                "--session-id",
                "0b5c3f6e-0000-4000-8000-000000000000",
                "--mcp-config",
                "{\"mcpServers\":{}}",
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
        // On a resume too, which has no `--` to end it.
        let args = claude_args(&record(Mode::Bypass), None, true, &offered);
        assert_eq!(
            args[..4],
            [
                "--resume",
                "0b5c3f6e-0000-4000-8000-000000000000",
                "--mcp-config",
                "{\"mcpServers\":{}}"
            ]
        );
        assert_eq!(args[4], "--model");
    }

    #[test]
    fn pi_gets_its_own_arguments_and_one_flag_for_start_and_resume() {
        let a = AgentRecord {
            agent: AgentKind::Pi,
            pi_model: Some("openai/gpt-5".into()),
            ..record(Mode::Bypass)
        };
        let id = "0b5c3f6e-0000-4000-8000-000000000000";
        assert_eq!(
            agent_args(&a, Some("-fix the build"), false, &[]),
            [
                "--session-id",
                id,
                "--model",
                "openai/gpt-5",
                "--thinking",
                "xhigh",
                "--approve",
                "--",
                "-fix the build"
            ]
        );
        assert_eq!(
            agent_args(&a, None, true, &[]),
            [
                "--session-id",
                id,
                "--model",
                "openai/gpt-5",
                "--thinking",
                "xhigh",
                "--approve"
            ]
        );
        // Claude's are as they were.
        assert_eq!(
            agent_args(&record(Mode::Auto), None, true, &[]),
            claude_args(&record(Mode::Auto), None, true, &[])
        );
    }

    #[test]
    fn claude_gets_the_profile_and_the_prompt_as_an_argument() {
        assert_eq!(
            claude_args(&record(Mode::Auto), Some("-fix the build"), false, &[]),
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
            claude_args(&record(Mode::Bypass), None, true, &[]),
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
    fn only_max_awake_agents_turns_an_agent_away() {
        let few = HostLimits {
            max_awake_agents: 2,
            ..HostLimits::default()
        };
        assert_eq!(too_many_awake(1, &few), None);
        let err = too_many_awake(2, &few).unwrap();
        assert_eq!(
            err,
            "2 agents are awake already, the most this host takes (max_awake_agents)"
        );
        // Memory never does (#47): sixteen 4G agents on any host are fine
        // by default, whatever its RAM.
        assert_eq!(too_many_awake(15, &HostLimits::default()), None);
        assert_eq!(size(1536 * MIB), "1.5G");
        assert_eq!(size(512 * MIB), "512M");
    }
}
