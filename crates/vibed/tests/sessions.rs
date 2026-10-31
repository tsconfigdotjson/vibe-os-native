//! Sessions through the core: finding the shims in `run/`, listing them with
//! the records of those that exited, attaching, and starting one.

mod common;

use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use common::{alive, short_tempdir, start_shim, wait_until, workspace_bin, write_record};
use serde_json::json;
use vibe_core::model::{Environment, SessionState};
use vibe_core::paths::Layout;
use vibe_core::rpc::{ErrorCode, HostParams, ProjectAddParams, SessionParams};
use vibe_core::shim::{self, Size, control_hello};
use vibe_core::store::{AgentRecord, Store};
use vibed::core::{Config, Core, DaemonInfo, OpenError};
use vibed::sessions::SessionSpec;

struct World {
    _dir: tempfile::TempDir,
    layout: Layout,
}

fn world() -> World {
    let dir = short_tempdir();
    let home = dir.path().canonicalize().unwrap().join("home");
    World {
        layout: Layout::new(home, std::env::consts::OS),
        _dir: dir,
    }
}

fn config(w: &World) -> Config {
    let mut config = Config::new(DaemonInfo::current(), w.layout.clone());
    config.gh = "/nonexistent/gh".into();
    config.shim = workspace_bin("vibe-shim").into_os_string();
    config
}

fn core(w: &World) -> Arc<Core> {
    Arc::new(Core::open(config(w)).unwrap())
}

fn attach_params(r: &str) -> SessionParams {
    SessionParams {
        host: None,
        agent: r.into(),
    }
}

#[tokio::test]
async fn running_and_exited_sessions_are_listed() {
    let w = world();
    let core = core(&w);
    assert!(
        core.sessions(HostParams::default())
            .await
            .unwrap()
            .sessions
            .is_empty()
    );

    let shim = start_shim(&w.layout, "quiet-amber-otter", &["sleep", "600"]);
    write_record(
        &w.layout,
        "brave-copper-lynx",
        Some(1),
        None,
        "2026-10-30T11:00:00Z",
    );
    // A record left from before the running session is superseded by it.
    write_record(
        &w.layout,
        "quiet-amber-otter",
        Some(0),
        None,
        "2026-10-30T09:00:00Z",
    );
    // Unreadable records are skipped, not fatal.
    std::fs::create_dir_all(w.layout.agent_dir("junk")).unwrap();
    std::fs::write(w.layout.agent_dir("junk").join(shim::RECORD_FILE), "{").unwrap();

    let list = core.sessions(HostParams::default()).await.unwrap().sessions;
    let refs: Vec<_> = list.iter().map(|s| (s.agent.as_str(), s.state)).collect();
    assert_eq!(
        refs,
        [
            ("brave-copper-lynx", SessionState::Exited),
            ("quiet-amber-otter", SessionState::Running)
        ]
    );
    assert_eq!(list[0].exit.as_ref().unwrap().code, Some(1));
    assert_eq!(list[1].title, "quiet-amber-otter");
    assert_eq!((list[1].clients, list[1].exit.as_ref()), (0, None));

    // An attached client shows, with its terminal.
    let hello = shim::Hello {
        protocol: shim::PROTOCOL,
        role: shim::Role::Attach,
        client: "test".into(),
        tty: Some("/dev/ttys009".into()),
        size: Some(Size { rows: 30, cols: 90 }),
    };
    let _client = shim::connect(&shim.socket, &hello, Duration::from_secs(10)).unwrap();
    let list = core.sessions(HostParams::default()).await.unwrap().sessions;
    assert_eq!(list[1].clients, 1);
    assert_eq!(list[1].ttys, ["/dev/ttys009"]);
    let found = core
        .attach_session(attach_params("quiet-amber-otter"))
        .await
        .unwrap();
    assert_eq!(found.ttys, ["/dev/ttys009"]);
    assert_eq!(found.socket, shim.socket.to_str().unwrap());

    // A host this one has never added is unknown.
    let other = core
        .sessions(HostParams {
            host: Some("vps".into()),
        })
        .await
        .unwrap_err();
    assert_eq!(other.code, ErrorCode::NotFound.code());
}

