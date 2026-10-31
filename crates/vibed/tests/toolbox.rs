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

use common::{short_tempdir, wait_until, workspace_bin};
use vibe_core::model::{EventKind, ToolboxAction, ToolboxState};
use vibe_core::paths::Layout;
use vibe_core::rpc::{AgentStartParams, ErrorCode, NoParams, ProjectAddParams};
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

fn kill_shim(pid: u32) {
    // SAFETY: a signal to a shim this test started.
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exiting_the_shell_saves_and_the_next_one_starts_afresh() {
    let w = world();
    let core = w.core(&["sh", "-c", "sleep 0.5"]);
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

    // While the shell runs, nothing is saved.
    core.check_toolbox().await;
    assert!(!w.version("template-1.img"));
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
    let watched = core.clone();
    tokio::task::spawn_blocking(move || {
        wait_until("the toolbox to be saved", || {
            watched.toolbox_status_now().template == Some(1)
        })
    })
    .await
    .unwrap();
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
async fn an_open_toolbox_counts_against_admission() {
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
