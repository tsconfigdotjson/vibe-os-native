//! Agents through the core: start (worktree, branch, compartment, setup,
//! pre-fill, Claude on its prompt), list, resume and delete, against the
//! test backend with the stand-in for Claude Code.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{Golden, git, git_out, wait_until};
use serde_json::Value;
use vibe_core::model::{Activity, AgentState, Certainty, EventKind, Mode};
use vibe_core::paths::Layout;
use vibe_core::rpc::{
    Agent, AgentDeleteParams, AgentParams, AgentStartParams, ErrorCode, ErrorObject, HostParams,
    ProfileUpdateParams, ProjectAddParams,
};
use vibe_core::shim::{self, Frame, FrameKind, control_hello};
use vibe_core::units::GIB;
use vibe_env::ClaudeHome;
use vibe_env::tree::SessionTree;
use vibed::agents::Backends;
use vibed::core::Core;

struct World {
    golden: Golden,
    core: Arc<Core>,
    layout: Layout,
    /// The project's checkout, `src/app`, and its `origin`.
    project: PathBuf,
    origin: PathBuf,
}

/// The golden world, with `app` added and room for agents.
async fn world() -> World {
    world_with(|_| {}).await
}

async fn world_with(change: impl FnOnce(&mut vibed::core::Config)) -> World {
    let golden = Golden::new();
    let mut config = golden.config();
    change(&mut config);
    let core = Arc::new(Core::open(config).unwrap());
    let project = golden.home.join("src/app");
    core.add_project(ProjectAddParams {
        path: Some(project.to_str().unwrap().into()),
        ..Default::default()
    })
    .await
    .unwrap();
    // 1G agents, so a few fit anywhere.
    for profile in ["software-eng-linux", "software-eng-mac"] {
        core.update_profile(ProfileUpdateParams {
            project: "app".into(),
            profile: profile.into(),
            memory: Some(GIB),
            ..Default::default()
        })
        .unwrap();
    }
    let origin = golden
        .dir
        .path()
        .canonicalize()
        .unwrap()
        .join("remotes/app");
    World {
        layout: Layout::new(&golden.home, "macos"),
        golden,
        core,
        project,
        origin,
    }
}

fn start(prompt: &str) -> AgentStartParams {
    AgentStartParams {
        project: "app".into(),
        profile: "software-eng-linux".into(),
        prompt: prompt.into(),
        ..Default::default()
    }
}

fn delete(r: &str, force: bool) -> AgentDeleteParams {
    AgentDeleteParams {
        host: None,
        agent: r.into(),
        force: force.then_some(true),
    }
}

fn code(e: &ErrorObject) -> ErrorCode {
    ErrorCode::from_code(e.code).unwrap()
}

/// `r`'s screen, from its shim.
fn screen_of(layout: &Layout, r: &str) -> String {
    shim::connect(
        &layout.session_socket(r),
        &control_hello("test"),
        Duration::from_secs(10),
    )
    .unwrap()
    .read_output(None)
    .unwrap()
    .screen
}

fn wait_screen_of(layout: &Layout, r: &str, text: &str) {
    wait_until(&format!("{text:?} on {r}'s screen"), || {
        screen_of(layout, r).contains(text)
    });
}

impl World {
    fn screen(&self, r: &str) -> String {
        screen_of(&self.layout, r)
    }

    fn wait_screen(&self, r: &str, text: &str) {
        wait_screen_of(&self.layout, r, text);
    }

    /// Whether `r`'s shim answers.
    fn runs(&self, r: &str) -> bool {
        shim::connect(
            &self.layout.session_socket(r),
            &control_hello("test"),
            Duration::from_secs(2),
        )
        .is_ok()
    }

    fn type_line(&self, r: &str, line: &str) {
        let mut c = shim::connect(
            &self.layout.session_socket(r),
            &control_hello("test"),
            Duration::from_secs(10),
        )
        .unwrap();
        shim::write_frame(
            &mut c.stream,
            &Frame::new(FrameKind::Input, format!("{line}\r")),
        )
        .unwrap();
        c.status().unwrap();
    }

    fn claude(&self, r: &str) -> ClaudeHome {
        ClaudeHome::own(self.layout.agent_dir(r).join("claude"))
    }

    /// The session leader of `r`'s running shim.
    fn session(&self, r: &str) -> SessionTree {
        let status = shim::connect(
            &self.layout.session_socket(r),
            &control_hello("test"),
            Duration::from_secs(10),
        )
        .unwrap()
        .status()
        .unwrap();
        SessionTree::new(status.pid)
    }

    /// Everything VibeOS keeps of agents: worktrees, agent directories and
    /// sessions' sockets.
    fn agent_files(&self) -> BTreeSet<PathBuf> {
        fn walk(dir: &Path, out: &mut BTreeSet<PathBuf>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for e in entries.filter_map(Result::ok) {
                out.insert(e.path());
                if e.file_type().unwrap().is_dir() {
                    walk(&e.path(), out);
                }
            }
        }
        let mut out = BTreeSet::new();
        for d in [
            self.layout.worktrees_dir(),
            self.layout.agents_dir(),
            self.layout.run_dir(),
        ] {
            walk(&d, &mut out);
        }
        out
    }

    async fn list(&self) -> Vec<Agent> {
        self.core
            .agents(HostParams::default())
            .await
            .unwrap()
            .agents
    }
}

