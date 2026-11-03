//! The toolbox through vibed (PRD: The toolbox shell): its shell as a
//! session, saving once the shell exits, one toolbox at a time, the weekly
//! update, and what survives a restart. It runs on the test toolbox; the VM
//! itself is `vibe_env::toolbox`'s, tested on a fake there and for real on
//! a Mac (`tests/linux_vm.rs`).

mod common;

use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{short_tempdir, workspace_bin};
use vibe_core::model::{EventKind, ToolboxAction, ToolboxState};
use vibe_core::paths::Layout;
use vibe_core::rpc::{
    AgentStartParams, ErrorCode, NoParams, ProjectAddParams, SessionExit, ToolboxStatus,
};
use vibe_core::shim;
use vibe_env::testing::TestToolbox;
use vibed::core::{Config, Core, DaemonInfo};
use vibed::sessions::{self, Probe};

struct World {
    _dir: tempfile::TempDir,
    layout: Layout,
    clock: Arc<AtomicI64>,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

const DAY: i64 = 24 * 60 * 60;

fn world() -> World {
    let dir = short_tempdir();
    let home = dir.path().canonicalize().unwrap().join("home");
    let layout = Layout::new(home, std::env::consts::OS);
    let base = layout.linux_dir().join("base-1");
    fs::create_dir_all(&base).unwrap();
    for f in ["Image", "initrd", "base.img"] {
        fs::write(base.join(f), f).unwrap();
    }
    World {
        _dir: dir,
        layout,
        clock: Arc::new(AtomicI64::new(now())),
    }
}

impl World {
    fn core_with(&self, toolbox: Option<TestToolbox>) -> Arc<Core> {
        let mut config = Config::new(DaemonInfo::current(), self.layout.clone());
        config.gh = "/nonexistent/gh".into();
        config.shim = workspace_bin("vibe-shim").into_os_string();
        let clock = self.clock.clone();
        config.clock = Arc::new(move || clock.load(Ordering::SeqCst));
        config.toolbox = toolbox.map(|t| Arc::new(t) as _);
        Arc::new(Core::open(config).unwrap())
    }

    fn core(&self, shell: &[&str]) -> Arc<Core> {
        self.core_with(Some(self.toolbox(shell)))
    }

    fn toolbox(&self, shell: &[&str]) -> TestToolbox {
        TestToolbox::new(
            self.layout.clone(),
            shell.iter().map(|s| (*s).to_owned()).collect(),
        )
    }

    fn socket(&self) -> std::path::PathBuf {
        self.layout.session_socket("toolbox")
    }

    fn recipe(&self, text: &str) {
        fs::create_dir_all(self.layout.config_dir()).unwrap();
        fs::write(self.layout.toolbox_config(), text).unwrap();
    }

    fn version(&self, name: &str) -> bool {
        self.layout.linux_dir().join(name).exists()
    }

    /// A file the test makes to let a shell it holds open exit: see
    /// [`held_shell`].
    fn release(&self) -> std::path::PathBuf {
        self._dir.path().join("release")
    }

    /// A shell that runs until the test makes [`World::release`], however
    /// slow the machine is, and then exits at once (every time after).
    fn held_shell(&self) -> String {
        format!(
            "while [ ! -e '{}' ]; do sleep 0.05; done",
            self.release().display()
        )
    }

