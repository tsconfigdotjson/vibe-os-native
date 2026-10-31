//! The core handlers over a real socket, beyond the golden examples: what
//! survives a restart, what's refused, and what's left on disk.

mod common;

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use common::{Conn, Golden, repo, serve};
use serde_json::{Value, json};
use vibe_core::model::{EventKind, GhState, LOCAL_HOST};
use vibe_core::paths::Layout;
use vibe_core::rpc::{GhStatusParams, MAX_MESSAGE_BYTES};
use vibe_core::store::{AgentRecord, Store};
use vibed::core::{Config, Core, OpenError};

const G: u64 = 1 << 30;

fn ok(reply: &Value) -> &Value {
    assert!(reply.get("error").is_none(), "{reply}");
    &reply["result"]
}

/// A new core on `golden`'s home, once the last one has let go of it: a
/// stopped server's tasks finish on their own time.
async fn reopen(golden: &Golden) -> Arc<Core> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match Core::open(golden.config()) {
            Ok(core) => return Arc::new(core),
            Err(OpenError::AlreadyRunning { .. }) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await
            }
            Err(e) => panic!("{e}"),
        }
    }
}

fn code(reply: &Value) -> i64 {
    reply["error"]["code"]
        .as_i64()
        .unwrap_or_else(|| panic!("not an error: {reply}"))
}

/// The ticket's "done when": over the socket, add a project, list its two
/// starter profiles, edit one and change a host limit; then restart and
/// find all of it.
#[tokio::test]
async fn registry_survives_a_restart() {
    let golden = Golden::new();
    let app = golden.home.join("src/app");
    {
        let server = serve(
            golden.core(),
            tempfile::tempdir().unwrap(),
            MAX_MESSAGE_BYTES,
        );
        let mut c = Conn::open(&server.path).await;
        let added = c.call("projects.add", json!({"path": app})).await;
        assert_eq!(ok(&added)["name"], "app");
        let profiles = c.call("profiles.list", json!({"project": "app"})).await;
        let names: Vec<&str> = ok(&profiles)["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["software-eng-linux", "software-eng-mac"]);
        let edited = c
            .call(
                "profiles.update",
                json!({"project": "app", "profile": "software-eng-mac", "memory": 6 * G, "mode": "bypass"}),
            )
            .await;
        assert_eq!(ok(&edited)["memory"], 6 * G);
        let limits = c.call("limits.set", json!({"cpu_cores": 6})).await;
        assert_eq!(ok(&limits)["cpu_cores"], 6);
        let key = c.call("keys.create", json!({"name": "laptop"})).await;
        assert!(ok(&key)["secret"].as_str().unwrap().starts_with("vk_"));
    }
    // A new daemon on the same home.
    let server = serve(
        reopen(&golden).await,
        tempfile::tempdir().unwrap(),
        MAX_MESSAGE_BYTES,
    );
    let mut c = Conn::open(&server.path).await;
    let projects = c.call("projects.list", json!({})).await;
    let project = &ok(&projects)["projects"][0];
    assert_eq!(project["name"], "app");
    assert_eq!(project["path"], app.to_str().unwrap());
    let mac = &project["profiles"][1];
    assert_eq!(
        (&mac["name"], &mac["memory"], &mac["mode"]),
        (&json!("software-eng-mac"), &json!(6 * G), &json!("bypass"))
    );
    assert_eq!(ok(&c.call("limits.get", json!({})).await)["cpu_cores"], 6);
    assert_eq!(
        ok(&c.call("keys.list", json!({})).await)["keys"][0]["name"],
        "laptop"
    );
}

#[test]
fn one_daemon_per_state_directory() {
    let golden = Golden::new();
    let _first = golden.core();
    let err = Core::open(golden.config()).unwrap_err();
    assert!(matches!(err, OpenError::AlreadyRunning { .. }), "{err}");
    assert!(err.to_string().contains("another vibed"), "{err}");
}

#[test]
fn a_lock_let_go_of_a_moment_later_is_taken() {
    // As when a child forked before the last vibed closed its lock still
    // holds it, until it runs its own program.
    let golden = Golden::new();
    drop(golden.core());
    let layout = vibe_core::paths::Layout::new(&golden.home, "macos");
    let held = std::fs::OpenOptions::new()
        .write(true)
        .open(layout.data_dir().join("vibed.lock"))
        .unwrap();
    // Waited for, not tried: the core just dropped may not have let go in
    // every process yet, for the same reason.
    held.lock().unwrap();
    let let_go = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        drop(held);
    });
    let started = std::time::Instant::now();
    Core::open(golden.config()).unwrap();
    assert!(started.elapsed() >= Duration::from_millis(400));
    let_go.join().unwrap();
}