#[tokio::test]
async fn a_start_makes_the_worktree_branch_and_session_and_claude_writes_its_transcript() {
    let w = world().await;
    let (_, mut events) = w.core.subscribe();
    let a = w.core.start_agent(start("env")).await.unwrap();
    // Three words, naming the worktree and the branch.
    assert_eq!(a.agent.split('-').count(), 3, "{}", a.agent);
    assert_eq!(a.branch, a.agent);
    assert_eq!(a.host, "local");
    assert_eq!(a.project, "app");
    assert_eq!(a.profile, "software-eng-linux");
    assert_eq!(a.state, AgentState::Busy);
    assert_eq!(a.attach, format!("vibe attach {}", a.agent));
    assert_eq!(a.created_at, "2026-10-30T12:00:00Z");
    let worktree = w.layout.worktree("app", &a.agent);
    assert_eq!(Path::new(&a.worktree), worktree);
    // On its own branch, cut from origin's default branch, tracking
    // nothing until its first push sets that up.
    assert_eq!(
        git_out(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"]),
        a.agent
    );
    assert_eq!(
        git_out(&worktree, &["rev-parse", "HEAD"]),
        git_out(&w.origin, &["rev-parse", "HEAD"])
    );
    let upstream = std::process::Command::new("git")
        .arg("-C")
        .arg(&worktree)
        .args(["rev-parse", "--abbrev-ref", "@{upstream}"])
        .output()
        .unwrap();
    assert!(!upstream.status.success(), "no upstream before a push");

    // The transcript is where Claude writes it, by the session id.
    let claude = w.claude(&a.agent);
    let transcript = claude.transcript(&worktree, &a.session_id);
    assert!(
        transcript.starts_with(
            w.layout
                .agent_dir(&a.agent)
                .join("claude/projects/")
                .join(vibe_env::claude::project_dir_name(&worktree))
        ),
        "{}",
        transcript.display()
    );
    wait_until("the transcript", || transcript.is_file());
    let line: Value = serde_json::from_str(
        fs::read_to_string(&transcript)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(line["sessionId"], a.session_id.as_str());
    assert_eq!(line["cwd"], worktree.to_str().unwrap());

    // Pre-filled: onboarding done and the worktree trusted.
    let state: Value = serde_json::from_str(&fs::read_to_string(&claude.state).unwrap()).unwrap();
    assert_eq!(state["hasCompletedOnboarding"], true);
    assert_eq!(
        state["projects"][worktree.to_str().unwrap()]["hasTrustDialogAccepted"],
        true
    );

    // Its temp dir and git settings, as the agent sees them.
    w.wait_screen(&a.agent, "Ready.");
    let screen = w.screen(&a.agent);
    let tmp = w.layout.agent_tmp(&a.agent);
    assert!(
        screen.contains(&format!("TMPDIR={}", tmp.display())),
        "{screen}"
    );
    assert!(
        screen.contains("autoSetupRemote=true gc.auto=0"),
        "{screen}"
    );
    // Nothing of that is in the shared .git/config.
    assert!(
        !fs::read_to_string(w.project.join(".git/config"))
            .unwrap()
            .contains("autoSetupRemote")
    );

    // Listed, with what it uses measured, its session titled for it, and
    // announced.
    let list = w.list().await;
    let measured = Agent {
        memory_used: Some(common::AGENT_MEMORY_USED),
        ..a.clone()
    };
    assert_eq!(list, [measured]);
    let sessions = w.core.sessions(HostParams::default()).await.unwrap();
    let s = sessions
        .sessions
        .iter()
        .find(|s| s.agent == a.agent)
        .unwrap();
    assert_eq!(s.title, format!("{} · software-eng-linux", a.agent));
    let e = events.try_recv().unwrap();
    assert_eq!(e.kind, EventKind::AgentAdded);
    assert_eq!(e.agent.as_deref(), Some(a.agent.as_str()));
    assert_eq!(e.state.as_deref(), Some("busy"));

    w.core.delete_agent(delete(&a.agent, false)).await.unwrap();
}

#[tokio::test]
async fn a_delete_leaves_nothing_but_the_branch() {
    let w = world().await;
    let before = w.agent_files();
    let a = w.core.start_agent(start("question")).await.unwrap();
    w.wait_screen(&a.agent, "Red or blue?");
    let tree = w.session(&a.agent);
    let transcript = w
        .claude(&a.agent)
        .transcript(Path::new(&a.worktree), &a.session_id);
    wait_until("the transcript", || transcript.is_file());
    let (_, mut events) = w.core.subscribe();

    let gone = w.core.delete_agent(delete(&a.agent, false)).await.unwrap();
    assert_eq!(gone.agent, a.agent);
    assert_eq!(gone.state, AgentState::Exited);
    assert!(tree.is_empty(), "its processes are gone");
    assert!(!Path::new(&a.worktree).exists());
    assert!(!w.layout.agent_dir(&a.agent).exists());
    assert!(!w.layout.session_socket(&a.agent).exists());
    assert!(!transcript.exists());
    assert_eq!(w.agent_files(), before, "nothing of the agent is left");
    // git has forgotten the worktree, but the branch is kept.
    let worktrees = git_out(&w.project, &["worktree", "list", "--porcelain"]);
    assert!(!worktrees.contains(&a.agent), "{worktrees}");
    assert_eq!(
        git_out(&w.project, &["branch", "--list", &a.agent]).trim_start_matches("* "),
        a.agent
    );
    assert!(w.list().await.is_empty());
    let e = events.try_recv().unwrap();
    assert_eq!(e.kind, EventKind::AgentDeleted);
    assert_eq!(e.agent.as_deref(), Some(a.agent.as_str()));
    let err = w
        .core
        .delete_agent(delete(&a.agent, false))
        .await
        .unwrap_err();
    assert_eq!(code(&err), ErrorCode::NotFound);
    // The branch keeps its name: a new agent of that name is refused.
    let mut again = start("x");
    again.name = Some(a.agent.clone());
    let err = w.core.start_agent(again).await.unwrap_err();
    assert_eq!(code(&err), ErrorCode::Conflict, "{err}");
}

#[tokio::test]
async fn delete_is_refused_while_a_commit_is_unpushed() {
    let w = world().await;
    let a = w.core.start_agent(start("commit")).await.unwrap();
    w.wait_screen(&a.agent, "Committed.");
    let err = w
        .core
        .delete_agent(delete(&a.agent, false))
        .await
        .unwrap_err();
    assert_eq!(code(&err), ErrorCode::Conflict);
    assert_eq!(
        err.message,
        format!(
            "{r} has unsaved work: {r} has 1 commit that isn't pushed. Commit and push it, or \
             delete with force",
            r = a.agent
        )
    );
    // Still there, and still running.
    assert!(Path::new(&a.worktree).exists());
    assert_eq!(w.list().await[0].state, AgentState::Busy);
    w.type_line(&a.agent, "hi");
    w.wait_screen(&a.agent, "> hi");

    // Pushed (as push.autoSetupRemote lets a plain `git push` do), it goes.
    git(&["-C", &a.worktree, "push", "-q", "origin", &a.agent]);
    w.core.delete_agent(delete(&a.agent, false)).await.unwrap();
    assert!(
        git_out(&w.origin, &["branch", "--list", &a.agent]).contains(&a.agent),
        "the branch is on origin"
    );
}

#[tokio::test]
async fn uncommitted_changes_are_refused_unless_forced() {
    let w = world().await;
    let a = w.core.start_agent(start("dirty")).await.unwrap();
    w.wait_screen(&a.agent, "Left notes.txt.");
    let err = w
        .core
        .delete_agent(delete(&a.agent, false))
        .await
        .unwrap_err();
    assert_eq!(code(&err), ErrorCode::Conflict);
    assert!(err.message.contains("has 1 uncommitted change."), "{err}");
    let tree = w.session(&a.agent);
    w.core.delete_agent(delete(&a.agent, true)).await.unwrap();
    assert!(tree.is_empty());
    assert!(!Path::new(&a.worktree).exists());
    assert!(w.list().await.is_empty());
}

#[tokio::test]
async fn a_project_without_origin_counts_commits_no_other_branch_has() {
    let w = world().await;
    git(&[
        "-C",
        w.project.to_str().unwrap(),
        "remote",
        "remove",
        "origin",
    ]);
    let a = w.core.start_agent(start("commit")).await.unwrap();
    w.wait_screen(&a.agent, "Committed.");
    // Cut from the project's HEAD.
    assert_eq!(
        git_out(&w.project, &["rev-parse", "HEAD"]),
        git_out(&w.project, &["rev-parse", &format!("{}^", a.agent)])
    );
    let err = w
        .core
        .delete_agent(delete(&a.agent, false))
        .await
        .unwrap_err();
    assert!(
        err.message.contains("1 commit no other branch has"),
        "{err}"
    );
    // Merged into another branch, it's safe.
    git(&[
        "-C",
        w.project.to_str().unwrap(),
        "branch",
        "kept",
        &a.agent,
    ]);
    w.core.delete_agent(delete(&a.agent, false)).await.unwrap();
}

#[tokio::test]
async fn setup_runs_in_the_compartment_and_a_failing_one_undoes_the_start() {
    let w = world().await;
    let origin = w.origin.to_str().unwrap();
    // A branch whose setup works, and one whose setup fails.
    for (branch, script) in [
        (
            "with-setup",
            "echo \"setup in $(pwd -P) with gc.auto=$(git config gc.auto)\" >setup-ran\n",
        ),
        (
            "bad-setup",
            "echo installing\necho 'npm: not found' >&2\nexit 7\n",
        ),
    ] {
        git(&["-C", origin, "checkout", "-q", "-b", branch]);
        fs::create_dir_all(w.origin.join(".vibe")).unwrap();
        fs::write(w.origin.join(".vibe/setup.sh"), script).unwrap();
        git(&["-C", origin, "add", "."]);
        git(&["-C", origin, "commit", "-q", "-m", branch]);
        git(&["-C", origin, "checkout", "-q", "-"]);
    }

    let mut p = start("hello");
    p.from = Some("with-setup".into());
    let a = w.core.start_agent(p).await.unwrap();
    let ran = fs::read_to_string(Path::new(&a.worktree).join("setup-ran")).unwrap();
    assert_eq!(ran, format!("setup in {} with gc.auto=0\n", a.worktree));
    let log = w.layout.agent_dir(&a.agent).join("setup.log");
    assert!(log.is_file());
    w.wait_screen(&a.agent, "Working on: hello");
    w.core.delete_agent(delete(&a.agent, true)).await.unwrap();

    let before = w.agent_files();
    let branches = git_out(&w.project, &["branch", "--list"]);
    let mut p = start("hello");
    p.from = Some("bad-setup".into());
    p.name = Some("doomed".into());
    let err = w.core.start_agent(p).await.unwrap_err();
    assert_eq!(code(&err), ErrorCode::CommandFailed);
    assert_eq!(
        err.message,
        ".vibe/setup.sh exited with code 7: npm: not found"
    );
    // Everything it made is gone again, the branch too.
    assert_eq!(w.agent_files(), before);
    assert_eq!(git_out(&w.project, &["branch", "--list"]), branches);
    assert!(w.list().await.is_empty());
    let worktrees = git_out(&w.project, &["worktree", "list"]);
    assert!(!worktrees.contains("doomed"), "{worktrees}");
}

#[tokio::test]
async fn names_and_froms() {
    let w = world().await;
    let mut p = start("hello");
    p.name = Some("fix-the-build".into());
    let a = w.core.start_agent(p.clone()).await.unwrap();
    assert_eq!(
        (a.agent.as_str(), a.branch.as_str()),
        ("fix-the-build", "fix-the-build")
    );
    let err = w.core.start_agent(p.clone()).await.unwrap_err();
    assert_eq!(code(&err), ErrorCode::Conflict);
    assert!(err.message.starts_with("fix-the-build is taken"), "{err}");

    for (name, want) in [
        ("a..b", ErrorCode::InvalidParams),
        ("vibed", ErrorCode::InvalidParams),
        ("x.lock", ErrorCode::InvalidParams),
        (&"a".repeat(33), ErrorCode::InvalidParams),
    ] {
        let mut p = start("x");
        p.name = Some(name.to_owned());
        let err = w.core.start_agent(p).await.unwrap_err();
        assert_eq!(code(&err), want, "{name}: {err}");
    }

    // From the first agent's branch, once it has a commit of its own that
    // origin hasn't: the local branch is newer, so it's the one.
    git(&[
        "-C",
        &a.worktree,
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "local work",
    ]);
    let mut p = start("follow up");
    p.from = Some("fix-the-build".into());
    let b = w.core.start_agent(p).await.unwrap();
    assert_eq!(
        git_out(Path::new(&b.worktree), &["rev-parse", "HEAD"]),
        git_out(Path::new(&a.worktree), &["rev-parse", "HEAD"])
    );

    for (from, want) in [
        ("no-such-branch", ErrorCode::NotFound),
        ("--upload-pack=x", ErrorCode::InvalidParams),
    ] {
        let mut p = start("x");
        p.from = Some(from.into());
        let err = w.core.start_agent(p).await.unwrap_err();
        assert_eq!(code(&err), want, "{from}: {err}");
    }
    for r in [&a.agent, &b.agent] {
        w.core.delete_agent(delete(r, true)).await.unwrap();
    }
}

#[tokio::test]
async fn bad_starts_say_why() {
    let w = world().await;
    let err = w.core.start_agent(start("  ")).await.unwrap_err();
    assert_eq!(code(&err), ErrorCode::InvalidParams);
    let mut p = start("x");
    p.project = "nope".into();
    assert_eq!(
        code(&w.core.start_agent(p).await.unwrap_err()),
        ErrorCode::NotFound
    );
    let mut p = start("x");
    p.profile = "nope".into();
    assert_eq!(
        code(&w.core.start_agent(p).await.unwrap_err()),
        ErrorCode::NotFound
    );
    let mut p = start("x");
    p.host = Some("vps".into());
    assert_eq!(
        code(&w.core.start_agent(p).await.unwrap_err()),
        ErrorCode::NotFound
    );
    let err = w
        .core
        .delete_agent(delete("../etc", false))
        .await
        .unwrap_err();
    assert_eq!(code(&err), ErrorCode::InvalidParams);
    assert!(w.list().await.is_empty());
}

#[tokio::test]
async fn a_host_without_a_backend_for_the_environment_refuses() {
    let w = world_with(|c| {
        c.backends = Backends::new();
    })
    .await;
    let err = w.core.start_agent(start("x")).await.unwrap_err();
    assert_eq!(code(&err), ErrorCode::Conflict);
    assert_eq!(err.message, "this host can't run linux agents yet");
    assert!(w.agent_files().is_empty());
}

#[tokio::test]
async fn memory_is_a_ceiling_not_a_reservation() {
    // A 6G host whose awake agents' profiles already ask for more than it
    // has (#47): each agent's memory is the most it may use, so a 4G start
    // still goes ahead, and the host swaps if they use it all.
    let w = world_with(|c| {
        c.memory = Arc::new(|| {
            Some(vibe_core::rpc::MemoryUse {
                total: 6 * GIB,
                used: 5 * GIB,
                swap_total: 0,
                swap_used: 0,
                pressure: None,
            })
        });
        c.pressure = Arc::new(vibed::pressure::NoPressure);
    })
    .await;
    w.core
        .update_profile(ProfileUpdateParams {
            project: "app".into(),
            profile: "software-eng-linux".into(),
            memory: Some(4 * GIB),
            ..Default::default()
        })
        .unwrap();
    let mut started = Vec::new();
    for prompt in ["one", "two", "three"] {
        let a = w.core.start_agent(start(prompt)).await.unwrap();
        assert_eq!(a.memory, 4 * GIB);
        started.push(a);
    }
    for a in &started {
        w.wait_screen(&a.agent, "Working on:");
    }
    // 12G of ceilings on a 6G host, all awake, and each one's real use
    // reported against its ceiling.
    let list = w.list().await;
    assert_eq!(list.len(), 3);
    for a in &list {
        assert_ne!(a.state, AgentState::Asleep, "{a:?}");
        assert_eq!(
            (a.memory, a.memory_used),
            (4 * GIB, Some(common::AGENT_MEMORY_USED))
        );
    }
    // A wake doesn't count memory either.
    let one = &started[0].agent;
    w.core.sleep_agent(None, one).await.unwrap();
    let woke = w.core.resume_agent(None, one).await.unwrap();
    assert_ne!(woke.state, AgentState::Asleep);
    // The host's own use, as its kernel measures it, and no warning while
    // it has room.
    let local = w.core.hosts().unwrap().hosts.remove(0);
    let memory = local.memory.expect("the host's memory");
    assert_eq!((memory.used, memory.total), (5 * GIB, 6 * GIB));
    assert_eq!(memory.pressure, None);
    for a in &started {
        w.core.delete_agent(delete(&a.agent, true)).await.unwrap();
    }
}

#[tokio::test]
async fn the_host_reports_its_memory_and_warns_when_it_is_short() {
    let w = world().await;
    // The golden Mac: macOS warns of pressure, and it's only a warning.
    let local = w.core.hosts().unwrap().hosts.remove(0);
    assert_eq!(
        local.memory,
        Some(vibe_core::rpc::MemoryUse {
            pressure: Some("macOS warns of memory pressure".into()),
            ..common::golden_memory()
        })
    );
    let a = w.core.start_agent(start("hello")).await.unwrap();
    w.wait_screen(&a.agent, "Working on: hello");
    w.core.delete_agent(delete(&a.agent, true)).await.unwrap();

    // Where the kernel says nothing of pressure, under 5% free is short.
    let w = world_with(|c| {
        c.pressure = Arc::new(vibed::pressure::NoPressure);
        c.memory = Arc::new(|| {
            Some(vibe_core::rpc::MemoryUse {
                total: 16 * GIB,
                used: 15 * GIB + 900 * 1024 * 1024,
                swap_total: 0,
                swap_used: 0,
                pressure: None,
            })
        });
    })
    .await;
    let memory = w.core.hosts().unwrap().hosts.remove(0).memory.unwrap();
    assert_eq!(
        memory.pressure.as_deref(),
        Some("only 124M of its 16G is free")
    );
}

#[tokio::test]
async fn an_exited_agent_resumes_its_session() {
    let w = world().await;
    let mut p = start("hello");
    p.profile = "software-eng-mac".into();
    let a = w.core.start_agent(p).await.unwrap();
    w.wait_screen(&a.agent, "Working on: hello");
    // Its settings are its own from the start: editing the profile later
    // changes nothing for it.
    w.core
        .update_profile(ProfileUpdateParams {
            project: "app".into(),
            profile: "software-eng-mac".into(),
            mode: Some(Mode::Bypass),
            ..Default::default()
        })
        .unwrap();
    let (_, mut events) = w.core.subscribe();
    w.session(&a.agent).end(Duration::from_secs(5));
    let list = w.list().await;
    assert_eq!(list[0].state, AgentState::Exited);
    let e = events.try_recv().unwrap();
    assert_eq!(
        (e.kind, e.state.as_deref()),
        (EventKind::AgentState, Some("exited"))
    );

    let back = w.core.resume_agent(None, &a.agent).await.unwrap();
    assert_eq!(back.state, AgentState::Busy);
    assert_eq!(back.session_id, a.session_id);
    w.wait_screen(&a.agent, &format!("Resumed {}.", a.session_id));
    let transcript = w
        .claude(&a.agent)
        .transcript(Path::new(&a.worktree), &a.session_id);
    let text = fs::read_to_string(&transcript).unwrap();
    assert!(text.contains("\"resumed\":true"), "{text}");
    let status = shim::connect(
        &w.layout.session_socket(&a.agent),
        &control_hello("test"),
        Duration::from_secs(10),
    )
    .unwrap()
    .status()
    .unwrap();
    assert!(
        status.command.iter().any(|c| c == "--permission-mode"),
        "still auto: {:?}",
        status.command
    );
    assert!(
        !status
            .command
            .iter()
            .any(|c| c == "--dangerously-skip-permissions"),
        "{:?}",
        status.command
    );
    // Running already, a resume changes nothing.
    w.core.resume_agent(None, &a.agent).await.unwrap();
    assert_eq!(w.session(&a.agent).sid, status.pid);
    w.core.delete_agent(delete(&a.agent, false)).await.unwrap();
}

#[tokio::test]
async fn interrupt_presses_escape_while_the_session_runs() {
    let w = world().await;
    // A "Claude" that writes down the first key it's sent, raw.
    common::write_script(
        &w.golden.claude,
        "#!/bin/sh\nstty raw -echo\n: >.vibe-ready\nhead -c 1 | od -An -c >.vibe-key\nexec sleep 600\n",
    );
    let a = w.core.start_agent(start("anything")).await.unwrap();
    let worktree = Path::new(&a.worktree);
    wait_until("the stand-in to be ready", || {
        worktree.join(".vibe-ready").exists()
    });
    let interrupt = |r: &str| AgentParams {
        host: None,
        agent: r.into(),
    };
    let got = w.core.interrupt_agent(interrupt(&a.agent)).await.unwrap();
    assert_eq!(got.agent, a.agent);
    assert_eq!(got.state, AgentState::Busy);
    assert_eq!(got.memory, GIB);
    assert_eq!(got.memory_used, Some(common::AGENT_MEMORY_USED));
    let key = worktree.join(".vibe-key");
    wait_until("the key", || {
        fs::read_to_string(&key).is_ok_and(|k| !k.trim().is_empty())
    });
    assert_eq!(fs::read_to_string(&key).unwrap().trim(), "033");
    // Interrupting presses a key; it doesn't attach.
    let sessions = w.core.sessions(HostParams::default()).await.unwrap();
    assert_eq!(sessions.sessions[0].clients, 0);

    // Once its session has gone there's nothing to interrupt, and a
    // listing no longer measures it.
    w.session(&a.agent).end(Duration::from_secs(5));
    let list = w.list().await;
    assert_eq!(
        (list[0].state, list[0].memory, list[0].memory_used),
        (AgentState::Exited, GIB, None)
    );
    let e = w
        .core
        .interrupt_agent(interrupt(&a.agent))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Conflict, "{e:?}");
    assert!(e.message.contains("isn't running"), "{}", e.message);
    let e = w
        .core
        .interrupt_agent(interrupt("lone-grey-wolf"))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound, "{e:?}");
    let e = w.core.interrupt_agent(interrupt("../x")).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidParams, "{e:?}");
    w.core.delete_agent(delete(&a.agent, true)).await.unwrap();
}

