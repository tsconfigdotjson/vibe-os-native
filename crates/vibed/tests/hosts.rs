//! A Mac and a Linux host in one process, joined the way `ssh <host> vibe
//! rpc` joins them, by a carrier the tests can cut or freeze: the link, the
//! calls that go over it, the host's agents and events in the Mac's view,
//! the API keys pushed to it, and what happens when the network goes.

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Conn, Golden, TestServer, serve};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;
use vibe_core::model::{AgentState, EventKind, HostLink};
use vibe_core::paths::Layout;
use vibe_core::rpc::{
    AgentStartParams, ErrorCode, HostAddParams, HostParams, KeyCreateParams, KeyParams,
    MAX_MESSAGE_BYTES, MemoryUse,
};
use vibe_core::units::GIB;
use vibe_mcp::{Daemon, Input, Target};
use vibed::core::Core;
use vibed::link::Timing;
use vibed::remote::{BoxFuture, Channel, Installed, Remote};

/// What the carrier between the two does with bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wire {
    Up,
    /// Holds everything, as a pulled cable does: no data, no close.
    Frozen,
    /// Drops every connection.
    Cut,
}

/// `ssh`, in process: each dial connects to the host's socket through a
/// pump the test controls.
#[derive(Debug)]
struct Wires {
    sockets: Mutex<HashMap<String, PathBuf>>,
    reachable: AtomicBool,
    wire: watch::Sender<Wire>,
    installs: Mutex<u32>,
}

impl Wires {
    fn new() -> Arc<Wires> {
        Arc::new(Wires {
            sockets: Mutex::new(HashMap::new()),
            reachable: AtomicBool::new(true),
            wire: watch::channel(Wire::Up).0,
            installs: Mutex::new(0),
        })
    }

    /// The network goes: connections drop, and new ones can't be made.
    fn pull(&self) {
        self.reachable.store(false, Ordering::SeqCst);
        self.wire.send_replace(Wire::Cut);
    }

    /// Connections hang, with nothing sent or closed.
    fn freeze(&self) {
        self.wire.send_replace(Wire::Frozen);
    }

    fn plug_in(&self) {
        self.wire.send_replace(Wire::Up);
        self.reachable.store(true, Ordering::SeqCst);
    }
}

impl Remote for Wires {
    fn dial<'a>(&'a self, address: &'a str) -> BoxFuture<'a, Result<Channel, String>> {
        Box::pin(async move {
            if !self.reachable.load(Ordering::SeqCst) {
                return Err(format!(
                    "ssh: connect to host {address} port 22: Network is unreachable"
                ));
            }
            let socket = self.sockets.lock().unwrap().get(address).cloned();
            let socket =
                socket.ok_or_else(|| format!("ssh: Could not resolve hostname {address}"))?;
            let stream = tokio::net::UnixStream::connect(&socket)
                .await
                .map_err(|e| format!("vibe: can't reach vibed: {e}"))?;
            // The wire's state now, before a dial in a frozen spell sees it.
            if *self.wire.borrow() == Wire::Cut {
                self.wire.send_replace(Wire::Up);
            }
            let (ours, theirs) = tokio::io::duplex(1 << 20);
            let mut wire = self.wire.subscribe();
            tokio::spawn(async move {
                let (mut sr, mut sw) = stream.into_split();
                let (mut tr, mut tw) = tokio::io::split(theirs);
                let mut a = vec![0u8; 1 << 16];
                let mut b = vec![0u8; 1 << 16];
                loop {
                    let now = *wire.borrow_and_update();
                    match now {
                        Wire::Cut => return,
                        Wire::Frozen => {
                            if wire.changed().await.is_err() {
                                return;
                            }
                            continue;
                        }
                        Wire::Up => {}
                    }
                    tokio::select! {
                        _ = wire.changed() => {}
                        n = sr.read(&mut a) => match n {
                            Ok(0) | Err(_) => return,
                            Ok(n) => if tw.write_all(&a[..n]).await.is_err() { return },
                        },
                        n = tr.read(&mut b) => match n {
                            Ok(0) | Err(_) => return,
                            Ok(n) => if sw.write_all(&b[..n]).await.is_err() { return },
                        },
                    }
                }
            });
            let (r, w) = tokio::io::split(ours);
            Ok(Channel::new(
                BufReader::new(r),
                w,
                (),
                Arc::new(Mutex::new(String::new())),
            ))
        })
    }