#[tokio::test]
async fn an_agents_session_is_titled_with_its_profile() {
    let w = world();
    let core = core(&w);
    let repo = w.layout.home().join("src/app");
    common::repo(&repo);
    core.add_project(ProjectAddParams {
        path: Some(repo.to_str().unwrap().into()),
        ..Default::default()
    })
    .await
    .unwrap();
    Store::open(&w.layout.db())
        .unwrap()
        .add_agent(&AgentRecord {
            host: "local".into(),
            agent_ref: "quiet-amber-otter".into(),
            project: "app".into(),
            profile: "reviewer".into(),
            environment: Environment::Linux,
            model: vibe_core::model::Model::Opus,
            reasoning: vibe_core::model::Reasoning::High,
            mode: vibe_core::model::Mode::Auto,
            memory: 1 << 30,
            worktree: "/w".into(),
            branch: "quiet-amber-otter".into(),
            session_id: "s".into(),
            shim_socket: None,
            state: vibe_core::model::AgentState::Busy,
            created_at: 0,
            compartment: "{}".into(),
            starting: false,
        })
        .unwrap();
    let _shim = start_shim(&w.layout, "quiet-amber-otter", &["sleep", "600"]);
    let found = core
        .attach_session(attach_params("quiet-amber-otter"))
        .await
        .unwrap();
    assert_eq!(found.title, "quiet-amber-otter · reviewer");
    let list = core.sessions(HostParams::default()).await.unwrap().sessions;
    assert_eq!(list[0].title, "quiet-amber-otter · reviewer");
}

#[tokio::test]
async fn attaching_says_why_it_cant() {
    let w = world();
    let core = core(&w);
    let err = core
        .attach_session(attach_params("lone-grey-wolf"))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound.code());
    assert_eq!(err.message, "no session lone-grey-wolf on local");

    write_record(
        &w.layout,
        "brave-copper-lynx",
        None,
        Some("SIGHUP"),
        "2026-10-30T11:00:00Z",
    );
    let err = core
        .attach_session(attach_params("brave-copper-lynx"))
        .await
        .unwrap_err();
    assert_eq!(
        err.message,
        "session brave-copper-lynx isn't running: its command was ended by SIGHUP at 2026-10-30T11:00:00Z"
    );

    for bad in ["", "../x", "vibed", "a b"] {
        let err = core.attach_session(attach_params(bad)).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams.code(), "{bad:?}");
    }

    // A socket nobody serves any more is cleaned up on the way.
    let stale = w.layout.session_socket("dead-end");
    std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
    let err = core
        .attach_session(attach_params("dead-end"))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound.code());
    assert!(!stale.exists());
}

#[tokio::test]
async fn adopting_finds_running_shims_and_clears_stale_sockets() {
    let w = world();
    let a = start_shim(&w.layout, "quiet-amber-otter", &["sleep", "600"]);
    let _b = start_shim(&w.layout, "brave-copper-lynx", &["sleep", "600"]);
    let stale = w.layout.session_socket("gone-away");
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
    let core = core(&w);
    assert_eq!(core.adopt_sessions().await, 2);
    assert!(!stale.exists(), "the stale socket is removed");

    // A shim that dies is gone from the list; its command was hung up.
    let child = shim::connect(&a.socket, &control_hello("t"), Duration::from_secs(5))
        .unwrap()
        .welcome
        .child_pid as i32;
    // SAFETY: a signal to the shim this test started.
    unsafe { libc::kill(a.pid, libc::SIGKILL) };
    wait_until("the command to be hung up", || !alive(child));
    let list = core.sessions(HostParams::default()).await.unwrap().sessions;
    let refs: Vec<_> = list.iter().map(|s| s.agent.as_str()).collect();
    assert_eq!(refs, ["brave-copper-lynx"]);
    assert!(!a.socket.exists(), "its socket was stale, so it went");
}