#[tokio::test]
async fn agents_outlive_vibed() {
    let w = world().await;
    let a = w.core.start_agent(start("question")).await.unwrap();
    w.wait_screen(&a.agent, "Red or blue?");
    let tree = w.session(&a.agent);
    let before = w.list().await;
    assert_eq!(before[0].state, AgentState::Question);
    let golden = &w.golden;
    drop(w.core);
    // A new vibed finds it running, rebuilds its state from its transcript,
    // and can delete it.
    let core = Arc::new(Core::open(golden.config()).unwrap());
    assert_eq!(core.adopt_sessions().await, 1);
    let list = core.agents(HostParams::default()).await.unwrap().agents;
    assert_eq!(list, before);
    assert_eq!(list[0].status.activity, Activity::AwaitingAnswer);
    // A start doesn't measure memory; a listing does.
    assert_eq!(list[0].memory_used, Some(common::AGENT_MEMORY_USED));
    assert_eq!(
        Agent {
            state: a.state,
            status: a.status.clone(),
            memory_used: None,
            ..list[0].clone()
        },
        a
    );
    core.delete_agent(delete(&a.agent, false)).await.unwrap();
    assert!(tree.is_empty());
}

/// Waits up to 20 s for `answered`.
async fn wait_answered(core: &Core, r: &str, mark: &vibe_status::Mark) {
    for _ in 0..1000 {
        if core.answered(None, r, mark).await.unwrap() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{r} never answered");
}

#[tokio::test]
async fn the_status_follows_the_transcript_through_a_question_and_its_answer() {
    let w = world().await;
    let a = w.core.start_agent(start("pick a question")).await.unwrap();
    w.wait_screen(&a.agent, "Red or blue?");
    let list = w.list().await;
    let s = &list[0].status;
    assert_eq!(list[0].state, AgentState::Question);
    assert_eq!(
        (s.activity, s.certainty),
        (Activity::AwaitingAnswer, Certainty::Firm)
    );
    assert!(s.since.is_some());
    assert_eq!(s.line.as_deref(), Some("AskUserQuestion"));

    // Waiting on the answer to a send: quiet isn't an answer, and the old
    // question isn't one either.
    let mark = w.core.mark_send(None, &a.agent).await.unwrap();
    assert!(!w.core.answered(None, &a.agent, &mark).await.unwrap());
    w.type_line(&a.agent, "red");
    wait_answered(&w.core, &a.agent, &mark).await;
    let list = w.list().await;
    assert_eq!(list[0].state, AgentState::Idle);
    assert_eq!(list[0].status.activity, Activity::Done);

    // A turn after it is waited for the same way.
    let mark = w.core.mark_send(None, &a.agent).await.unwrap();
    assert!(
        !w.core.answered(None, &a.agent, &mark).await.unwrap(),
        "done with the last turn isn't an answer to this one"
    );
    w.type_line(&a.agent, "and now green");
    wait_answered(&w.core, &a.agent, &mark).await;
    assert_eq!(
        w.list().await[0].status.line.as_deref(),
        Some("› and now green")
    );

    // vibed's own asleep stands while it holds; the transcript doesn't
    // undo it.
    {
        let mut store = vibe_core::store::Store::open(&w.layout.db()).unwrap();
        store
            .set_agent_state("local", &a.agent, AgentState::Asleep)
            .unwrap();
    }
    assert_eq!(w.list().await[0].state, AgentState::Asleep);
    w.core.delete_agent(delete(&a.agent, false)).await.unwrap();
    assert!(w.core.mark_send(None, &a.agent).await.is_err());
}

#[tokio::test]
async fn the_status_loop_announces_states_and_statuses() {
    let w = world().await;
    let (_, mut events) = w.core.subscribe();
    let looping = w.core.spawn_status();
    let a = w.core.start_agent(start("question")).await.unwrap();
    // Waits for the event `ok` takes, and returns it.
    async fn next(
        events: &mut tokio::sync::broadcast::Receiver<vibe_core::rpc::Event>,
        what: &str,
        ok: impl Fn(&vibe_core::rpc::Event) -> bool,
    ) -> vibe_core::rpc::Event {
        let found = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let e = events.recv().await.unwrap();
                if ok(&e) {
                    return e;
                }
            }
        })
        .await;
        found.unwrap_or_else(|_| panic!("no {what} event"))
    }
    let r = a.agent.clone();
    let e = next(&mut events, "question", |e| {
        e.kind == EventKind::AgentState && e.state.as_deref() == Some("question")
    })
    .await;
    assert_eq!(e.agent.as_deref(), Some(r.as_str()));
    assert_eq!(e.project.as_deref(), Some("app"));
    let e = next(&mut events, "awaiting_answer status", |e| {
        e.kind == EventKind::AgentStatus
            && e.status.as_ref().map(|s| s.activity) == Some(Activity::AwaitingAnswer)
    })
    .await;
    assert_eq!(e.agent.as_deref(), Some(r.as_str()));
    w.type_line(&r, "blue");
    next(&mut events, "idle", |e| {
        e.kind == EventKind::AgentState && e.state.as_deref() == Some("idle")
    })
    .await;
    let e = next(&mut events, "done status", |e| {
        e.kind == EventKind::AgentStatus
            && e.status.as_ref().map(|s| s.activity) == Some(Activity::Done)
    })
    .await;
    assert_eq!(e.agent.as_deref(), Some(r.as_str()));
    looping.abort();
    w.core.delete_agent(delete(&r, false)).await.unwrap();
}