    fn install<'a>(&'a self, address: &'a str) -> BoxFuture<'a, Result<Installed, String>> {
        Box::pin(async move {
            *self.installs.lock().unwrap() += 1;
            if !self.sockets.lock().unwrap().contains_key(address) {
                return Err(format!("can't reach {address} over SSH: no such host."));
            }
            Ok(Installed {
                os: "linux".into(),
                arch: "x86_64".into(),
                label: "vps-1".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                missing: vec!["gh".into()],
                changed: true,
                notes: vec![],
            })
        })
    }
}

/// An outside caller, as MCP passes one: these calls never reach a tunnel,
/// so its key isn't checked again.
fn caller() -> vibe_mcp::Caller {
    vibe_mcp::Caller::new("laptop", "vk_test")
}

fn quick() -> Timing {
    Timing {
        ping_every: Duration::from_millis(200),
        ping_timeout: Duration::from_millis(400),
        connect_timeout: Duration::from_secs(5),
        min_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(300),
        ..Timing::default()
    }
}

struct World {
    mac: Arc<Core>,
    host: Arc<Core>,
    wires: Arc<Wires>,
    _mac_world: Golden,
    host_world: Golden,
    _server: TestServer,
}

/// A Mac and, at `me@vps.example.com`, a Linux host with the project
/// `app`, both running agents under the test backend.
async fn world() -> World {
    world_with(|_| {}).await
}

/// [`world`], with the Linux host's config changed by `change`.
async fn world_with(change: impl FnOnce(&mut vibed::core::Config)) -> World {
    let wires = Wires::new();
    let mac_world = Golden::new();
    let mut config = mac_world.config();
    config.remote = Arc::clone(&wires) as Arc<dyn Remote>;
    config.link_timing = quick();
    let mac = Arc::new(Core::open(config).unwrap());
    mac.spawn_links();

    let host_world = Golden::new();
    let mut config = host_world.config();
    config.info.os = "linux".into();
    config.info.version = env!("CARGO_PKG_VERSION").into();
    config.label = "vps-1".into();
    config.layout = Layout::new(&host_world.home, "linux");
    config.toolbox = None;
    change(&mut config);
    let host = Arc::new(Core::open(config).unwrap());
    let server = serve(
        Arc::clone(&host),
        common::short_tempdir(),
        MAX_MESSAGE_BYTES,
    );
    wires
        .sockets
        .lock()
        .unwrap()
        .insert("me@vps.example.com".into(), server.path.clone());
    World {
        mac,
        host,
        wires,
        _mac_world: mac_world,
        host_world,
        _server: server,
    }
}

impl World {
    async fn add_host(&self) {
        let added = self
            .mac
            .add_host(HostAddParams {
                name: "vps".into(),
                address: "me@vps.example.com".into(),
            })
            .await
            .unwrap();
        assert_eq!(added.host.address.as_deref(), Some("me@vps.example.com"));
        assert_eq!(added.host.missing, ["gh"]);
        self.wait_link(HostLink::Connected).await;
    }

    async fn link(&self) -> vibe_core::rpc::HostInfo {
        self.mac
            .hosts()
            .unwrap()
            .hosts
            .into_iter()
            .find(|h| h.name == "vps")
            .unwrap()
    }