    fn advance(&self, by: i64) {
        self.clock.fetch_add(by, Ordering::SeqCst);
    }
}

async fn running(socket: &std::path::Path) -> Option<u32> {
    match sessions::probe(socket).await {
        Probe::Running(status) => Some(status.pid),
        _ => None,
    }
}

async fn wait_gone(socket: &std::path::Path) {
    for _ in 0..400 {
        if matches!(sessions::probe(socket).await, Probe::Gone) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the toolbox shell didn't end");
}

/// Waits for the toolbox's status to satisfy `ok`.
async fn wait_for_status(core: &Core, ok: impl Fn(&ToolboxStatus) -> bool) {
    for _ in 0..800 {
        if ok(&core.toolbox_status(NoParams {}).await.unwrap()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the toolbox never got there");
}

fn kill_shim(pid: u32) {
    // SAFETY: a signal to a shim this test started.
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exiting_the_shell_saves_and_the_next_one_starts_afresh() {
    let w = world();
    let shell = w.held_shell();
    let core = w.core(&["sh", "-c", &shell]);
    let (_, mut events) = core.subscribe();
    let status = core.toolbox_status(NoParams {}).await.unwrap();
    assert_eq!(status.state, ToolboxState::Closed);
    assert_eq!(
        (status.template, status.home, status.base),
        (None, None, Some(1))
    );

    let attach = core.open_toolbox(NoParams {}).await.unwrap();
    assert_eq!(attach.agent, "toolbox");
    assert_eq!(attach.title, "Linux toolbox");
    assert_eq!(attach.socket, w.socket().to_string_lossy());
    assert_eq!(
        core.toolbox_status(NoParams {}).await.unwrap().state,
        ToolboxState::Open
    );
    let e = events.recv().await.unwrap();
    assert_eq!(e.kind, EventKind::ToolboxChanged);
    assert!(e.message.unwrap().contains("is open"));
    // It's a session like any other, which `vibe attach toolbox` finds.
    let listed = core.sessions(Default::default()).await.unwrap();
    assert!(listed.sessions.iter().any(|s| s.agent == "toolbox"));

    // While the shell runs, nothing is saved: it's held open until the
    // test lets it go.
    assert!(running(&w.socket()).await.is_some());
    core.check_toolbox().await;
    assert!(!w.version("template-1.img"));
    assert!(running(&w.socket()).await.is_some(), "the shell still runs");
    fs::write(w.release(), "").unwrap();
    wait_gone(&w.socket()).await;
    core.check_toolbox().await;
    let status = core.toolbox_status(NoParams {}).await.unwrap();
    assert_eq!(status.state, ToolboxState::Closed);
    assert_eq!((status.template, status.home), (Some(1), Some(1)));
    let last = status.last.unwrap();
    assert_eq!(last.action, ToolboxAction::Shell);
    assert_eq!((last.template, last.error), (Some(1), None));
    let e = events.recv().await.unwrap();
    assert_eq!(
        e.message.as_deref(),
        Some("The toolbox saved template 1 and Linux home 1")
    );
    // The shell's record goes with it, so it isn't listed as exited.
    let listed = core.sessions(Default::default()).await.unwrap();
    assert!(!listed.sessions.iter().any(|s| s.agent == "toolbox"));

    // The next one is a new shell, and a new version.
    core.open_toolbox(NoParams {}).await.unwrap();
    wait_gone(&w.socket()).await;
    core.check_toolbox().await;
    let status = core.toolbox_status(NoParams {}).await.unwrap();
    assert_eq!((status.template, status.home), (Some(2), Some(2)));
}

/// The toolbox said to be open when nothing of its shell is left is closed,
/// and says so: Settings never goes on showing a shell open that isn't
/// (#46).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_toolbox_with_no_shell_left_is_closed_and_says_so() {
    let w = world();
    let core = w.core(&["sleep", "600"]);
    core.open_toolbox(NoParams {}).await.unwrap();
    let pid = running(&w.socket()).await.unwrap();
    let (_, mut events) = core.subscribe();
    // While the shell runs, it's open, and stays so.
    core.check_toolbox().await;
    assert_eq!(
        core.toolbox_status(NoParams {}).await.unwrap().state,
        ToolboxState::Open
    );
    assert!(events.try_recv().is_err());
    // What it had is thrown away under it.
    fs::remove_file(w.layout.agent_dir("toolbox").join("open.json")).unwrap();
    kill_shim(pid);
    wait_gone(&w.socket()).await;
    core.check_toolbox().await;
    assert_eq!(
        core.toolbox_status(NoParams {}).await.unwrap().state,
        ToolboxState::Closed
    );
    let e = events.try_recv().expect("the close is told");
    assert_eq!(e.kind, EventKind::ToolboxChanged);
    // Told once.
    core.check_toolbox().await;
    assert!(events.try_recv().is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_toolbox_at_a_time() {
    let w = world();
    let core = w.core(&["sleep", "600"]);
    let first = core.open_toolbox(NoParams {}).await.unwrap();
    let pid = running(&w.socket()).await.unwrap();
    // Opening again attaches to the same shell.
    let again = core.open_toolbox(NoParams {}).await.unwrap();
    assert_eq!(again.socket, first.socket);
    assert_eq!(running(&w.socket()).await, Some(pid));
    w.recipe("[[install]]\nname = \"x\"\nrun = \"true\"\n\n[update]\nrun = [\"true\"]\n");
    for e in [
        core.update_toolbox(NoParams {}).await.unwrap_err(),
        core.rebuild_toolbox(NoParams {}).await.unwrap_err(),
    ] {
        assert_eq!(e.code, ErrorCode::Conflict.code());
        assert!(e.message.contains("shell is open"), "{}", e.message);
    }
    core.check_toolbox().await;
    assert_eq!(
        core.toolbox_status(NoParams {}).await.unwrap().state,
        ToolboxState::Open
    );
    // A shim that dies takes this shell with it, which counts as an exit.
    kill_shim(pid);
    wait_gone(&w.socket()).await;
    core.check_toolbox().await;
    assert_eq!(
        core.toolbox_status(NoParams {}).await.unwrap().template,
        Some(1)
    );
    // Then an update may run.
    let run = core.update_toolbox(NoParams {}).await.unwrap();
    assert_eq!((run.action, run.template), (ToolboxAction::Update, Some(2)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shell_that_outlives_its_shim_gets_a_new_one() {
    let w = world();
    let core = w.core_with(Some(w.toolbox(&["sleep", "600"]).outliving_its_shim()));
    core.open_toolbox(NoParams {}).await.unwrap();
    let pid = running(&w.socket()).await.unwrap();
    kill_shim(pid);
    wait_gone(&w.socket()).await;
    core.check_toolbox().await;
    let again = running(&w.socket())
        .await
        .expect("a new shim holds the shell");
    assert_ne!(again, pid);
    assert_eq!(
        core.toolbox_status(NoParams {}).await.unwrap().state,
        ToolboxState::Open
    );
    assert!(
        !w.version("template-1.img"),
        "nothing saved: the shell runs on"
    );
    kill_shim(again);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn updates_and_rebuilds_say_how_they_went() {
    let w = world();
    let core = w.core_with(Some(
        w.toolbox(&["true"])
            .with_uncovered(vec!["apt: htop".into()]),
    ));
    // No recipe: nothing to update, and nothing recorded.
    let e = core.update_toolbox(NoParams {}).await.unwrap_err();
    assert!(
        e.message.contains("has no [update] commands"),
        "{}",
        e.message
    );
    assert_eq!(core.toolbox_status(NoParams {}).await.unwrap().last, None);
    w.recipe("packages = [\"--bad\"]\n");
    let e = core.rebuild_toolbox(NoParams {}).await.unwrap_err();
    assert!(
        e.message.contains("isn't a Debian package name"),
        "{}",
        e.message
    );

    w.recipe("[[install]]\nname = \"gcloud\"\nrun = \"true\"\n\n[update]\nrun = [\"exit 4\"]\n");
    let e = core.update_toolbox(NoParams {}).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::CommandFailed.code());
    assert_eq!(e.message, "`exit 4` exited with code 4");
    let last = core
        .toolbox_status(NoParams {})
        .await
        .unwrap()
        .last
        .unwrap();
    assert_eq!(last.error.as_deref(), Some("`exit 4` exited with code 4"));
    assert_eq!(
        core.toolbox_status(NoParams {}).await.unwrap().state,
        ToolboxState::Closed
    );

    let run = core.rebuild_toolbox(NoParams {}).await.unwrap();
    assert_eq!(run.action, ToolboxAction::Rebuild);
    assert_eq!(run.template, Some(1));
    assert_eq!(run.uncovered, ["apt: htop"]);
    assert_eq!(
        fs::read_to_string(w.layout.linux_dir().join("template-1.base")).unwrap(),
        "1\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_update_runs_weekly() {
    let w = world();
    let core = w.core(&["true"]);
    w.recipe("[update]\nrun = [\"true\"]\n");
    // No template yet: nothing to update.
    w.advance(30 * DAY);
    core.update_toolbox_if_due().await;
    assert!(!w.version("template-1.img"));
    // The first template, then a week later the update.
    w.clock.store(now(), Ordering::SeqCst);
    core.rebuild_toolbox(NoParams {}).await.unwrap();
    core.update_toolbox_if_due().await;
    assert!(!w.version("template-2.img"), "not due yet");
    w.advance(7 * DAY + 60);
    core.update_toolbox_if_due().await;
    assert!(
        w.version("template-2.img"),
        "due a week after the template was made"
    );
    core.update_toolbox_if_due().await;
    assert!(!w.version("template-3.img"), "and then a week after that");

    // A failed update is tried again a day later, not a week.
    w.recipe("[update]\nrun = [\"false\"]\n");
    w.advance(7 * DAY + 60);
    core.update_toolbox_if_due().await;
    let last = core
        .toolbox_status(NoParams {})
        .await
        .unwrap()
        .last
        .unwrap();
    assert!(last.error.is_some());
    w.recipe("[update]\nrun = [\"true\"]\n");
    w.advance(3600);
    core.update_toolbox_if_due().await;
    assert!(!w.version("template-3.img"));
    w.advance(DAY);
    core.update_toolbox_if_due().await;
    assert!(w.version("template-3.img"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_vibed_saves_a_shell_that_exited_while_it_was_down() {
    let w = world();
    let core = w.core(&["sh", "-c", "sleep 0.5"]);
    core.open_toolbox(NoParams {}).await.unwrap();
    drop(core);
    // The shell exits while no vibed watches it.
    wait_gone(&w.socket()).await;
    let core = w.core(&["sh", "-c", "sleep 0.5"]);
    let supervisor = core
        .spawn_toolbox(Duration::from_millis(50), Duration::from_secs(3600))
        .unwrap();
    // Saved and recorded: the image is kept a moment before the run's
    // record is written, and stopping the supervisor in between would
    // cancel the record.
    wait_for_status(&core, |s| {
        s.template == Some(1) && s.state == ToolboxState::Closed && s.last.is_some()
    })
    .await;
    supervisor.abort();
    // What it did survives another restart.
    drop(core);
    let core = w.core(&["true"]);
    let last = core
        .toolbox_status(NoParams {})
        .await
        .unwrap()
        .last
        .unwrap();
    assert_eq!(
        (last.action, last.template),
        (ToolboxAction::Shell, Some(1))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_as_vibed_starts_waits_for_its_tidying_and_isnt_undone() {
    // Opened just after the supervisor starts, while its tidying runs; the
    // shell ends a while after the tidying would have, and is saved.
    let w = world();
    let toolbox = w
        .toolbox(&["sh", "-c", "sleep 1.5"])
        .with_slow_discard(Duration::from_millis(500));
    let core = w.core_with(Some(toolbox));
    let supervisor = core
        .spawn_toolbox(Duration::from_millis(50), Duration::from_secs(3600))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    core.open_toolbox(NoParams {}).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let status = core.toolbox_status(NoParams {}).await.unwrap();
    assert_eq!(status.state, ToolboxState::Open);
    assert!(running(&w.socket()).await.is_some(), "the shell runs");
    wait_for_status(&core, |s| s.template == Some(1) && s.last.is_some()).await;
    supervisor.abort();

    // Opened before the supervisor starts: its tidying leaves it alone.
    let w = world();
    let toolbox = w
        .toolbox(&["sleep", "600"])
        .with_slow_discard(Duration::from_millis(200));
    let core = w.core_with(Some(toolbox));
    core.open_toolbox(NoParams {}).await.unwrap();
    let supervisor = core
        .spawn_toolbox(Duration::from_millis(50), Duration::from_secs(3600))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let status = core.toolbox_status(NoParams {}).await.unwrap();
    assert_eq!(status.state, ToolboxState::Open);
    let pid = running(&w.socket()).await.expect("the shell runs on");
    supervisor.abort();
    kill_shim(pid);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_during_an_update_is_refused_not_queued() {
    let w = world();
    let core = w.core(&["sleep", "600"]);
    w.recipe("[update]\nrun = [\"sleep 1\"]\n");
    let updating = core.clone();
    let update = tokio::spawn(async move { updating.update_toolbox(NoParams {}).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    for e in [
        core.open_toolbox(NoParams {}).await.unwrap_err(),
        core.close_toolbox(NoParams {}).await.unwrap_err(),
    ] {
        assert_eq!(e.code, ErrorCode::Conflict.code());
        assert!(e.message.contains("busy updating"), "{}", e.message);
    }
    assert_eq!(update.await.unwrap().unwrap().template, Some(1));
}

#[tokio::test]
async fn a_host_without_a_toolbox_says_so() {
    let w = world();
    let core = w.core_with(None);
    let status = core.toolbox_status(NoParams {}).await.unwrap();
    assert_eq!(status.state, ToolboxState::Unavailable);
    assert_eq!(status.base, None);
    for e in [
        core.open_toolbox(NoParams {}).await.unwrap_err(),
        core.update_toolbox(NoParams {}).await.unwrap_err(),
        core.rebuild_toolbox(NoParams {}).await.unwrap_err(),
        core.toolbox_logins(NoParams {}).await.unwrap_err(),
        core.close_toolbox(NoParams {}).await.unwrap_err(),
    ] {
        assert_eq!(e.code, ErrorCode::Conflict.code());
        assert!(e.message.contains("no Linux toolbox"), "{}", e.message);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_agent_may_take_the_toolboxs_name() {
    let golden = common::Golden::new();
    let core = golden.core();
    core.add_project(ProjectAddParams {
        path: Some(golden.home.join("src/app").to_string_lossy().into_owned()),
        ..ProjectAddParams::default()
    })
    .await
    .unwrap();
    let e = core
        .start_agent(AgentStartParams {
            project: "app".into(),
            profile: "software-eng-linux".into(),
            prompt: "hi".into(),
            name: Some("toolbox".into()),
            ..AgentStartParams::default()
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams.code());
    assert!(e.message.contains("Linux toolbox's"), "{}", e.message);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_toolbox_counts_as_awake() {
    let golden = common::Golden::new();
    let core = golden.core();
    core.add_project(ProjectAddParams {
        path: Some(golden.home.join("src/app").to_string_lossy().into_owned()),
        ..ProjectAddParams::default()
    })
    .await
    .unwrap();
    core.set_limits(vibe_core::rpc::LimitsSetParams {
        max_awake_agents: Some(1),
        ..Default::default()
    })
    .unwrap();
    core.open_toolbox(NoParams {}).await.unwrap();
    let e = core
        .start_agent(AgentStartParams {
            project: "app".into(),
            profile: "software-eng-linux".into(),
            prompt: "hi".into(),
            ..AgentStartParams::default()
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict.code());
    assert!(e.message.contains("can't start an agent"), "{}", e.message);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn see_logins_says_what_the_home_holds_and_whether_claude_is_logged_in() {
    use vibe_core::model::LoginTool;
    use vibe_core::rpc::Login;
    let w = world();
    let gh = Login {
        tool: LoginTool::Gh,
        account: "octocat on github.com".into(),
    };
    let core = w.core_with(Some(
        w.toolbox(&["sh", "-c", "sleep 0.5"])
            .with_logins(vec![gh.clone()], true),
    ));
    // No home yet: nothing to log in with, and the toolbox stays closed.
    let l = core.toolbox_logins(NoParams {}).await.unwrap();
    assert_eq!((l.home, l.logins, l.claude), (None, Some(vec![]), true));
    assert_eq!(
        core.toolbox_status(NoParams {}).await.unwrap().state,
        ToolboxState::Closed
    );

    // A shell's save notes them.
    core.open_toolbox(NoParams {}).await.unwrap();
    wait_gone(&w.socket()).await;
    core.check_toolbox().await;
    let l = core.toolbox_logins(NoParams {}).await.unwrap();
    assert_eq!((l.home, l.logins), (Some(1), Some(vec![gh.clone()])));

    // A home saved without a look: unknown, not empty.
    fs::write(w.layout.linux_dir().join("home-2.img"), "").unwrap();
    let l = core.toolbox_logins(NoParams {}).await.unwrap();
    assert_eq!((l.home, l.logins), (Some(2), None));
    assert_eq!(l.claude_problem, None);

    // No Claude login for linux agents: it says so, and why.
    let other = world();
    let core = other.core(&["true"]);
    let l = core.toolbox_logins(NoParams {}).await.unwrap();
    assert!(!l.claude);
    assert!(
        l.claude_problem
            .as_deref()
            .is_some_and(|p| p.contains("vibe setup-token")),
        "{l:?}"
    );
}

/// A terminal attached to the toolbox shell, as `vibe toolbox` is: the
/// shim counts it while it's connected.
fn attach_terminal(socket: &std::path::Path) -> shim::Connection {
    let hello = shim::Hello {
        protocol: shim::PROTOCOL,
        role: shim::Role::Attach,
        client: "test".into(),
        tty: Some("/dev/ttys042".into()),
        size: None,
    };
    shim::connect(socket, &hello, Duration::from_secs(10)).unwrap()
}

/// Settings tells a shell open in a Terminal from one left open in the
/// background, with nothing attached (#54); and says nothing when it can't
/// know, rather than that nothing is attached.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_status_counts_the_terminals_attached_to_the_shell() {
    let w = world();
    let core = w.core(&["sleep", "600"]);
    assert_eq!(
        core.toolbox_status(NoParams {}).await.unwrap().clients,
        None
    );
    core.open_toolbox(NoParams {}).await.unwrap();
    // Open, with nothing attached: in the background.
    let status = core.toolbox_status(NoParams {}).await.unwrap();
    assert_eq!(
        (status.state, status.clients),
        (ToolboxState::Open, Some(0))
    );
    let first = attach_terminal(&w.socket());
    let second = attach_terminal(&w.socket());
    let clients = core.toolbox_status(NoParams {}).await.unwrap().clients;
    assert_eq!(clients, Some(2));
    // A detach leaves it open, and counted as it is.
    drop(first);
    wait_for_status(&core, |s| s.clients == Some(1)).await;
    drop(second);
    wait_for_status(&core, |s| s.clients == Some(0)).await;
    assert_eq!(
        core.toolbox_status(NoParams {}).await.unwrap().state,
        ToolboxState::Open
    );
    kill_shim(running(&w.socket()).await.unwrap());
}

/// During a boot it's open, but no shim is up to ask: that's not known,
/// not "nothing attached".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_count_is_unknown_while_the_toolbox_boots() {
    let w = world();
    let core = w.core_with(Some(
        w.toolbox(&["sleep", "600"])
            .with_slow_open(Duration::from_secs(1)),
    ));
    let opening = core.clone();
    let open = tokio::spawn(async move { opening.open_toolbox(NoParams {}).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let status = core.toolbox_status(NoParams {}).await.unwrap();
    assert_eq!((status.state, status.clients), (ToolboxState::Open, None));
    open.await.unwrap().unwrap();
    assert_eq!(
        core.toolbox_status(NoParams {}).await.unwrap().clients,
        Some(0)
    );
    kill_shim(running(&w.socket()).await.unwrap());
}

/// Save & Close: the shell is hung up on, as closing its window does, and
/// saved as an exit is, whoever is attached; and closing a closed toolbox
/// changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_and_close_ends_the_shell_and_saves_it() {
    let w = world();
    // A shell that ignores nothing: a hangup ends it, as it does bash.
    let core = w.core(&["sleep", "600"]);
    // Closed already: nothing to do, and nothing is said.
    let (_, mut events) = core.subscribe();
    let status = core.close_toolbox(NoParams {}).await.unwrap();
    assert_eq!(
        (status.state, status.template),
        (ToolboxState::Closed, None)
    );
    assert!(events.try_recv().is_err());

    core.open_toolbox(NoParams {}).await.unwrap();
    assert!(
        events
            .recv()
            .await
            .unwrap()
            .message
            .unwrap()
            .contains("is open")
    );
    let mut attached = attach_terminal(&w.socket());
    let status = core.close_toolbox(NoParams {}).await.unwrap();
    assert_eq!(status.state, ToolboxState::Closed);
    assert_eq!(
        (status.template, status.home, status.clients),
        (Some(1), Some(1), None)
    );
    let last = status.last.unwrap();
    assert_eq!(
        (last.action, last.template, last.error),
        (ToolboxAction::Shell, Some(1), None)
    );
    assert!(w.version("template-1.img") && w.version("home-1.img"));
    assert_eq!(
        events.recv().await.unwrap().message.as_deref(),
        Some("The toolbox saved template 1 and Linux home 1")
    );
    // The attached terminal saw the shell end, as after an exit. (Its
    // reads wait at most the 10 s it connected with.)
    let exit = loop {
        match shim::read_frame(&mut attached.stream).unwrap() {
            Some(f) if f.kind == shim::FrameKind::Exit => break f.decode::<SessionExit>().unwrap(),
            Some(_) => continue,
            None => panic!("the shim went without saying the shell ended"),
        }
    };
    assert_eq!(exit.signal.as_deref(), Some("SIGHUP"));
    assert!(matches!(sessions::probe(&w.socket()).await, Probe::Gone));
    // Its session is gone, not left listed as exited.
    let listed = core.sessions(Default::default()).await.unwrap();
    assert!(!listed.sessions.iter().any(|s| s.agent == "toolbox"));
    // The supervisor, looking after, finds nothing more to do.
    core.check_toolbox().await;
    assert!(events.try_recv().is_err());

    // Twice is harmless: closed, nothing new kept, nothing said.
    let again = core.close_toolbox(NoParams {}).await.unwrap();
    assert_eq!(again.state, ToolboxState::Closed);
    assert_eq!((again.template, again.home), (Some(1), Some(1)));
    assert!(!w.version("template-2.img"));
    assert!(events.try_recv().is_err());

    // With nothing attached, the same.
    core.open_toolbox(NoParams {}).await.unwrap();
    let status = core.close_toolbox(NoParams {}).await.unwrap();
    assert_eq!(
        (status.state, status.template),
        (ToolboxState::Closed, Some(2))
    );
}

/// A shell whose shim has died is saved by Save & Close all the same:
/// nothing is attached to it, and the save ends what still runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_and_close_saves_a_shell_whose_shim_died() {
    let w = world();
    let core = w.core_with(Some(w.toolbox(&["sleep", "600"]).outliving_its_shim()));
    core.open_toolbox(NoParams {}).await.unwrap();
    kill_shim(running(&w.socket()).await.unwrap());
    wait_gone(&w.socket()).await;
    let status = core.close_toolbox(NoParams {}).await.unwrap();
    assert_eq!(
        (status.state, status.template),
        (ToolboxState::Closed, Some(1))
    );
}

/// A shell that ignores the hang-up is refused, not forced: nothing is
/// saved, it stays open, and exiting it afterwards saves as usual.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_and_close_refuses_a_shell_that_wont_exit() {
    let w = world();
    // It says when the trap is set: a hang-up before then would end it.
    let trapped = w.release().with_extension("trapped");
    let shell = format!(
        "trap '' HUP; : > '{}'; {}",
        trapped.display(),
        w.held_shell()
    );
    let core = w.core(&["sh", "-c", &shell]);
    core.open_toolbox(NoParams {}).await.unwrap();
    for _ in 0..400 {
        if trapped.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(trapped.exists(), "the shell never set its trap");
    let e = core.close_toolbox(NoParams {}).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict.code());
    assert!(
        e.message.contains("didn't exit when asked"),
        "{}",
        e.message
    );
    let status = core.toolbox_status(NoParams {}).await.unwrap();
    assert_eq!((status.state, status.template), (ToolboxState::Open, None));
    assert!(running(&w.socket()).await.is_some());
    fs::write(w.release(), "").unwrap();
    wait_gone(&w.socket()).await;
    core.check_toolbox().await;
    let status = core.toolbox_status(NoParams {}).await.unwrap();
    assert_eq!(
        (status.state, status.template),
        (ToolboxState::Closed, Some(1))
    );
}

/// A save that fails comes back as the call's error, and is the last run,
/// as after an exit; the toolbox isn't left said to be open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_and_close_says_when_the_save_failed() {
    let w = world();
    let core = w.core(&["sleep", "600"]);
    core.open_toolbox(NoParams {}).await.unwrap();
    // The test toolbox can't save with no base image.
    fs::remove_dir_all(w.layout.linux_dir().join("base-1")).unwrap();
    let e = core.close_toolbox(NoParams {}).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::CommandFailed.code());
    assert!(e.message.contains("no Linux base image"), "{}", e.message);
    let status = core.toolbox_status(NoParams {}).await.unwrap();
    assert_eq!(status.state, ToolboxState::Closed);
    let last = status.last.unwrap();
    assert_eq!(last.action, ToolboxAction::Shell);
    assert_eq!(last.error.as_deref(), Some(e.message.as_str()));
}

/// A `kill` that fails because the shell had just exited isn't an error:
/// it's saved like any exit. One that fails with the shell still there is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_and_close_tells_a_failed_hang_up_from_a_shell_just_gone() {
    let w = world();
    let shell = w.held_shell();
    let core = w.core_with(Some(
        w.toolbox(&["sh", "-c", &shell]).with_failing_hang_up(),
    ));
    core.open_toolbox(NoParams {}).await.unwrap();
    let e = core.close_toolbox(NoParams {}).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::CommandFailed.code());
    assert!(
        e.message.contains("can't close the toolbox shell"),
        "{}",
        e.message
    );
    assert_eq!(
        core.toolbox_status(NoParams {}).await.unwrap().state,
        ToolboxState::Open
    );
    // Now the shell goes just as the call comes.
    fs::write(w.release(), "").unwrap();
    let status = core.close_toolbox(NoParams {}).await.unwrap();
    assert_eq!(
        (status.state, status.template),
        (ToolboxState::Closed, Some(1))
    );
}