/// The false hand-up of #46, end to end: a tool call whose transcript has
/// stalled past agent-tail's 20 s guess, with no dialog on the screen, is
/// busy, raises no question and answers no wait; once the screen shows a
/// permission dialog, it's a question.
#[tokio::test]
async fn a_silent_tool_call_is_busy_until_its_screen_shows_a_dialog() {
    let w = world_with(|c| c.clock = Arc::new(vibe_core::time::now)).await;
    let a = w.core.start_agent(start("hello")).await.unwrap();
    w.wait_screen(&a.agent, "Ready.");
    // Claude in auto mode, 30 s into `sleep 60`.
    let transcript = w
        .claude(&a.agent)
        .transcript(Path::new(&a.worktree), &a.session_id);
    let at = vibe_core::time::now() - 30;
    let entry = |v: Value| format!("{v}\n");
    let mut lines = entry(
        serde_json::json!({"type": "user", "sessionId": a.session_id,
        "cwd": a.worktree, "timestamp": vibe_core::time::rfc3339(at - 1),
        "message": {"role": "user", "content": "wait a minute"}}),
    );
    lines += &entry(
        serde_json::json!({"type": "assistant", "sessionId": a.session_id,
        "cwd": a.worktree, "permissionMode": "auto", "timestamp": vibe_core::time::rfc3339(at),
        "message": {"id": "m1", "role": "assistant", "stop_reason": "tool_use",
            "content": [{"type": "tool_use", "id": "toolu_s", "name": "Bash",
                "input": {"command": "sleep 60"}}]}}),
    );
    {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        f.write_all(lines.as_bytes()).unwrap();
        f.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(at as u64))
            .unwrap();
    }
    let (_, mut events) = w.core.subscribe();
    let mut published = std::collections::HashMap::new();
    w.core.refresh_status(&mut published, true).await;
    let mut guessed = false;
    while let Ok(e) = events.try_recv() {
        assert_ne!(e.state.as_deref(), Some("question"), "no hand: {e:?}");
        if let Some(s) = e
            .status
            .filter(|s| s.activity == Activity::AwaitingApproval)
        {
            assert_eq!(s.certainty, Certainty::Inferred);
            guessed = true;
        }
    }
    assert!(guessed, "agent-tail guessed a prompt");
    assert_eq!(w.list().await[0].state, AgentState::Busy);
    let check = w.core.check_agent(None, &a.agent, false).await.unwrap();
    assert_eq!((check.state, check.answered), (AgentState::Busy, false));

    // The dialog comes up.
    for line in ["Do you want to proceed?", "1. Yes", "2. No"] {
        w.type_line(&a.agent, line);
    }
    w.wait_screen(&a.agent, "2. No");
    w.core.refresh_status(&mut published, true).await;
    let mut raised = false;
    while let Ok(e) = events.try_recv() {
        raised |= e.kind == EventKind::AgentState && e.state.as_deref() == Some("question");
    }
    assert!(raised, "the hand goes up");
    let check = w.core.check_agent(None, &a.agent, false).await.unwrap();
    assert_eq!((check.state, check.answered), (AgentState::Question, true));
    w.core.delete_agent(delete(&a.agent, true)).await.unwrap();
}

#[tokio::test]
async fn sessions_are_asked_on_a_full_refresh_or_a_change() {
    let w = world().await;
    let a = w.core.start_agent(start("hello")).await.unwrap();
    w.wait_screen(&a.agent, "Working on: hello");
    let mut published = std::collections::HashMap::new();
    let (_, mut events) = w.core.subscribe();
    let paths = w.core.refresh_status(&mut published, true).await;
    let transcript = w
        .claude(&a.agent)
        .transcript(Path::new(&a.worktree), &a.session_id);
    assert!(paths.contains(&transcript), "{paths:?}");
    let e = events.try_recv().unwrap();
    assert_eq!(e.kind, EventKind::AgentStatus);
    assert_eq!(
        e.status.map(|s| s.activity),
        Some(Activity::Unknown),
        "the stand-in's line says nothing yet"
    );
    assert!(events.try_recv().is_err(), "nothing else changed");
    // Its session ends. A refresh for a file-system event, with nothing new
    // in the transcript, doesn't ask; the next full one does.
    w.session(&a.agent).end(Duration::from_secs(5));
    w.core.refresh_status(&mut published, false).await;
    assert!(events.try_recv().is_err());
    w.core.refresh_status(&mut published, true).await;
    let e = events.try_recv().unwrap();
    assert_eq!(
        (e.kind, e.state.as_deref()),
        (EventKind::AgentState, Some("exited"))
    );
    w.core.delete_agent(delete(&a.agent, false)).await.unwrap();
    // A deleted agent's tail and status go with it.
    assert!(w.core.refresh_status(&mut published, true).await.is_empty());
    assert!(published.is_empty());
}

#[tokio::test]
async fn after_a_clear_the_agent_follows_its_new_session() {
    let w = world().await;
    let a = w.core.start_agent(start("question")).await.unwrap();
    w.wait_screen(&a.agent, "Red or blue?");
    // `/clear`: Claude starts a new session in the same worktree, in a
    // new transcript beside the first.
    let claude = w.claude(&a.agent);
    let worktree = Path::new(&a.worktree);
    let new_id = "c1ea4ed0-0000-4000-8000-00000000c1ea";
    // It began after the first, whose entries the stand-in dates now.
    let later = vibe_core::time::rfc3339(vibe_core::time::now() + 60);
    let line = serde_json::json!({"type": "user", "sessionId": new_id, "cwd": a.worktree,
        "timestamp": later,
        "message": {"role": "user", "content": "after the clear"}});
    std::thread::sleep(Duration::from_millis(20));
    fs::write(claude.transcript(worktree, new_id), format!("{line}\n")).unwrap();
    let list = w.list().await;
    assert_eq!(list[0].session_id, new_id);
    assert_eq!(list[0].status.activity, Activity::Thinking);
    assert_eq!(list[0].status.line.as_deref(), Some("› after the clear"));
    // Recorded, so a resume continues the new session.
    let store = vibe_core::store::Store::open(&w.layout.db()).unwrap();
    assert_eq!(store.agent("local", &a.agent).unwrap().session_id, new_id);
    w.core.delete_agent(delete(&a.agent, true)).await.unwrap();
}

#[tokio::test]
async fn one_thing_at_a_time_per_agent() {
    let w = world().await;
    let a = w.core.start_agent(start("hello")).await.unwrap();
    let (first, second) = tokio::join!(
        w.core.delete_agent(delete(&a.agent, false)),
        w.core.delete_agent(delete(&a.agent, false)),
    );
    let errors: Vec<_> = [first, second]
        .into_iter()
        .filter_map(Result::err)
        .collect();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        matches!(code(&errors[0]), ErrorCode::Conflict | ErrorCode::NotFound),
        "{:?}",
        errors[0]
    );
    assert!(w.list().await.is_empty());
}

/// Commits loose objects to a project, and sets a threshold they're over, so
/// a maintenance run packs them. git counts only those in objects/17/ and
/// guesses the rest, so there are enough for some to be there: their
/// contents, and so their names, are fixed.
fn commit_loose_objects(dir: &Path) {
    let project = dir.to_str().unwrap();
    git(&["-C", project, "config", "gc.auto", "1"]);
    fs::create_dir_all(dir.join("many")).unwrap();
    for i in 0..1500 {
        fs::write(dir.join(format!("many/{i}")), format!("{i}\n")).unwrap();
    }
    git(&["-C", project, "add", "."]);
    // Not the maintenance git starts after a commit; and vibed's fetch, as
    // a start makes one, mustn't start one either.
    git(&[
        "-C",
        project,
        "-c",
        "maintenance.auto=false",
        "commit",
        "-q",
        "-m",
        "many",
    ]);
    assert!(loose_objects(dir) > 1);
}

fn loose_objects(dir: &Path) -> u32 {
    let out = git_out(dir, &["count-objects"]);
    out.split_whitespace()
        .next()
        .unwrap()
        .parse::<u32>()
        .unwrap()
}

/// A send to a program that's just been started waits until it takes input, as it
/// does when the send wakes it. Typed sooner, it sits in a terminal that isn't raw
/// yet, so its Enter becomes a newline in the text and nothing is ever submitted,
/// which is what a real Pi did with a send right after a resume.
#[tokio::test(flavor = "multi_thread")]
async fn a_send_just_after_a_start_is_submitted() {
    let w = world().await;
    let got = w.project.with_file_name("what-it-read");
    // Slow to start, as a real one is, and only then raw, as a real one goes.
    common::write_script(
        &w.golden.claude,
        &format!(
            "#!/bin/sh\nsleep 2\nstty raw -echo\nprintf '\\033[?2004h> '\nexec cat >'{}'\n",
            got.display()
        ),
    );
    let a = w.core.start_agent(start("hello")).await.unwrap();
    w.core
        .send_input(
            None,
            &a.agent,
            vibe_mcp::Input::Text {
                text: "after a start".into(),
                submit: true,
                force: false,
            },
        )
        .await
        .unwrap();
    let read = || fs::read(&got).unwrap_or_default();
    common::wait_until("it to read what was sent", || {
        read().ends_with(b"\r") || read().ends_with(b"\n")
    });
    let read = String::from_utf8_lossy(&read()).into_owned();
    assert!(read.contains("after a start"), "{read:?}");
    assert!(read.ends_with('\r'), "not submitted: {read:?}");
    w.core.delete_agent(delete(&a.agent, true)).await.unwrap();
}

#[tokio::test]
async fn git_maintenance_runs_only_while_a_project_is_quiet() {
    let w = world().await;
    let project = w.project.to_str().unwrap();
    commit_loose_objects(&w.project);
    let loose = || loose_objects(&w.project);

    let a = w.core.start_agent(start("hello")).await.unwrap();
    w.core.maintain_if_quiet("local", "app", &w.project).await;
    assert!(loose() > 1, "not while an agent is awake");
    w.core.delete_agent(delete(&a.agent, false)).await.unwrap();
    // Quiet, and it's `gc.auto` that says whether there's anything to do, on
    // every git: its own default strategy would go by other thresholds, or
    // not look at this one at all.
    git(&["-C", project, "config", "gc.auto", "6700"]);
    w.core.maintain_if_quiet("local", "app", &w.project).await;
    assert!(loose() > 1, "not with so few loose objects");
    git(&["-C", project, "config", "gc.auto", "0"]);
    w.core.maintain_if_quiet("local", "app", &w.project).await;
    assert!(loose() > 1, "not with gc turned off");
    // The task by name, so a project's strategy isn't what decides, which
    // would leave these loose on any git.
    git(&["-C", project, "config", "gc.auto", "1"]);
    git(&["-C", project, "config", "maintenance.gc.enabled", "false"]);
    w.core.maintain_if_quiet("local", "app", &w.project).await;
    assert_eq!(loose(), 0, "packed once it's quiet");
}