#[test]
fn the_state_directory_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let golden = Golden::new();
    let core = golden.core();
    let mode = |p: &std::path::Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&core.layout().data_dir()), 0o700);
    assert_eq!(mode(&core.layout().db()), 0o600);
}

#[test]
fn an_existing_state_directory_is_made_private() {
    use std::os::unix::fs::PermissionsExt;
    let golden = Golden::new();
    let dir = Layout::new(&golden.home, "macos").data_dir();
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let _core = golden.core();
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

/// A clone that takes a while holds its name, but nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_clone_holds_only_its_own_name() {
    let golden = Golden::new();
    let slow = golden
        .dir
        .path()
        .canonicalize()
        .unwrap()
        .join("bin/slow-gh");
    let marker = golden.dir.path().join("cloning");
    common::write_script(
        &slow,
        &format!(
            "#!/bin/sh\ntouch {}\nsleep 3\necho 'GraphQL: Could not resolve' >&2\nexit 1\n",
            marker.display()
        ),
    );
    let mut config = golden.config();
    config.gh = slow.into();
    let core = Arc::new(Core::open(config).unwrap());
    let server = serve(core, tempfile::tempdir().unwrap(), MAX_MESSAGE_BYTES);
    let mut cloning = Conn::open(&server.path).await;
    let mut other = Conn::open(&server.path).await;
    cloning
        .send(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"projects.add\",\"params\":{\"repo\":\"me/web\"}}\n")
        .await;
    while !marker.exists() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let started = std::time::Instant::now();
    let same = other.call("projects.add", json!({"repo": "me/web"})).await;
    assert_eq!(code(&same), -32002);
    assert_eq!(
        same["error"]["message"],
        "a project named web is being added already"
    );
    let app = other
        .call("projects.add", json!({"path": golden.home.join("src/app")}))
        .await;
    assert_eq!(ok(&app)["name"], "app");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the local add waited for the clone"
    );
    // Once the clone fails, the name is free again.
    assert_eq!(code(&cloning.reply().await), -32004);
    let again = other
        .call(
            "projects.add",
            json!({"path": golden.home.join("src/app"), "name": "web"}),
        )
        .await;
    assert_eq!(
        code(&again),
        -32002,
        "same path, but the name is free: {again}"
    );
    assert!(
        again["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already the project app")
    );
}

#[tokio::test]
async fn a_linux_host_starts_projects_with_one_profile_and_no_macos() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().canonicalize().unwrap().join("home");
    repo(&home.join("src/app"));
    let mut info = common::golden_info();
    info.os = "linux".into();
    let core = Arc::new(Core::open(Config::new(info, Layout::new(&home, "linux"))).unwrap());
    let server = serve(core, tempfile::tempdir().unwrap(), MAX_MESSAGE_BYTES);
    let mut c = Conn::open(&server.path).await;
    let added = c
        .call("projects.add", json!({"path": home.join("src/app")}))
        .await;
    let profiles = ok(&added)["profiles"].as_array().unwrap().clone();
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0]["name"], "software-eng-linux");
    assert_eq!(profiles[0]["environment"], "linux");

    let mac = c
        .call(
            "profiles.create",
            json!({"project": "app", "name": "m", "environment": "macos"}),
        )
        .await;
    assert_eq!(code(&mac), -32602);
    let to_mac = c
        .call(
            "profiles.update",
            json!({"project": "app", "profile": "software-eng-linux", "environment": "macos"}),
        )
        .await;
    assert_eq!(code(&to_mac), -32602);
    assert!(
        to_mac["error"]["message"]
            .as_str()
            .unwrap()
            .contains("only a Mac")
    );
}