    async fn wait_link(&self, want: HostLink) -> vibe_core::rpc::HostInfo {
        for _ in 0..500 {
            let h = self.link().await;
            if h.link == want {
                return h;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("vps's link never became {want}: {:?}", self.link().await);
    }

    async fn host_project(&self) {
        let path = self.host_world.home.join("src/app");
        self.mac
            .forward(
                &self.mac.link("vps").unwrap(),
                "projects.add",
                json!({"host": "vps", "path": path}),
            )
            .await
            .unwrap();
    }

    async fn rpc(&self, method: &str, params: Value) -> Value {
        rpc(&self.mac, method, params).await
    }
}

/// One call through `core`'s dispatcher, as a socket client makes it.
async fn rpc(core: &Core, method: &str, params: Value) -> Value {
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    let reply = vibed::dispatch::handle_message(
        core,
        &mut vibed::dispatch::Session::default(),
        line.as_bytes(),
    )
    .await
    .unwrap();
    serde_json::from_str(&reply).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_linux_hosts_agents_join_the_macs_view_over_the_link() {
    let w = world().await;
    // Kept for the whole test: dropping it stops the server.
    let server = serve(
        Arc::clone(&w.mac),
        common::short_tempdir(),
        MAX_MESSAGE_BYTES,
    );
    let mut events = Conn::open(&server.path).await;
    assert!(events.call("events.subscribe", json!({})).await["result"].is_object());
    w.add_host().await;
    let h = w.link().await;
    assert_eq!(h.label, "vps-1");
    assert_eq!(h.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    assert_eq!(h.detail, None);
    assert!(!h.mcp, "the host's vibed here serves no MCP");

    // Calls that name the host go to its vibed, and come back in its name.
    w.host_project().await;
    let projects = w.rpc("projects.list", json!({"host": "vps"})).await;
    let p = &projects["result"]["projects"][0];
    assert_eq!(p["host"], "vps");
    assert_eq!(p["name"], "app");
    // A Linux host's projects start with one linux profile.
    assert_eq!(p["profiles"].as_array().unwrap().len(), 1);
    let missing = w.rpc("projects.list", json!({"host": "vps", "x": 1})).await;
    assert_eq!(
        missing["error"]["code"],
        ErrorCode::InvalidParams.code(),
        "the host checks them"
    );
    let gone = w
        .rpc("profiles.list", json!({"host": "vps", "project": "nope"}))
        .await;
    assert_eq!(gone["error"]["code"], ErrorCode::NotFound.code());
    assert!(
        gone["error"]["message"]
            .as_str()
            .unwrap()
            .contains("on vps"),
        "{gone}"
    );

    // An agent started through the Mac's MCP lives on the host.
    let agent = Daemon::start_agent(
        &*w.mac,
        &caller(),
        AgentStartParams {
            host: Some("vps".into()),
            project: "app".into(),
            profile: "software-eng-linux".into(),
            prompt: "hello there".into(),
            from: None,
            name: Some("remote-one".into()),
        },
    )
    .await
    .unwrap();
    assert_eq!(agent.host, "vps");
    assert_eq!(
        agent.attach,
        "ssh -t me@vps.example.com .local/lib/vibeos/vibe attach remote-one"
    );
    let on_host = w.host.agents(HostParams::default()).await.unwrap().agents;
    assert_eq!(on_host[0].agent, "remote-one", "recorded on the host");
    assert_eq!(on_host[0].host, "local", "where it's local");
    let on_mac = w.mac.agents(HostParams::default()).await.unwrap().agents;
    assert!(on_mac.is_empty(), "and not on the Mac");

    // Its events reach the Mac's subscribers under the host's name.
    loop {
        let e = events.reply().await;
        if e["params"]["kind"] == "agent.added" {
            assert_eq!(e["params"]["host"], "vps");
            assert_eq!(e["params"]["ref"], "remote-one");
            break;
        }
    }

    // The menu's listing, and MCP's.
    let list = w.rpc("agents.list", json!({"host": "vps"})).await;
    let listed = &list["result"]["agents"][0];
    assert_eq!(listed["ref"], "remote-one");
    assert_eq!(listed["host"], "vps");
    assert_eq!(
        listed["attach"],
        "ssh -t me@vps.example.com .local/lib/vibeos/vibe attach remote-one"
    );
    let views = Daemon::agents(&*w.mac, &caller(), "vps").await.unwrap();
    assert!(views[0].available);
    assert_eq!(views[0].attach_remote, views[0].agent.attach);
    assert_eq!(Daemon::hosts(&*w.mac).await.unwrap(), ["local", "vps"]);
    let attach = w
        .rpc(
            "sessions.attach",
            json!({"host": "vps", "ref": "remote-one"}),
        )
        .await;
    assert_eq!(attach["result"]["host"], "vps");

    // Talking to it goes over the link too: send, wait, read.
    let t = Target {
        host: "vps".into(),
        agent: "remote-one".into(),
    };
    for _ in 0..200 {
        let screen = Daemon::read_output(&*w.mac, &caller(), &t, None)
            .await
            .unwrap();
        if screen.screen.contains("Ready.") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let sent = Daemon::send(
        &*w.mac,
        &caller(),
        &t,
        Input::Text {
            text: "ping".into(),
            submit: true,
            force: false,
        },
        "ping-1",
    )
    .await
    .unwrap();
    assert!(!sent.woke);
    let mut echoed = false;
    for _ in 0..200 {
        let screen = Daemon::read_output(&*w.mac, &caller(), &t, None)
            .await
            .unwrap();
        if screen.screen.contains("> ping") {
            echoed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(echoed, "the send reached the agent on the host");
    Daemon::send(
        &*w.mac,
        &caller(),
        &t,
        Input::Keys(vec!["y".into(), "Enter".into()]),
        "keys-1",
    )
    .await
    .unwrap();
    // A request id gets over the link to the host that types: there, it knows this
    // one was for something else, and nothing more is typed.
    let reused = Daemon::send(
        &*w.mac,
        &caller(),
        &t,
        Input::Keys(vec!["n".into()]),
        "keys-1",
    )
    .await
    .unwrap_err();
    assert_eq!(reused.code, ErrorCode::Conflict.code(), "{reused:?}");
    assert!(reused.message.contains("different send"), "{reused:?}");
    let check = Daemon::check(&*w.mac, &caller(), &t, false).await.unwrap();
    assert_ne!(check.state, AgentState::Exited);
    assert!(
        Daemon::read_messages(&*w.mac, &caller(), &t, 5)
            .await
            .unwrap()
            .is_empty(),
        "the stand-in writes none"
    );

    // Keys made on the Mac work on the host; a revoke reaches it at once.
    let made = w
        .mac
        .create_key(KeyCreateParams {
            name: "laptop".into(),
        })
        .unwrap();
    let mut pushed = false;
    for _ in 0..250 {
        if w.host.verify_key(&made.secret).unwrap().is_some() {
            pushed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(pushed, "the key reached the host");
    let on_host = w.host.keys().unwrap().keys;
    assert!(
        on_host.iter().any(|k| k.name == "laptop" && k.pushed),
        "{on_host:?}"
    );
    // It was used there, not on the Mac: the Mac's list asks the host, and
    // keeps what it said.
    let laptop =
        |keys: Vec<vibe_core::rpc::ApiKey>| keys.into_iter().find(|k| k.name == "laptop").unwrap();
    let used_there = laptop(on_host).last_used_at;
    assert!(used_there.is_some(), "the host recorded the use");
    assert_eq!(laptop(w.mac.keys().unwrap().keys).last_used_at, None);
    // Each listing gives the host a few seconds and then goes without it, as it
    // should, and a busy machine can use them up. So it's asked again here; a
    // host that never says fails, and so does a listing that doesn't end.
    let mut listed = Value::Null;
    for _ in 0..10 {
        listed = tokio::time::timeout(Duration::from_secs(20), w.rpc("keys.list", json!({})))
            .await
            .expect("a listing doesn't wait on a host for long");
        if listed["result"]["keys"][0]["last_used_at"].as_str() == used_there.as_deref() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        listed["result"]["keys"][0]["last_used_at"].as_str(),
        used_there.as_deref(),
        "{listed}"
    );
    assert_eq!(
        laptop(w.mac.keys().unwrap().keys).last_used_at,
        used_there,
        "kept on the Mac"
    );
    let refused = w
        .host
        .revoke_key(KeyParams {
            name: "laptop".into(),
        })
        .unwrap_err();
    assert_eq!(
        refused.code,
        ErrorCode::Conflict.code(),
        "revoked on the Mac, not here"
    );
    w.mac
        .revoke_key(KeyParams {
            name: "laptop".into(),
        })
        .unwrap();
    let mut revoked = false;
    for _ in 0..250 {
        if w.host.verify_key(&made.secret).unwrap().is_none() {
            revoked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(revoked, "the revoke reached the host");

    // Deleting it there goes over the link as well.
    let deleted = w
        .rpc(
            "agents.delete",
            json!({"host": "vps", "ref": "remote-one", "force": true}),
        )
        .await;
    assert_eq!(deleted["result"]["host"], "vps", "{deleted}");
    assert!(
        w.host
            .agents(HostParams::default())
            .await
            .unwrap()
            .agents
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pulled_network_shows_the_host_down_and_it_reconnects() {
    let w = world().await;
    w.add_host().await;
    w.host_project().await;
    w.rpc(
        "agents.start",
        json!({"host": "vps", "project": "app", "profile": "software-eng-linux", "prompt": "hi", "name": "keeps-going"}),
    )
    .await;
    assert_eq!(
        w.rpc("agents.list", json!({"host": "vps"})).await["result"]["agents"][0]["ref"],
        "keeps-going"
    );

    w.wires.pull();
    let down = w.wait_link(HostLink::Down).await;
    let why = down.detail.unwrap();
    assert!(!why.is_empty(), "it says why");
    // Its agents show as they were last seen, unavailable; calls fail.
    let list = w.rpc("agents.list", json!({"host": "vps"})).await;
    assert_eq!(list["result"]["agents"][0]["ref"], "keeps-going");
    let views = Daemon::agents(&*w.mac, &caller(), "vps").await.unwrap();
    assert!(!views[0].available);
    let refused = w.rpc("projects.list", json!({"host": "vps"})).await;
    assert_eq!(refused["error"]["code"], ErrorCode::HostUnavailable.code());
    // Removing a project there says it can't, and the host keeps it.
    let kept = w
        .rpc("projects.remove", json!({"host": "vps", "project": "app"}))
        .await;
    assert_eq!(
        kept["error"]["code"],
        ErrorCode::HostUnavailable.code(),
        "{kept}"
    );
    assert!(
        kept["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("vps is unavailable"),
        "{kept}"
    );
    assert_eq!(
        w.host
            .projects(HostParams::default())
            .unwrap()
            .projects
            .len(),
        1
    );
    // Nothing stopped on the host.
    let there = w.host.agents(HostParams::default()).await.unwrap().agents;
    assert_eq!(there[0].agent, "keeps-going");
    assert_ne!(there[0].state, AgentState::Exited);
    // While it's unreachable, reconnects keep failing and it stays down.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let still = w.link().await;
    assert_eq!(still.link, HostLink::Down);
    assert!(still.detail.unwrap().contains("Network is unreachable"));

    w.wires.plug_in();
    w.wait_link(HostLink::Connected).await;
    let back = w.rpc("projects.list", json!({"host": "vps"})).await;
    assert_eq!(back["result"]["projects"][0]["host"], "vps");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_linux_hosts_memory_comes_with_its_pings_and_is_no_link_event() {
    // The host's own memory, as its kernel would measure it: 12G, with
    // `used` moving as the test says.
    let used = Arc::new(AtomicU64::new(5 * GIB));
    let measured = Arc::clone(&used);
    let w = world_with(move |c| {
        c.memory = Arc::new(move || {
            Some(MemoryUse {
                total: 12 * GIB,
                used: measured.load(Ordering::SeqCst),
                swap_total: 2 * GIB,
                swap_used: GIB,
                pressure: None,
            })
        });
        c.pressure = Arc::new(vibed::pressure::NoPressure);
    })
    .await;
    w.add_host().await;
    // The Mac's hosts.list carries what the host said as its link came up.
    let listed = w.rpc("hosts.list", json!({})).await;
    let vps = listed["result"]["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["name"] == "vps")
        .unwrap()
        .clone();
    assert_eq!(
        vps["memory"],
        json!({"total": 12 * GIB, "used": 5 * GIB, "swap_total": 2 * GIB, "swap_used": GIB}),
        "{vps}"
    );

    // Only its memory changes: the next ping brings it, and it's no change
    // of the link, so no host.link event.
    let (_, mut events) = w.mac.subscribe();
    used.store(7 * GIB, Ordering::SeqCst);
    let mut seen = None;
    for _ in 0..250 {
        seen = w.link().await.memory.map(|m| m.used);
        if seen == Some(7 * GIB) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        seen,
        Some(7 * GIB),
        "the ping never brought the new reading"
    );
    // A few more pings, all the same.
    tokio::time::sleep(Duration::from_millis(600)).await;
    while let Ok(e) = events.try_recv() {
        assert_ne!(e.kind, EventKind::HostLink, "{e:?}");
    }

    // Down, its memory isn't known: nothing stale is shown.
    w.wires.pull();
    let down = w.wait_link(HostLink::Down).await;
    assert_eq!(down.memory, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_that_stops_answering_is_down_until_it_answers_again() {
    let w = world().await;
    w.add_host().await;
    w.wires.freeze();
    // Nothing closes; the pings notice.
    let down = w.wait_link(HostLink::Down).await;
    assert!(down.detail.unwrap().contains("stopped answering"));
    w.wires.plug_in();
    w.wait_link(HostLink::Connected).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosts_are_added_once_and_removed_without_touching_the_server() {
    let w = world().await;
    let bad = w
        .mac
        .add_host(HostAddParams {
            name: "local".into(),
            address: "me@vps.example.com".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(bad.code, ErrorCode::InvalidParams.code());
    let bad = w
        .mac
        .add_host(HostAddParams {
            name: "vps".into(),
            address: "-oProxyCommand=evil".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(bad.code, ErrorCode::InvalidParams.code());
    // A failed install records nothing.
    let failed = w
        .mac
        .add_host(HostAddParams {
            name: "nope".into(),
            address: "me@nowhere.example.com".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(failed.code, ErrorCode::CommandFailed.code());
    assert!(
        failed.message.contains("no such host"),
        "{}",
        failed.message
    );
    assert_eq!(w.mac.hosts().unwrap().hosts.len(), 1);

    w.add_host().await;
    let again = w
        .mac
        .add_host(HostAddParams {
            name: "vps".into(),
            address: "me@vps.example.com".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(again.code, ErrorCode::Conflict.code());
    let installs = *w.wires.installs.lock().unwrap();
    let upgraded = w.rpc("hosts.upgrade", json!({"host": "vps"})).await;
    assert_eq!(upgraded["result"]["host"]["name"], "vps", "{upgraded}");
    assert_eq!(*w.wires.installs.lock().unwrap(), installs + 1);

    // The Mac's keys work there while it's linked, and stop when it's
    // forgotten: a revoke here couldn't reach it any more.
    let made = w
        .mac
        .create_key(KeyCreateParams {
            name: "laptop".into(),
        })
        .unwrap();
    let mut pushed = false;
    for _ in 0..250 {
        if w.host.verify_key(&made.secret).unwrap().is_some() {
            pushed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(pushed, "the key reached the host");
    let removed = w.rpc("hosts.remove", json!({"host": "vps"})).await;
    assert_eq!(removed["result"]["name"], "vps", "{removed}");
    assert!(
        w.host.verify_key(&made.secret).unwrap().is_none(),
        "the Mac's keys came off the host it forgot"
    );
    assert_eq!(w.mac.hosts().unwrap().hosts.len(), 1);
    let unknown = w.rpc("projects.list", json!({"host": "vps"})).await;
    assert_eq!(unknown["error"]["code"], ErrorCode::NotFound.code());
    // The host itself runs on.
    assert!(w.host.hosts().is_ok());
    let local = w.rpc("hosts.remove", json!({"host": "local"})).await;
    assert_eq!(local["error"]["code"], ErrorCode::InvalidParams.code());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mac_restarted_keeps_its_hosts_and_links_again() {
    let wires = Wires::new();
    let mac_world = Golden::new();
    let host_world = Golden::new();
    let mut config = host_world.config();
    config.info.os = "linux".into();
    config.layout = Layout::new(&host_world.home, "linux");
    config.toolbox = None;
    let host = Arc::new(Core::open(config).unwrap());
    let server = serve(
        Arc::clone(&host),
        common::short_tempdir(),
        MAX_MESSAGE_BYTES,
    );
    wires
        .sockets
        .lock()
        .unwrap()
        .insert("me@vps.example.com".into(), server.path.clone());
    // The state lock is free once every task holding the last Core is done.
    let open = async || {
        for _ in 0..100 {
            let mut config = mac_world.config();
            config.remote = Arc::clone(&wires) as Arc<dyn Remote>;
            config.link_timing = quick();
            if let Ok(mac) = Core::open(config) {
                let mac = Arc::new(mac);
                mac.spawn_links();
                return mac;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("the Mac's state stayed locked");
    };
    let mac = open().await;
    mac.add_host(HostAddParams {
        name: "vps".into(),
        address: "me@vps.example.com".into(),
    })
    .await
    .unwrap();
    for _ in 0..500 {
        if mac.link("vps").is_some_and(|l| l.connected()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let path = host_world.home.join("src/app");
    rpc(&mac, "projects.add", json!({"host": "vps", "path": path})).await;
    let started = rpc(
        &mac,
        "agents.start",
        json!({"host": "vps", "project": "app", "profile": "software-eng-linux", "prompt": "hi", "name": "kept-one"}),
    )
    .await;
    assert_eq!(started["result"]["host"], "vps", "{started}");
    rpc(&mac, "agents.list", json!({"host": "vps"})).await;
    drop(mac);
    wires.pull();
    let mac = open().await;
    // While the host is out of reach, its agents are as last listed.
    let kept = Daemon::agents(&*mac, &caller(), "vps").await.unwrap();
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert!(!kept[0].available);
    wires.plug_in();
    for _ in 0..500 {
        if mac.link("vps").is_some_and(|l| l.connected()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the restarted Mac didn't link to vps");
}