#[tokio::test]
async fn git_maintenance_clears_only_a_stale_lock() {
    let w = world().await;
    let project = w.project.to_str().unwrap();
    commit_loose_objects(&w.project);
    let loose = || loose_objects(&w.project);
    let lock = w.project.join(".git/objects/maintenance.lock");
    let backdate = || {
        let at = std::time::SystemTime::now() - Duration::from_secs(11 * 60);
        let f = fs::OpenOptions::new().write(true).open(&lock).unwrap();
        f.set_modified(at).unwrap();
    };

    // A new one could be a run that's under way, and git stops at it.
    fs::write(&lock, "").unwrap();
    w.core.maintain_if_quiet("local", "app", &w.project).await;
    assert!(lock.exists(), "a new lock stays");
    assert!(loose() > 1, "git stops at a lock");

    // One older than the timeout goes, found from a linked checkout too.
    let linked = w.project.with_file_name("linked");
    git(&[
        "-C",
        project,
        "worktree",
        "add",
        "-q",
        "--detach",
        linked.to_str().unwrap(),
    ]);
    git(&["-C", project, "config", "gc.auto", "0"]);
    backdate();
    w.core.maintain_if_quiet("local", "app", &linked).await;
    assert!(!lock.exists(), "a stale lock goes, from a linked checkout");
    assert!(loose() > 1, "and gc.auto is still off");

    // And then the repo gets packed.
    git(&["-C", project, "config", "gc.auto", "1"]);
    fs::write(&lock, "").unwrap();
    backdate();
    w.core.maintain_if_quiet("local", "app", &w.project).await;
    assert!(!lock.exists(), "a stale lock goes");
    assert_eq!(loose(), 0, "and the repo is packed");
}

#[tokio::test]
async fn git_maintenance_runs_no_hook_in_the_project() {
    let w = world().await;
    commit_loose_objects(&w.project);
    // As an agent could plant it, its VM having `.git` read-write.
    let ran = w.project.with_file_name("hook-ran");
    let hooks = w.project.join(".git/hooks");
    fs::create_dir_all(&hooks).unwrap();
    common::write_script(
        &hooks.join("pre-auto-gc"),
        &format!("#!/bin/sh\ntouch '{}'\n", ran.display()),
    );

    w.core.maintain_if_quiet("local", "app", &w.project).await;
    assert_eq!(loose_objects(&w.project), 0, "gc did run");
    assert!(!ran.exists(), "and it ran no hook");
}

#[tokio::test]
async fn git_maintenance_writes_the_commit_graph_even_without_packing() {
    let w = world().await;
    let project = w.project.to_str().unwrap();
    // gc writes one too when it packs, so it's off, and any commit is enough.
    git(&["-C", project, "config", "gc.auto", "0"]);
    git(&[
        "-C",
        project,
        "config",
        "maintenance.commit-graph.auto",
        "1",
    ]);
    let chain = w
        .project
        .join(".git/objects/info/commit-graphs/commit-graph-chain");
    assert!(!chain.exists());

    w.core.maintain_if_quiet("local", "app", &w.project).await;
    assert!(chain.exists(), "written by the commit-graph task");
}

/// The test backend, but holding an agent to its limits fails, as a real
/// backend's governor or cgroup setup can once the shim runs.
#[derive(Debug)]
struct ApplyFails(vibe_env::testing::TestBackend);

impl vibe_env::Compartment for ApplyFails {
    fn create(&self, spec: &vibe_env::AgentSpec) -> vibe_env::Result<vibe_env::Handle> {
        self.0.create(spec)
    }
    fn run(
        &self,
        h: &vibe_env::Handle,
        r: &vibe_env::Run,
    ) -> vibe_env::Result<vibe_env::RunOutput> {
        self.0.run(h, r)
    }
    fn launch(
        &self,
        h: &vibe_env::Handle,
        l: &vibe_env::Launch,
    ) -> vibe_env::Result<vibe_env::TerminalEnd> {
        self.0.launch(h, l)
    }
    fn processes(&self, h: &vibe_env::Handle) -> vibe_env::Result<Vec<vibe_env::Proc>> {
        self.0.processes(h)
    }
    fn stop(&self, h: &vibe_env::Handle) -> vibe_env::Result<()> {
        self.0.stop(h)
    }
    fn delete(&self, h: &vibe_env::Handle) -> vibe_env::Result<()> {
        self.0.delete(h)
    }
}

impl vibe_env::Limiter for ApplyFails {
    fn apply(&self, _: &vibe_env::Handle, _: &vibe_env::Limits) -> vibe_env::Result<()> {
        Err(vibe_env::EnvError::new("no cgroup for you"))
    }
    fn usage(&self, h: &vibe_env::Handle) -> vibe_env::Result<vibe_env::Usage> {
        self.0.usage(h)
    }
}

impl vibe_env::Sleeper for ApplyFails {
    fn sleep(&self, h: &vibe_env::Handle) -> vibe_env::Result<()> {
        self.0.sleep(h)
    }
    fn wake(&self, h: &vibe_env::Handle) -> vibe_env::Result<vibe_env::Woke> {
        self.0.wake(h)
    }
}

impl vibe_env::Backend for ApplyFails {
    fn capabilities(&self) -> vibe_env::Capabilities {
        self.0.capabilities()
    }
}

/// Pids of every process whose command line mentions `text`.
fn pids_mentioning(text: &str) -> Vec<String> {
    let out = std::process::Command::new("pgrep")
        .args(["-f", text])
        .output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn a_start_that_fails_after_its_shim_runs_ends_the_shim_too() {
    let w = world().await;
    // The world's own stand-in, under a backend whose apply fails.
    let backend = Arc::new(ApplyFails(vibe_env::testing::TestBackend::new(
        &w.golden.claude,
    )));
    let w = World {
        core: {
            let mut config = w.golden.config();
            config.backends = Backends::all(backend);
            drop(w.core);
            Arc::new(Core::open(config).unwrap())
        },
        ..w
    };
    let before = w.agent_files();
    let mut p = start("question");
    p.name = Some("doomed-after-launch".into());
    let err = w.core.start_agent(p).await.unwrap_err();
    assert_eq!(code(&err), ErrorCode::CommandFailed);
    assert_eq!(err.message, "no cgroup for you");
    // Its shim and the stand-in under it are gone, with everything else.
    wait_until("the shim to end", || {
        pids_mentioning("doomed-after-launch").is_empty()
    });
    assert_eq!(w.agent_files(), before);
    assert!(w.list().await.is_empty());
}

#[tokio::test]
async fn a_shim_that_was_killed_leaves_nothing_after_a_delete() {
    let w = world().await;
    let before = w.agent_files();
    let a = w.core.start_agent(start("question")).await.unwrap();
    w.wait_screen(&a.agent, "Red or blue?");
    let tree = w.session(&a.agent);
    // SIGKILL leaves its socket and lock behind.
    unsafe { libc::kill(tree.sid as i32, libc::SIGKILL) };
    wait_until("the session to end", || tree.is_empty());
    assert!(w.layout.session_socket(&a.agent).exists());
    w.core.delete_agent(delete(&a.agent, false)).await.unwrap();
    assert_eq!(w.agent_files(), before);
}

#[tokio::test]
async fn untracked_files_count_even_when_the_repo_hides_them() {
    let w = world().await;
    git(&[
        "-C",
        w.project.to_str().unwrap(),
        "config",
        "status.showUntrackedFiles",
        "no",
    ]);
    let a = w.core.start_agent(start("dirty")).await.unwrap();
    w.wait_screen(&a.agent, "Left notes.txt.");
    let err = w
        .core
        .delete_agent(delete(&a.agent, false))
        .await
        .unwrap_err();
    assert!(err.message.contains("1 uncommitted change"), "{err}");
    w.core.delete_agent(delete(&a.agent, true)).await.unwrap();
}

#[tokio::test]
async fn starts_at_once_dont_share_the_last_room() {
    let w = world().await;
    w.core
        .set_limits(vibe_core::rpc::LimitsSetParams {
            max_awake_agents: Some(2),
            ..Default::default()
        })
        .unwrap();
    // Room for two awake agents; three start at once, and none is idle to
    // make room.
    let (a, b, c) = tokio::join!(
        w.core.start_agent(start("one")),
        w.core.start_agent(start("two")),
        w.core.start_agent(start("three")),
    );
    let results = [a, b, c];
    let started: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    let refused: Vec<_> = results.iter().filter_map(|r| r.as_ref().err()).collect();
    assert_eq!(started.len(), 2, "{refused:?}");
    assert_eq!(refused.len(), 1);
    assert!(
        refused[0].message.contains("(max_awake_agents)"),
        "{:?}",
        refused[0]
    );
    for a in started {
        w.core.delete_agent(delete(&a.agent, true)).await.unwrap();
    }
}

#[tokio::test]
async fn a_session_pid_reused_by_someone_else_is_left_alone() {
    let w = world().await;
    let a = w.core.start_agent(start("hello")).await.unwrap();
    w.wait_screen(&a.agent, "Working on: hello");
    w.session(&a.agent).end(Duration::from_secs(5));
    // Someone else's session, under the pid the agent's record names, as
    // after a reboot.
    let mut other = {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("60");
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        cmd.spawn().unwrap()
    };
    {
        let mut store = vibe_core::store::Store::open(&w.layout.db()).unwrap();
        let record = store.agent("local", &a.agent).unwrap();
        let mut handle: vibe_env::Handle = serde_json::from_str(&record.compartment).unwrap();
        handle.session = Some(other.id());
        store
            .set_agent_compartment("local", &a.agent, &serde_json::to_string(&handle).unwrap())
            .unwrap();
    }
    let back = w.core.resume_agent(None, &a.agent).await.unwrap();
    assert_eq!(back.state, AgentState::Busy);
    w.wait_screen(&a.agent, "Resumed");
    assert!(common::alive(other.id() as i32), "a resume killed it");
    // The new session is the agent's own: its delete leaves the other be.
    w.core.delete_agent(delete(&a.agent, false)).await.unwrap();
    assert!(common::alive(other.id() as i32), "a delete killed it");
    other.kill().unwrap();
    other.wait().unwrap();
}

#[tokio::test]
async fn a_restarted_vibed_holds_agents_to_their_limits_again() {
    let w = world().await;
    let a = w.core.start_agent(start("fork 3")).await.unwrap();
    w.wait_screen(&a.agent, "Ready.");
    let golden = &w.golden;
    drop(w.core);
    let backend = Arc::new(vibe_env::testing::TestBackend::new(&golden.claude));
    let mut config = golden.config();
    config.backends = Backends::all(backend.clone());
    let core = Arc::new(Core::open(config).unwrap());
    // Lower than what it runs, so holding it to it pauses it.
    core.set_limits(vibe_core::rpc::LimitsSetParams {
        max_processes: Some(2),
        ..Default::default()
    })
    .unwrap();
    core.adopt_sessions().await;
    let record = vibe_core::store::Store::open(&w.layout.db())
        .unwrap()
        .agent("local", &a.agent)
        .unwrap();
    let handle: vibe_env::Handle = serde_json::from_str(&record.compartment).unwrap();
    wait_until("a pause", || {
        vibe_env::Limiter::usage(&*backend, &handle).unwrap().paused
            == Some(vibe_env::Limit::Processes)
    });
    core.delete_agent(delete(&a.agent, false)).await.unwrap();
}

#[tokio::test]
async fn from_a_local_branch_behind_origin_takes_origin() {
    let w = world().await;
    let origin = w.origin.to_str().unwrap();
    let base = git_out(&w.origin, &["rev-parse", "HEAD"]);
    git(&["-C", origin, "checkout", "-q", "-b", "feature"]);
    git(&["-C", origin, "commit", "-q", "--allow-empty", "-m", "ahead"]);
    let ahead = git_out(&w.origin, &["rev-parse", "HEAD"]);
    git(&["-C", origin, "checkout", "-q", "-"]);
    git(&[
        "-C",
        w.project.to_str().unwrap(),
        "branch",
        "feature",
        &base,
    ]);
    let mut p = start("x");
    p.from = Some("feature".into());
    let a = w.core.start_agent(p).await.unwrap();
    assert_eq!(
        git_out(Path::new(&a.worktree), &["rev-parse", "HEAD"]),
        ahead
    );
    w.core.delete_agent(delete(&a.agent, true)).await.unwrap();
}

#[tokio::test]
async fn a_forced_delete_works_without_a_backend() {
    let w = world().await;
    let a = w.core.start_agent(start("question")).await.unwrap();
    w.wait_screen(&a.agent, "Red or blue?");
    let tree = w.session(&a.agent);
    let golden = &w.golden;
    drop(w.core);
    let mut config = golden.config();
    config.backends = Backends::new();
    let core = Arc::new(Core::open(config).unwrap());
    let err = core
        .delete_agent(delete(&a.agent, false))
        .await
        .unwrap_err();
    assert_eq!(err.message, "this host can't run linux agents yet");
    core.delete_agent(delete(&a.agent, true)).await.unwrap();
    assert!(tree.is_empty());
    assert!(!Path::new(&a.worktree).exists());
    assert!(!w.layout.agent_dir(&a.agent).exists());
}

#[tokio::test]
async fn a_start_cut_short_by_a_crash_is_undone_by_the_next_vibed() {
    let w = world().await;
    let origin = w.origin.to_str().unwrap();
    git(&["-C", origin, "checkout", "-q", "-b", "slow-setup"]);
    fs::create_dir_all(w.origin.join(".vibe")).unwrap();
    // It says when it's running and when it's done, and goes on until it's told to. Or
    // until this test has gone, by a failure that removed `marks` or by being killed,
    // so it can neither hold up a failure's report nor outlive the test for long.
    let marks = tempfile::tempdir().unwrap();
    let [started, finish, finished] =
        ["started", "finish", "finished"].map(|name| marks.path().join(name));
    fs::write(
        w.origin.join(".vibe/setup.sh"),
        format!(
            "touch '{started}'\n\
             i=0\n\
             while [ ! -e '{finish}' ] && [ -d '{marks}' ] && [ $i -lt 6000 ]; do\n\
             sleep 0.05\n\
             i=$((i+1))\n\
             done\n\
             touch '{finished}'\n",
            started = started.display(),
            finish = finish.display(),
            marks = marks.path().display(),
            finished = finished.display()
        ),
    )
    .unwrap();
    git(&["-C", origin, "add", "."]);
    git(&["-C", origin, "commit", "-q", "-m", "slow"]);
    git(&["-C", origin, "checkout", "-q", "-"]);
    let before = w.agent_files();
    let branches = git_out(&w.project, &["branch", "--list"]);

    // vibed dies during the setup: the start's future just stops, with no
    // undo run, as a crash would leave it.
    let mut p = start("x");
    p.from = Some("slow-setup".into());
    p.name = Some("cut-short".into());
    tokio::select! {
        done = w.core.start_agent(p) => panic!("the start finished: {done:?}"),
        () = file_appears(&started) => {}
    }
    assert!(w.layout.worktree("app", "cut-short").exists());
    let listed = w.list().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].agent, "cut-short");
    drop(w.core);
    // Let the orphaned setup finish, as it would have died with vibed.
    fs::write(&finish, "").unwrap();
    file_appears(&finished).await;

    let core = Arc::new(Core::open(w.golden.config()).unwrap());
    let w = World { core, ..w };
    w.core.adopt_sessions().await;
    assert!(w.list().await.is_empty());
    assert_eq!(w.agent_files(), before, "everything it made is gone");
    assert_eq!(git_out(&w.project, &["branch", "--list"]), branches);
    let worktrees = git_out(&w.project, &["worktree", "list"]);
    assert!(!worktrees.contains("cut-short"), "{worktrees}");
}