#[tokio::test]
async fn a_started_session_is_detached_and_outlives_vibed() {
    let w = world();
    let core = core(&w);
    let spec = SessionSpec {
        agent: "quiet-amber-otter".into(),
        command: ["sh", "-c", "echo hello-$GREETING; exec sleep 600"]
            .map(OsString::from)
            .to_vec(),
        cwd: w.layout.home().to_owned(),
        env: vec![("GREETING".into(), "world".into())],
        size: Some(Size { rows: 10, cols: 50 }),
        vm: None,
    };
    std::fs::create_dir_all(w.layout.home()).unwrap();
    let info = core.start_session(spec.clone()).await.unwrap();
    assert_eq!(info.state, SessionState::Running);
    assert_eq!(info.agent, "quiet-amber-otter");

    let socket = w.layout.session_socket("quiet-amber-otter");
    let mut conn = shim::connect(&socket, &control_hello("t"), Duration::from_secs(5)).unwrap();
    let shim_pid = conn.welcome.pid as i32;
    let status = conn.status().unwrap();
    assert_eq!(status.size, Size { rows: 10, cols: 50 });
    assert_eq!(status.cwd, w.layout.home().to_str().unwrap());
    wait_until("the greeting", || {
        conn.read_output(None)
            .unwrap()
            .screen
            .contains("hello-world")
    });
    // SAFETY: getsid only reads.
    assert_eq!(
        unsafe { libc::getsid(shim_pid) },
        shim_pid,
        "its own session"
    );
    // And its own process group, so launchd ending vibed's job, which kills
    // vibed's process group, leaves it alone.
    // SAFETY: getpgid only reads.
    let (shim_group, our_group) = unsafe { (libc::getpgid(shim_pid), libc::getpgid(0)) };
    assert_eq!(shim_group, shim_pid);
    assert_ne!(shim_group, our_group);

    // Its log is in the agent's directory.
    let log = std::fs::read_to_string(w.layout.agent_dir("quiet-amber-otter").join(shim::LOG_FILE))
        .unwrap();
    assert!(log.contains("started"), "{log}");

    // A second start of the same ref is refused by the shim.
    let err = core.start_session(spec).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::CommandFailed.code());
    assert!(err.message.contains("already running"), "{}", err.message);

    // vibed goes; the session doesn't, and holds nothing of vibed's, so a
    // new vibed can open the same state.
    drop(core);
    assert!(alive(shim_pid));
    let again = match Core::open(config(&w)) {
        Ok(c) => c,
        Err(OpenError::AlreadyRunning { .. }) => panic!("the shim kept vibed's state lock"),
        Err(e) => panic!("{e}"),
    };
    assert_eq!(again.adopt_sessions().await, 1);
    #[cfg(target_os = "linux")]
    for fd in std::fs::read_dir(format!("/proc/{shim_pid}/fd")).unwrap() {
        let target = std::fs::read_link(fd.unwrap().path()).unwrap_or_default();
        let target = target.to_string_lossy();
        assert!(
            !target.contains("vibe.db") && !target.contains("vibed.lock"),
            "the shim inherited {target}"
        );
    }
    // SAFETY: a signal to the shim this test started.
    unsafe { libc::kill(shim_pid, libc::SIGTERM) };
    wait_until("the shim to stop", || !alive(shim_pid));
    let list = again
        .sessions(HostParams::default())
        .await
        .unwrap()
        .sessions;
    assert_eq!(list[0].state, SessionState::Exited);
    assert_eq!(
        list[0].exit.as_ref().unwrap().signal.as_deref(),
        Some("SIGHUP")
    );
}

#[tokio::test]
async fn starting_reports_what_went_wrong() {
    let w = world();
    let core = core(&w);
    let spec = |agent: &str, command: &str| SessionSpec {
        agent: agent.into(),
        command: vec![command.into()],
        cwd: "/".into(),
        env: Vec::new(),
        size: None,
        vm: None,
    };
    let err = core
        .start_session(spec("quiet-amber-otter", "/nonexistent/claude"))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::CommandFailed.code());
    assert!(
        err.message.starts_with("can't start /nonexistent/claude"),
        "{}",
        err.message
    );
    let err = core.start_session(spec("../x", "sh")).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams.code());

    // A command that ends at once is reported as exited.
    let info = core.start_session(spec("brief-one", "true")).await.unwrap();
    if info.state == SessionState::Running {
        wait_until("the record", || {
            w.layout
                .agent_dir("brief-one")
                .join(shim::RECORD_FILE)
                .exists()
        });
    } else {
        assert_eq!(info.exit.unwrap().code, Some(0));
    }

    // No vibe-shim at all.
    let mut broken = config(&w);
    broken.shim = "/nonexistent/vibe-shim".into();
    drop(core);
    let core = Core::open(broken).unwrap();
    let err = core.start_session(spec("x", "sh")).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::CommandFailed.code());
    assert!(
        err.message.contains("can't run /nonexistent/vibe-shim"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn sessions_over_the_socket() {
    let w = world();
    let _shim = start_shim(&w.layout, "quiet-amber-otter", &["sleep", "600"]);
    let server = common::serve(core(&w), short_tempdir(), vibe_core::rpc::MAX_MESSAGE_BYTES);
    let mut conn = common::Conn::open(&server.path).await;
    let list = conn.call("sessions.list", json!({})).await;
    assert_eq!(list["result"]["sessions"][0]["state"], "running");
    let found = conn
        .call("sessions.attach", json!({"ref": "quiet-amber-otter"}))
        .await;
    assert_eq!(
        found["result"]["socket"],
        w.layout
            .session_socket("quiet-amber-otter")
            .to_str()
            .unwrap()
    );
    let bad = conn.call("sessions.attach", json!({"agent": "x"})).await;
    assert_eq!(bad["error"]["code"], ErrorCode::InvalidParams.code());
}