#[tokio::test]
async fn profile_rules() {
    let golden = Golden::new();
    let server = serve(
        golden.core(),
        tempfile::tempdir().unwrap(),
        MAX_MESSAGE_BYTES,
    );
    let mut c = Conn::open(&server.path).await;
    c.call("projects.add", json!({"path": golden.home.join("src/app")}))
        .await;

    // Defaults fill in what's left out.
    let p = c
        .call(
            "profiles.create",
            json!({"project": "app", "name": "plain"}),
        )
        .await;
    assert_eq!(
        ok(&p),
        &json!({"name": "plain", "environment": "linux", "model": "opus",
                "reasoning": "high", "mode": "auto", "memory": 4 * G})
    );
    let taken = c
        .call(
            "profiles.create",
            json!({"project": "app", "name": "plain"}),
        )
        .await;
    assert_eq!(code(&taken), -32002);
    let rename_onto = c
        .call(
            "profiles.update",
            json!({"project": "app", "profile": "plain", "name": "software-eng-mac"}),
        )
        .await;
    assert_eq!(code(&rename_onto), -32002);
    for bad in [
        json!({"project": "app", "name": "x", "model": "gpt-5"}),
        json!({"project": "app", "name": "x", "reasoning": "extreme"}),
        json!({"project": "app", "name": "x", "mode": "plan"}),
        json!({"project": "app", "name": "x", "memory": 512 << 20}),
        json!({"project": "app", "name": "x", "memory": -1}),
        json!({"project": "app", "name": "../x"}),
        json!({"project": "app", "name": "x", "colour": "red"}),
    ] {
        let r = c.call("profiles.create", bad.clone()).await;
        assert_eq!(code(&r), -32602, "{bad}: {r}");
    }
    let missing = c.call("profiles.list", json!({"project": "nope"})).await;
    assert_eq!(code(&missing), -32001);
    // Nothing invalid got in.
    let list = c.call("profiles.list", json!({"project": "app"})).await;
    assert_eq!(ok(&list)["profiles"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn every_per_host_method_refuses_an_unknown_host() {
    let golden = Golden::new();
    let server = serve(
        golden.core(),
        tempfile::tempdir().unwrap(),
        MAX_MESSAGE_BYTES,
    );
    let mut c = Conn::open(&server.path).await;
    for (method, params) in [
        ("projects.list", json!({})),
        ("projects.add", json!({"path": "/x"})),
        ("projects.remove", json!({"project": "x"})),
        ("profiles.list", json!({"project": "x"})),
        ("profiles.create", json!({"project": "x", "name": "y"})),
        ("profiles.update", json!({"project": "x", "profile": "y"})),
        ("profiles.delete", json!({"project": "x", "profile": "y"})),
        ("limits.get", json!({})),
        ("limits.set", json!({"cpu_cores": 2})),
        (
            "agents.start",
            json!({"project": "x", "profile": "y", "prompt": "z"}),
        ),
        ("agents.list", json!({})),
        ("agents.delete", json!({"ref": "x"})),
    ] {
        let mut params = params;
        params["host"] = json!("vps");
        let r = c.call(method, params).await;
        assert_eq!(code(&r), -32001, "{method}: {r}");
        assert_eq!(r["error"]["message"], "no host named vps", "{method}");
    }
    let r = c.call("limits.get", json!({"host": LOCAL_HOST})).await;
    ok(&r);
}

#[tokio::test]
async fn projects_with_agents_cannot_be_removed() {
    let golden = Golden::new();
    let core = golden.core();
    core.add_project(vibe_core::rpc::ProjectAddParams {
        path: Some(golden.home.join("src/app").to_str().unwrap().into()),
        ..Default::default()
    })
    .await
    .unwrap();
    // #4 adds agents; here one goes straight into the registry.
    drop(core);
    {
        let layout = Layout::new(&golden.home, "macos");
        let mut store = Store::open(&layout.db()).unwrap();
        store
            .add_agent(&AgentRecord {
                host: LOCAL_HOST.into(),
                agent_ref: "quiet-amber-otter".into(),
                project: "app".into(),
                profile: "software-eng-linux".into(),
                environment: vibe_core::model::Environment::Linux,
                model: vibe_core::model::Model::Opus,
                reasoning: vibe_core::model::Reasoning::High,
                mode: vibe_core::model::Mode::Auto,
                memory: 1 << 30,
                worktree: "/w".into(),
                branch: "quiet-amber-otter".into(),
                session_id: "s".into(),
                shim_socket: None,
                state: vibe_core::model::AgentState::Idle,
                created_at: 0,
                compartment: "{}".into(),
                starting: false,
            })
            .unwrap();
    }
    let server = serve(
        golden.core(),
        tempfile::tempdir().unwrap(),
        MAX_MESSAGE_BYTES,
    );
    let mut c = Conn::open(&server.path).await;
    let r = c.call("projects.remove", json!({"project": "app"})).await;
    assert_eq!(code(&r), -32002);
    assert_eq!(
        r["error"]["message"],
        "app still has 1 agent; delete it first"
    );
}

#[tokio::test]
async fn github_projects_clone_once_and_leave_nothing_half_done() {
    let golden = Golden::new();
    let core = golden.core();
    let projects_dir = core.layout().projects_dir();
    let server = serve(core, tempfile::tempdir().unwrap(), MAX_MESSAGE_BYTES);
    let mut c = Conn::open(&server.path).await;

    // A clone interrupted by a crash is cleared, not adopted.
    fs::create_dir_all(projects_dir.join(".site.cloning/junk")).unwrap();
    let added = c
        .call(
            "projects.add",
            json!({"repo": "me/web.git", "name": "site"}),
        )
        .await;
    let path = projects_dir.join("site");
    assert_eq!(ok(&added)["path"], path.to_str().unwrap());
    assert_eq!(ok(&added)["repo"], "me/web");
    assert!(path.join("README.md").exists());
    assert!(!path.join("junk").exists());
    let entries: Vec<_> = fs::read_dir(&projects_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(entries, ["site"]);

    // A failed clone leaves nothing.
    let failed = c.call("projects.add", json!({"repo": "me/missing"})).await;
    assert_eq!(code(&failed), -32004);
    assert!(!projects_dir.join("missing").exists());
    assert!(!projects_dir.join(".missing.cloning").exists());

    // Removing keeps the clone, and adding the repo again adopts it.
    fs::write(path.join("work.txt"), "unsaved").unwrap();
    ok(&c.call("projects.remove", json!({"project": "site"})).await);
    assert!(path.join("work.txt").exists(), "remove leaves the files");
    let again = c
        .call("projects.add", json!({"repo": "me/web", "name": "site"}))
        .await;
    assert_eq!(ok(&again)["path"], path.to_str().unwrap());
    assert_eq!(
        fs::read_to_string(path.join("work.txt")).unwrap(),
        "unsaved"
    );

    // A directory that isn't a clone of the repo is never touched.
    fs::create_dir_all(projects_dir.join("mine")).unwrap();
    fs::write(projects_dir.join("mine/notes"), "keep").unwrap();
    let clash = c
        .call("projects.add", json!({"repo": "me/web", "name": "mine"}))
        .await;
    assert_eq!(code(&clash), -32002);
    assert_eq!(
        fs::read_to_string(projects_dir.join("mine/notes")).unwrap(),
        "keep"
    );

    // A path that's already a project can't be added under a second name.
    let twice = c
        .call("projects.add", json!({"path": path, "name": "site2"}))
        .await;
    assert_eq!(code(&twice), -32002);
    for bad in [
        json!({"repo": "not a repo"}),
        json!({"path": "relative/dir"}),
        json!({"path": golden.home}),
        json!({}),
        json!({"repo": "me/web", "name": ".hidden"}),
    ] {
        let r = c.call("projects.add", bad.clone()).await;
        assert_eq!(code(&r), -32602, "{bad}: {r}");
    }
}

#[tokio::test]
async fn host_toml_hand_edits_and_mistakes() {
    let golden = Golden::new();
    let core = golden.core();
    let config = core.layout().host_config();
    let server = serve(core, tempfile::tempdir().unwrap(), MAX_MESSAGE_BYTES);
    let mut c = Conn::open(&server.path).await;

    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "max_awake_agents = 3 # small Mac\n").unwrap();
    assert_eq!(
        ok(&c.call("limits.get", json!({})).await)["max_awake_agents"],
        3
    );

    fs::write(&config, "max_awake_agent = 3\n").unwrap();
    let bad = c.call("limits.get", json!({})).await;
    assert_eq!(code(&bad), -32003);
    assert!(
        bad["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown setting `max_awake_agent`"),
        "{bad}"
    );
    fs::write(&config, "disk_soft = \"99G\"\n").unwrap();
    assert_eq!(code(&c.call("limits.get", json!({})).await), -32003);
    // Setting the bad value right fixes the file.
    let fixed = c.call("limits.set", json!({"disk_soft": 10 * G})).await;
    assert_eq!(ok(&fixed)["disk_soft"], 10 * G);
    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        "disk_soft = \"10G\"\n"
    );

    for bad in [
        json!({}),
        json!({"cpu_cores": 0}),
        json!({"cpu_cores": -1}),
        json!({"vm_disk": 1}),
    ] {
        let r = c.call("limits.set", bad.clone()).await;
        assert_eq!(code(&r), -32602, "{bad}: {r}");
    }
    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        "disk_soft = \"10G\"\n"
    );
}

#[tokio::test]
async fn key_secrets_are_never_stored() {
    let golden = Golden::new();
    let core = golden.core();
    let db = core.layout().db();
    let created = core
        .create_key(vibe_core::rpc::KeyCreateParams {
            name: "laptop".into(),
        })
        .unwrap();
    assert!(vibe_core::keys::is_key(&created.secret));
    assert_eq!(created.key.prefix, &created.secret[..11]);
    let mut stored = Vec::new();
    for suffix in ["", "-wal"] {
        let mut name = db.as_os_str().to_owned();
        name.push(suffix);
        stored.extend(fs::read(name).unwrap_or_default());
    }
    assert!(!stored.is_empty());
    let secret = created.secret.as_bytes();
    assert!(
        !stored.windows(secret.len()).any(|w| w == secret),
        "the secret is in vibe.db"
    );
    assert!(core.verify_key(&created.secret).unwrap().is_some());
    assert!(core.verify_key("vk_nope").unwrap().is_none());
    assert!(
        core.verify_key(&created.secret.to_uppercase())
            .unwrap()
            .is_none()
    );

    let dup = core.create_key(vibe_core::rpc::KeyCreateParams {
        name: "laptop".into(),
    });
    assert_eq!(dup.unwrap_err().code, -32002);
    // Key names are for people: spaces are fine, blank ones and line
    // breaks aren't.
    let spaced = core.create_key(vibe_core::rpc::KeyCreateParams {
        name: "laptop Claude Code".into(),
    });
    assert_eq!(spaced.unwrap().key.name, "laptop Claude Code");
    for name in ["", " ", "a\nb"] {
        let bad = core.create_key(vibe_core::rpc::KeyCreateParams { name: name.into() });
        assert_eq!(bad.unwrap_err().code, -32602, "{name:?}");
    }
}

#[tokio::test]
async fn gh_is_checked_on_a_schedule_and_changes_are_events() {
    let golden = Golden::new();
    let core = golden.core();
    let (_, mut events) = core.subscribe();
    let before = core.gh_status(GhStatusParams::default()).await;
    assert_eq!(before.state, GhState::Unknown);
    assert_eq!(before.checked_at, None);

    let checks = core.spawn_gh_checks(Duration::from_millis(50));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let first = tokio::time::timeout_at(deadline, events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.kind, EventKind::GhStatus);
    assert_eq!(first.state.as_deref(), Some("logged_in"));
    // Later checks find the same, so they send nothing more.
    tokio::time::sleep(Duration::from_millis(300)).await;
    checks.abort();
    assert!(events.try_recv().is_err());
    let now = core.gh_status(GhStatusParams::default()).await;
    assert_eq!(
        (now.state, now.account.as_deref()),
        (GhState::LoggedIn, Some("me"))
    );
}

#[tokio::test]
async fn gh_logging_out_is_an_event() {
    let golden = Golden::new();
    let core = golden.core();
    assert_eq!(core.check_gh().await.state, GhState::LoggedIn);
    let (_, mut events) = core.subscribe();
    // Swap in a gh that says it's logged out.
    let script = "#!/bin/sh\necho 'You are not logged into any GitHub hosts. To log in, run: gh auth login' >&2\nexit 1\n";
    common::write_script(&golden.gh, script);
    let status = core.check_gh().await;
    assert_eq!(status.state, GhState::LoggedOut);
    let e = events.try_recv().unwrap();
    assert_eq!(
        (e.kind, e.state.as_deref()),
        (EventKind::GhStatus, Some("logged_out"))
    );
}