/// Waits for what a script says by making `path`, however busy this machine is.
async fn file_appears(path: &Path) {
    for _ in 0..6000 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{} never appeared", path.display());
}

/// The test backend, but whose agents go on without their shim, as a
/// `linux` agent goes on in its VM: a wake asks for a new shim on the
/// terminal it had.
#[derive(Debug)]
struct Reattaches(vibe_env::testing::TestBackend);

impl vibe_env::Compartment for Reattaches {
    fn create(&self, spec: &vibe_env::AgentSpec) -> vibe_env::Result<vibe_env::Handle> {
        self.0.create(spec)
    }
    fn run(
        &self,
        h: &vibe_env::Handle,
        r: &vibe_env::Run,
    ) -> vibe_env::Result<vibe_env::RunOutput> {
        self.0.run(h, r)
    }
    fn launch(
        &self,
        h: &vibe_env::Handle,
        l: &vibe_env::Launch,
    ) -> vibe_env::Result<vibe_env::TerminalEnd> {
        self.0.launch(h, l)
    }
    fn processes(&self, h: &vibe_env::Handle) -> vibe_env::Result<Vec<vibe_env::Proc>> {
        self.0.processes(h)
    }
    fn stop(&self, h: &vibe_env::Handle) -> vibe_env::Result<()> {
        self.0.stop(h)
    }
    fn delete(&self, h: &vibe_env::Handle) -> vibe_env::Result<()> {
        self.0.delete(h)
    }
}

impl vibe_env::Limiter for Reattaches {
    fn apply(&self, h: &vibe_env::Handle, l: &vibe_env::Limits) -> vibe_env::Result<()> {
        self.0.apply(h, l)
    }
    fn usage(&self, h: &vibe_env::Handle) -> vibe_env::Result<vibe_env::Usage> {
        self.0.usage(h)
    }
}

impl vibe_env::Sleeper for Reattaches {
    fn sleep(&self, h: &vibe_env::Handle) -> vibe_env::Result<()> {
        self.0.sleep(h)
    }
    fn wake(&self, h: &vibe_env::Handle) -> vibe_env::Result<vibe_env::Woke> {
        Ok(vibe_env::Woke::Reattach(vibe_env::TerminalEnd::Pty {
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo 'Where it was.'; exec cat".into(),
            ],
            clean_env: false,
            cwd: h.worktree.clone(),
            env: vec![],
        }))
    }
}

impl vibe_env::Backend for Reattaches {
    fn capabilities(&self) -> vibe_env::Capabilities {
        self.0.capabilities()
    }
}

#[tokio::test]
async fn a_wake_that_reattaches_starts_a_shim_and_no_new_claude() {
    let w = world().await;
    let backend = Arc::new(Reattaches(vibe_env::testing::TestBackend::new(
        &w.golden.claude,
    )));
    let w = World {
        core: {
            let mut config = w.golden.config();
            config.backends = Backends::all(backend);
            drop(w.core);
            Arc::new(Core::open(config).unwrap())
        },
        ..w
    };
    let a = w.core.start_agent(start("hello")).await.unwrap();
    w.wait_screen(&a.agent, "Working on: hello");
    let old = w.session(&a.agent);
    unsafe { libc::kill(old.sid as i32, libc::SIGKILL) };
    wait_until("the shim to go", || old.is_empty());
    let back = w.core.resume_agent(None, &a.agent).await.unwrap();
    assert_eq!(back.agent, a.agent);
    w.wait_screen(&a.agent, "Where it was.");
    assert!(
        !w.screen(&a.agent).contains("Resumed"),
        "Claude was launched again"
    );
    let new = w.session(&a.agent);
    assert_ne!(new.sid, old.sid);
    // The new shim is the agent's session now: a delete ends it.
    w.core.delete_agent(delete(&a.agent, true)).await.unwrap();
    wait_until("the new shim to go", || new.is_empty());
}

#[tokio::test]
async fn a_limit_hit_makes_the_agent_paused_and_says_so() {
    // A backend of its own, kept to wake the agent with.
    let bin = common::short_tempdir();
    let backend = Arc::new(vibe_env::testing::TestBackend::new(common::stand_in(
        bin.path(),
    )));
    let w = {
        let backend = Arc::clone(&backend);
        world_with(move |c| {
            c.backends = Backends::all(backend.clone());
        })
        .await
    };
    // The stand-in's twenty sleeps against a limit of eight.
    w.core
        .set_limits(vibe_core::rpc::LimitsSetParams {
            max_processes: Some(8),
            ..Default::default()
        })
        .unwrap();
    let (_, mut events) = w.core.subscribe();
    let a = w.core.start_agent(start("fork 20")).await.unwrap();
    let watching = w.core.spawn_limit_watch(Duration::from_millis(100));
    let mut next = async |what: &str, ok: &dyn Fn(&vibe_core::rpc::Event) -> bool| {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let e = events.recv().await.unwrap();
                if e.agent.as_deref() == Some(a.agent.as_str()) && ok(&e) {
                    return e;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {what} event"))
    };
    let e = next("agent.limit", &|e| e.kind == EventKind::AgentLimit).await;
    assert_eq!(e.limit.as_deref(), Some("processes"));
    assert_eq!(e.project.as_deref(), Some("app"));
    let message = e.message.unwrap();
    assert!(
        message.starts_with("paused with ") && message.ends_with("over its limit of 8"),
        "{message}"
    );
    next("paused", &|e| {
        e.kind == EventKind::AgentState && e.state.as_deref() == Some("paused")
    })
    .await;
    let listed = w.list().await;
    assert_eq!(listed[0].state, AgentState::Paused);

    // Raised and woken, it's what its transcript says again, and not
    // announced twice.
    let record = vibe_core::store::Store::open(&w.layout.db())
        .unwrap()
        .agent("local", &a.agent)
        .unwrap();
    let handle: vibe_env::Handle = serde_json::from_str(&record.compartment).unwrap();
    use vibe_env::{Limiter, Sleeper};
    backend
        .apply(
            &handle,
            &vibe_env::Limits::new(GIB, &vibe_core::limits::HostLimits::default()),
        )
        .unwrap();
    backend.wake(&handle).unwrap();
    let e = next("a state after the pause", &|e| {
        e.kind == EventKind::AgentLimit || e.kind == EventKind::AgentState
    })
    .await;
    assert_eq!(e.kind, EventKind::AgentState, "{e:?}");
    assert_ne!(e.state.as_deref(), Some("paused"));
    assert_ne!(w.list().await[0].state, AgentState::Paused);
    watching.abort();
    w.core.delete_agent(delete(&a.agent, true)).await.unwrap();
}

#[tokio::test]
async fn host_limits_edited_while_an_agent_runs_hold_it_at_once() {
    let w = world().await;
    let a = w.core.start_agent(start("fork 3")).await.unwrap();
    w.wait_screen(&a.agent, "Ready.");
    let watching = w.core.spawn_limit_watch(Duration::from_millis(100));
    // Room enough: it runs on.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_ne!(w.list().await[0].state, AgentState::Paused);
    w.core
        .set_limits(vibe_core::rpc::LimitsSetParams {
            max_processes: Some(2),
            ..Default::default()
        })
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while w.list().await[0].state != AgentState::Paused {
        assert!(std::time::Instant::now() < deadline, "never paused");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    watching.abort();
    w.core.delete_agent(delete(&a.agent, true)).await.unwrap();
}

#[tokio::test]
async fn a_macos_agent_runs_on_the_governor_in_the_owners_own_claude_home() {
    let bin = common::short_tempdir();
    let claude = common::stand_in(bin.path());
    let w = world_with(move |c| {
        let backend = vibe_env::macos::Macos::new(c.layout.home())
            .with_claude(&claude)
            .with_path(std::env::var("PATH").unwrap());
        c.backends = Backends::new().with(vibe_core::model::Environment::Macos, Arc::new(backend));
    })
    .await;
    let a = w
        .core
        .start_agent(AgentStartParams {
            profile: "software-eng-mac".into(),
            ..start("fork 3 question")
        })
        .await
        .unwrap();
    assert_eq!(a.environment, vibe_core::model::Environment::Macos);
    w.wait_screen(&a.agent, "Red or blue?");
    // Its transcript is in the owner's ~/.claude, and it has no Claude
    // home of its own.
    let claude_home = ClaudeHome::user(&w.golden.home);
    let worktree = PathBuf::from(&a.worktree);
    let transcript = claude_home.transcript(&worktree, &a.session_id);
    wait_until("the transcript", || transcript.is_file());
    assert!(!w.layout.agent_dir(&a.agent).join("claude").exists());
    let state = fs::read_to_string(&claude_home.state).unwrap();
    assert!(
        state.contains(&a.worktree),
        "the worktree is trusted: {state}"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while w.list().await[0].state != AgentState::Question {
        assert!(std::time::Instant::now() < deadline, "never a question");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // An edited limit pauses it, and says so.
    let (_, mut events) = w.core.subscribe();
    let watching = w.core.spawn_limit_watch(Duration::from_millis(100));
    w.core
        .set_limits(vibe_core::rpc::LimitsSetParams {
            max_processes: Some(2),
            ..Default::default()
        })
        .unwrap();
    let e = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let e = events.recv().await.unwrap();
            if e.kind == EventKind::AgentLimit {
                return e;
            }
        }
    })
    .await
    .expect("an agent.limit event");
    assert_eq!(e.limit.as_deref(), Some("processes"));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while w.list().await[0].state != AgentState::Paused {
        assert!(std::time::Instant::now() < deadline, "never paused");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        w.layout
            .agent_dir(&a.agent)
            .join(vibe_env::macos::STATE_FILE)
            .is_file()
    );
    // Resumed while still over its limit, it's paused again.
    w.core.resume_agent(None, &a.agent).await.unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while w.list().await[0].state != AgentState::Paused {
        assert!(std::time::Instant::now() < deadline, "never paused again");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Given room, a resume brings it back where it was, under the new
    // limits, and it stays back.
    w.core
        .set_limits(vibe_core::rpc::LimitsSetParams {
            max_processes: Some(100),
            ..Default::default()
        })
        .unwrap();
    let resumed = w.core.resume_agent(None, &a.agent).await.unwrap();
    assert_eq!(resumed.state, AgentState::Question, "{resumed:?}");
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(w.list().await[0].state, AgentState::Question);
    w.type_line(&a.agent, "red");
    w.wait_screen(&a.agent, "You chose red.");
    watching.abort();

    // A delete takes its transcripts and its trust from the owner's
    // Claude state, and leaves the rest of it.
    w.core.delete_agent(delete(&a.agent, true)).await.unwrap();
    assert!(!transcript.parent().unwrap().exists());
    let state = fs::read_to_string(&claude_home.state).unwrap();
    assert!(!state.contains(&a.worktree), "{state}");
    assert!(state.contains("hasCompletedOnboarding"), "{state}");
    assert!(w.agent_files().is_empty(), "{:?}", w.agent_files());
}

/// Memory pressure as a test sets it.
#[derive(Debug, Default)]
struct Pressure(std::sync::Mutex<Option<vibed::pressure::Reading>>);

impl Pressure {
    fn set(&self, some10: f64) {
        *self.0.lock().unwrap() = Some(
            vibed::pressure::Psi {
                some10,
                full10: 0.0,
            }
            .into(),
        );
    }
}

impl vibed::pressure::MemoryPressure for Pressure {
    fn read(&self) -> Option<vibed::pressure::Reading> {
        *self.0.lock().unwrap()
    }
}

/// The test backend, held to its limits by "the kernel", as a Linux
/// host's cgroups are.
fn kernel_limited(claude: &Path) -> vibe_env::testing::TestBackend {
    let b = vibe_env::testing::TestBackend::new(claude);
    let caps = vibe_env::Capabilities {
        kernel_limits: true,
        ..vibe_env::Backend::capabilities(&b)
    };
    b.with_capabilities(caps)
}

#[tokio::test]
async fn memory_pressure_warns_then_ends_frozen_agents_oldest_first() {
    use std::time::Instant;
    use vibe_core::model::Environment;
    use vibed::pressure::{END_AFTER, END_EVERY, PressureWatch};
    let pressure = Arc::new(Pressure::default());
    let bin = common::short_tempdir();
    let claude = common::stand_in(bin.path());
    let w = {
        let pressure = Arc::clone(&pressure);
        world_with(move |c| {
            c.pressure = pressure;
            // linux agents here sleep the way VMs do, giving their memory
            // back; macos agents keep theirs, held by the kernel as a Linux
            // host's are.
            c.backends = Backends::new()
                .with(
                    Environment::Linux,
                    Arc::new(
                        vibe_env::testing::TestBackend::new(&claude).sleeping_to_disk(1 << 20),
                    ),
                )
                .with(Environment::Macos, Arc::new(kernel_limited(&claude)));
        })
        .await
    };
    let mac = |prompt: &str| AgentStartParams {
        profile: "software-eng-mac".into(),
        ..start(prompt)
    };
    let first = w.core.start_agent(mac("question")).await.unwrap();
    let second = w.core.start_agent(mac("question")).await.unwrap();
    let saved = w.core.start_agent(start("question")).await.unwrap();
    for a in [&first, &second, &saved] {
        w.wait_screen(&a.agent, "Red or blue?");
    }
    // Asleep in this order; the first was asleep longest.
    for a in [&saved, &first, &second] {
        let asleep = w.core.sleep_agent(None, &a.agent).await.unwrap();
        assert_eq!(asleep.state, AgentState::Asleep);
    }
    let awake = w.core.start_agent(mac("hello")).await.unwrap();
    w.wait_screen(&awake.agent, "Working on: hello");

    // Short of memory, a start still goes ahead (#47): the host warns of
    // it, and says why.
    pressure.set(45.0);
    let more = w.core.start_agent(mac("more")).await.unwrap();
    w.wait_screen(&more.agent, "Working on: more");
    let local = w.core.hosts().unwrap().hosts.remove(0);
    let warned = local.memory.and_then(|m| m.pressure).unwrap_or_default();
    assert!(warned.contains("45% of the last 10 s"), "{warned}");
    // Once it has lasted, the frozen agent asleep longest is ended, then
    // one more per interval while it lasts. One whose sleep gave its memory
    // back, and one awake, are left as they are.
    let mut watch = PressureWatch::default();
    let t0 = Instant::now();
    assert_eq!(w.core.check_pressure(&mut watch, t0).await, None);
    let t = t0 + END_AFTER;
    assert_eq!(
        w.core.check_pressure(&mut watch, t).await,
        Some(first.agent.clone())
    );
    assert!(w.runs(&second.agent));
    assert_eq!(
        w.core.check_pressure(&mut watch, t + END_EVERY / 2).await,
        None
    );
    assert_eq!(
        w.core.check_pressure(&mut watch, t + END_EVERY).await,
        Some(second.agent.clone())
    );
    assert_eq!(
        w.core.check_pressure(&mut watch, t + END_EVERY * 2).await,
        None,
        "nothing frozen is left to end"
    );
    let by_ref = |list: &[Agent], r: &str| list.iter().find(|a| a.agent == r).unwrap().state;
    let list = w.list().await;
    assert_eq!(by_ref(&list, &first.agent), AgentState::Asleep);
    assert_eq!(by_ref(&list, &second.agent), AgentState::Asleep);
    assert_eq!(by_ref(&list, &saved.agent), AgentState::Asleep);
    assert_ne!(by_ref(&list, &awake.agent), AgentState::Asleep);
    assert!(!w.runs(&first.agent));
    assert!(w.runs(&saved.agent));
    assert!(w.runs(&awake.agent));
    assert!(w.runs(&more.agent));

    // Eased, the warning goes, and a pressure that comes back waits
    // its time before anything more is ended.
    *pressure.0.lock().unwrap() = None;
    assert_eq!(
        w.core.check_pressure(&mut watch, t + END_EVERY * 3).await,
        None
    );
    pressure.set(45.0);
    let t1 = t + END_EVERY * 4;
    assert_eq!(w.core.check_pressure(&mut watch, t1).await, None);
    *pressure.0.lock().unwrap() = None;
    let later = w.core.start_agent(mac("later")).await.unwrap();
    w.wait_screen(&later.agent, "Working on: later");

    // An ended agent resumes its session; its question went with it.
    let back = w.core.resume_agent(None, &first.agent).await.unwrap();
    assert_eq!(back.session_id, first.session_id);
    w.wait_screen(&first.agent, &format!("Resumed {}.", first.session_id));
    let local = w.core.hosts().unwrap().hosts.remove(0);
    assert_eq!(local.memory.and_then(|m| m.pressure), None);
    for a in [&first, &second, &saved, &awake, &more, &later] {
        w.core.delete_agent(delete(&a.agent, true)).await.unwrap();
    }
    assert!(w.agent_files().is_empty(), "{:?}", w.agent_files());
}

#[tokio::test]
async fn sleep_needs_a_running_agent() {
    let w = world().await;
    let a = w.core.start_agent(start("question")).await.unwrap();
    w.wait_screen(&a.agent, "Red or blue?");
    w.session(&a.agent).end(Duration::from_secs(5));
    let err = w.core.sleep_agent(None, &a.agent).await.unwrap_err();
    assert_eq!(code(&err), ErrorCode::Conflict);
    assert!(err.message.contains("isn't running"), "{}", err.message);
    let err = w.core.sleep_agent(None, "no-such-agent").await.unwrap_err();
    assert_eq!(code(&err), ErrorCode::NotFound);
    w.core.delete_agent(delete(&a.agent, true)).await.unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn after_a_reboot_every_agent_is_asleep_and_an_attach_resumes_it() {
    let w = world().await;
    let a = w.core.start_agent(start("question")).await.unwrap();
    w.wait_screen(&a.agent, "Red or blue?");
    let golden = &w.golden;
    let data = w.layout.data_dir();
    drop(w.core);
    // A restart of vibed alone changes nothing: an agent whose session
    // ended is exited.
    let tree = SessionTree::new(
        shim::connect(
            &w.layout.session_socket(&a.agent),
            &control_hello("test"),
            Duration::from_secs(10),
        )
        .unwrap()
        .status()
        .unwrap()
        .pid,
    );
    tree.end(Duration::from_secs(5));
    let core = Arc::new(Core::open(golden.config()).unwrap());
    core.adopt_sessions().await;
    let state = |core: &Arc<Core>| {
        let core = Arc::clone(core);
        async move { core.agents(HostParams::default()).await.unwrap().agents[0].state }
    };
    assert_eq!(state(&core).await, AgentState::Exited);
    drop(core);
    // After a reboot, a new boot ID, it's asleep.
    fs::write(
        data.join(vibed::agents::BOOT_FILE),
        "00000000-0000-0000-0000-000000000000\n",
    )
    .unwrap();
    let core = Arc::new(Core::open(golden.config()).unwrap());
    core.adopt_sessions().await;
    assert_eq!(state(&core).await, AgentState::Asleep);
    // And an attach resumes it with `claude --resume`.
    let attach = core
        .attach_session(vibe_core::rpc::SessionParams {
            host: None,
            agent: a.agent.clone(),
        })
        .await
        .unwrap();
    assert_eq!(
        attach.socket,
        w.layout.session_socket(&a.agent).to_string_lossy()
    );
    wait_screen_of(&w.layout, &a.agent, &format!("Resumed {}.", a.session_id));
    assert_ne!(state(&core).await, AgentState::Asleep);
    core.delete_agent(delete(&a.agent, true)).await.unwrap();
}

/// The Linux host's backend, with every agent's memory kept to tens of MB
/// whatever its profile says (profiles start at 1 GB).
#[derive(Debug)]
struct Tiny(vibe_env::cgroups::Cgroups);

const TINY: u64 = 48 << 20;

fn tiny(l: &vibe_env::Limits) -> vibe_env::Limits {
    vibe_env::Limits {
        memory: l.memory.min(TINY),
        ..*l
    }
}

impl vibe_env::Compartment for Tiny {
    fn create(&self, spec: &vibe_env::AgentSpec) -> vibe_env::Result<vibe_env::Handle> {
        self.0.create(&vibe_env::AgentSpec {
            agent: Default::default(),
            limits: tiny(&spec.limits),
            ..spec.clone()
        })
    }
    fn run(
        &self,
        h: &vibe_env::Handle,
        r: &vibe_env::Run,
    ) -> vibe_env::Result<vibe_env::RunOutput> {
        self.0.run(h, r)
    }
    fn launch(
        &self,
        h: &vibe_env::Handle,
        l: &vibe_env::Launch,
    ) -> vibe_env::Result<vibe_env::TerminalEnd> {
        self.0.launch(h, l)
    }
    fn processes(&self, h: &vibe_env::Handle) -> vibe_env::Result<Vec<vibe_env::Proc>> {
        self.0.processes(h)
    }
    fn stop(&self, h: &vibe_env::Handle) -> vibe_env::Result<()> {
        self.0.stop(h)
    }
    fn delete(&self, h: &vibe_env::Handle) -> vibe_env::Result<()> {
        self.0.delete(h)
    }
}

impl vibe_env::Limiter for Tiny {
    fn apply(&self, h: &vibe_env::Handle, l: &vibe_env::Limits) -> vibe_env::Result<()> {
        self.0.apply(h, &tiny(l))
    }
    fn usage(&self, h: &vibe_env::Handle) -> vibe_env::Result<vibe_env::Usage> {
        self.0.usage(h)
    }
}

impl vibe_env::Sleeper for Tiny {
    fn sleep(&self, h: &vibe_env::Handle) -> vibe_env::Result<()> {
        self.0.sleep(h)
    }
    fn wake(&self, h: &vibe_env::Handle) -> vibe_env::Result<vibe_env::Woke> {
        self.0.wake(h)
    }
}

impl vibe_env::Backend for Tiny {
    fn capabilities(&self) -> vibe_env::Capabilities {
        self.0.capabilities()
    }
}

/// Stops a test's slice, and whatever a failed test left in it.
struct StopSlice(String);

impl Drop for StopSlice {
    fn drop(&mut self) {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "stop", &self.0])
            .status();
    }
}

#[tokio::test]
async fn a_linux_agent_on_a_linux_host_runs_in_a_cgroup_of_its_own() {
    let must = std::env::var_os("VIBE_TEST_CGROUPS").is_some_and(|v| !v.is_empty());
    let bin = common::short_tempdir();
    let claude = common::stand_in(bin.path());
    let slice = format!("vibedtest{}.slice", std::process::id());
    let _stop = StopSlice(slice.clone());
    let probe = vibe_env::cgroups::Cgroups::new("/").with_slice(slice.clone());
    if !cfg!(target_os = "linux") || probe.check().is_err() {
        assert!(!must, "VIBE_TEST_CGROUPS is set, but: {:?}", probe.check());
        eprintln!("no cgroups here to test a Linux host's agents on");
        return;
    }
    let w = {
        let slice = slice.clone();
        world_with(move |c| {
            let backend = vibe_env::cgroups::Cgroups::new(c.layout.home())
                .with_claude(&claude)
                .with_path(std::env::var("PATH").unwrap())
                .with_slice(slice);
            c.backends = Backends::new().with(
                vibe_core::model::Environment::Linux,
                Arc::new(Tiny(backend)),
            );
        })
        .await
    };
    let a = w.core.start_agent(start("fork 3 question")).await.unwrap();
    assert_eq!(a.environment, vibe_core::model::Environment::Linux);
    w.wait_screen(&a.agent, "Red or blue?");
    // It's in its own cgroup, held to its limits by the kernel.
    let cgroup = vibe_env::cgroups::Cgroups::new("/")
        .with_slice(slice.clone())
        .cgroup(&a.agent)
        .unwrap();
    assert!(cgroup.dir.ends_with(format!(
        "{slice}/vibedtest{}-{}.scope",
        std::process::id(),
        a.agent
    )));
    assert_eq!(
        fs::read_to_string(cgroup.dir.join("memory.max"))
            .unwrap()
            .trim(),
        TINY.to_string()
    );
    assert_eq!(
        fs::read_to_string(cgroup.dir.join("pids.max"))
            .unwrap()
            .trim(),
        "4096",
        "512 processes, with room for their threads"
    );
    assert!(!cgroup.pids().is_empty());
    // Its transcript is in the server's own ~/.claude.
    let claude_home = ClaudeHome::user(&w.golden.home);
    let worktree = PathBuf::from(&a.worktree);
    let transcript = claude_home.transcript(&worktree, &a.session_id);
    wait_until("the transcript", || transcript.is_file());
    assert!(!w.layout.agent_dir(&a.agent).join("claude").exists());
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while w.list().await[0].state != AgentState::Question {
        assert!(std::time::Instant::now() < deadline, "never a question");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // A max_processes edited in host.toml is the kernel's at once; under
    // it, a fork is refused and the agent paused, and it says so.
    let (_, mut events) = w.core.subscribe();
    let watching = w.core.spawn_limit_watch(Duration::from_millis(100));
    w.core
        .set_limits(vibe_core::rpc::LimitsSetParams {
            max_processes: Some(2),
            ..Default::default()
        })
        .unwrap();
    let e = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let e = events.recv().await.unwrap();
            if e.kind == EventKind::AgentLimit {
                return e;
            }
        }
    })
    .await
    .expect("an agent.limit event");
    assert_eq!(e.limit.as_deref(), Some("processes"));
    assert_eq!(
        fs::read_to_string(cgroup.dir.join("pids.max"))
            .unwrap()
            .trim(),
        "16"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while w.list().await[0].state != AgentState::Paused {
        assert!(std::time::Instant::now() < deadline, "never paused");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(cgroup.frozen());
    w.core
        .set_limits(vibe_core::rpc::LimitsSetParams {
            max_processes: Some(100),
            ..Default::default()
        })
        .unwrap();
    let resumed = w.core.resume_agent(None, &a.agent).await.unwrap();
    assert_eq!(resumed.state, AgentState::Question, "{resumed:?}");
    assert!(!cgroup.frozen());
    watching.abort();

    // Asleep, it's frozen whole; woken, it's exactly where it was.
    let asleep = w.core.sleep_agent(None, &a.agent).await.unwrap();
    assert_eq!(asleep.state, AgentState::Asleep);
    assert!(cgroup.frozen());
    w.core.resume_agent(None, &a.agent).await.unwrap();
    assert!(!cgroup.frozen());
    w.type_line(&a.agent, "red");
    w.wait_screen(&a.agent, "You chose red.");

    // A delete leaves nothing of it: no scope, no transcript, no trust.
    w.core.delete_agent(delete(&a.agent, true)).await.unwrap();
    assert!(!cgroup.dir.exists());
    assert!(!transcript.parent().unwrap().exists());
    let state = fs::read_to_string(&claude_home.state).unwrap();
    assert!(!state.contains(&a.worktree), "{state}");
    assert!(w.agent_files().is_empty(), "{:?}", w.agent_files());
}

#[tokio::test]
async fn memory_pressure_ends_an_agent_a_limit_froze_too() {
    use std::time::Instant;
    use vibed::pressure::{END_AFTER, PressureWatch};
    let pressure = Arc::new(Pressure::default());
    let bin = common::short_tempdir();
    let claude = common::stand_in(bin.path());
    let w = {
        let pressure = Arc::clone(&pressure);
        world_with(move |c| {
            c.pressure = pressure;
            c.backends = Backends::all(Arc::new(kernel_limited(&claude)));
        })
        .await
    };
    let a = w.core.start_agent(start("fork 3 question")).await.unwrap();
    w.wait_screen(&a.agent, "Red or blue?");
    let watching = w.core.spawn_limit_watch(Duration::from_millis(100));
    w.core
        .set_limits(vibe_core::rpc::LimitsSetParams {
            max_processes: Some(2),
            ..Default::default()
        })
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while w.list().await[0].state != AgentState::Paused {
        assert!(std::time::Instant::now() < deadline, "never paused");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    watching.abort();
    // Paused, it's frozen with its memory held, and sleep leaves it paused.
    w.core.sleep_agent(None, &a.agent).await.unwrap();
    assert_eq!(w.list().await[0].state, AgentState::Paused);
    pressure.set(60.0);
    let mut watch = PressureWatch::default();
    let t0 = Instant::now();
    assert_eq!(w.core.check_pressure(&mut watch, t0).await, None);
    assert_eq!(
        w.core.check_pressure(&mut watch, t0 + END_AFTER).await,
        Some(a.agent.clone())
    );
    assert!(!w.runs(&a.agent));
    // Ended, it's asleep, and a send resumes it.
    assert_eq!(w.list().await[0].state, AgentState::Asleep);
    *pressure.0.lock().unwrap() = None;
    w.core
        .set_limits(vibe_core::rpc::LimitsSetParams {
            max_processes: Some(512),
            ..Default::default()
        })
        .unwrap();
    w.core.resume_agent(None, &a.agent).await.unwrap();
    w.wait_screen(&a.agent, &format!("Resumed {}.", a.session_id));
    w.core.delete_agent(delete(&a.agent, true)).await.unwrap();
}
