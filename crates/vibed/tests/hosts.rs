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
use vibe_core::model::{AdminNeed, AgentState, CheckId, CheckState, EventKind, HostLink};
use vibe_core::paths::Layout;
use vibe_core::rpc::{
    AgentStartParams, ErrorCode, HostAddParams, HostNameParams, HostParams, KeyCreateParams,
    KeyParams, MAX_MESSAGE_BYTES, MemoryUse,
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
    /// What an install fails with, while it does.
    install_fails: Mutex<Option<String>>,
    /// The Mac's bundle holds an older build than the Mac.
    older_bundle: AtomicBool,
    /// It holds just what the Mac runs, the Mac's being [`upgrades::MAC`].
    macs_bundle: AtomicBool,
}

impl Wires {
    fn new() -> Arc<Wires> {
        Arc::new(Wires {
            sockets: Mutex::new(HashMap::new()),
            reachable: AtomicBool::new(true),
            wire: watch::channel(Wire::Up).0,
            installs: Mutex::new(0),
            install_fails: Mutex::new(None),
            older_bundle: AtomicBool::new(false),
            macs_bundle: AtomicBool::new(false),
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
            if let Some(why) = self.install_fails.lock().unwrap().clone() {
                return Err(why);
            }
            if !self.sockets.lock().unwrap().contains_key(address) {
                return Err(format!("can't reach {address} over SSH: no such host."));
            }
            Ok(Installed {
                os: "linux".into(),
                arch: "x86_64".into(),
                label: "vps-1".into(),
                version: if self.older_bundle.load(Ordering::SeqCst) {
                    "0.0.5".into()
                } else if self.macs_bundle.load(Ordering::SeqCst) {
                    upgrades::MAC.into()
                } else {
                    env!("CARGO_PKG_VERSION").into()
                },
                missing: vec!["gh".into()],
                needs_admin: Vec::new(),
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
        upgrade_gap: Duration::from_millis(50),
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
    world_timed(quick(), change).await
}

/// [`world_with`], with the Mac's link timed by `timing`.
async fn world_timed(timing: Timing, change: impl FnOnce(&mut vibed::core::Config)) -> World {
    let wires = Wires::new();
    let mac_world = Golden::new();
    let mut config = mac_world.config();
    config.remote = Arc::clone(&wires) as Arc<dyn Remote>;
    config.link_timing = timing;
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

/// A Mac doesn't pass on a Pi sign-in to a host, as nothing has one (#158), but a host's
/// list of models still comes from there, with the login already there seen.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hosts_pi_list_comes_from_there_and_no_sign_in_is_passed_on() {
    let w = world().await;
    w.add_host().await;
    let own = w.host_world.home.join(".pi/agent");
    std::fs::create_dir_all(&own).unwrap();
    let login = r#"{"openai":{"type":"api_key","key":"k"}}"#;
    std::fs::write(own.join("auth.json"), login).unwrap();

    let list = w
        .rpc("pi.models", json!({"host": "vps", "target": "vm"}))
        .await;
    assert_eq!(list["result"]["host"], "vps", "{list}");
    assert_eq!(
        list["result"]["target"], "native",
        "a host has no VMs: {list}"
    );
    let openai = list["result"]["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "openai")
        .unwrap_or_else(|| panic!("{list}"))
        .clone();
    assert_eq!(openai["signed_in"], true, "{list}");

    let tried = w
        .rpc(
            "pi.login.start",
            json!({"host": "vps", "target": "vm", "provider": "anthropic", "method": "oauth"}),
        )
        .await;
    assert_eq!(
        tried["error"]["code"],
        vibe_core::rpc::ErrorCode::MethodNotFound.code(),
        "{tried}"
    );
    assert_eq!(
        std::fs::read_to_string(own.join("auth.json")).unwrap(),
        login
    );
}

/// Section C, on the host: keys the Mac pushed say so when it's been away a while, and
/// one that's leaked can be shut out here, and stays out when the Mac's back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_with_the_mac_away_can_see_stale_keys_and_revoke_them() {
    let now = Arc::new(std::sync::atomic::AtomicI64::new(common::GOLDEN_NOW));
    let clock = Arc::clone(&now);
    let w = world_with(move |c| {
        c.clock = Arc::new(move || clock.load(Ordering::SeqCst));
        // Each side's own, or this host's key would be the same as the Mac's.
        c.entropy = Arc::new(vibe_core::keys::random_bytes);
    })
    .await;
    w.add_host().await;
    w.link().await;
    let leaked = w
        .mac
        .create_key(KeyCreateParams {
            name: "leaked".into(),
        })
        .unwrap();
    let other = w
        .mac
        .create_key(KeyCreateParams {
            name: "other".into(),
        })
        .unwrap();
    let mut pushed = false;
    for _ in 0..250 {
        if w.host.verify_key(&other.secret).unwrap().is_some() {
            pushed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(pushed, "the keys reached the host");
    let own = w
        .host
        .create_key(KeyCreateParams { name: "own".into() })
        .unwrap();
    let stale = |name: &str| {
        w.host
            .keys()
            .unwrap()
            .keys
            .into_iter()
            .find(|k| k.name == name)
            .unwrap()
            .stale
    };
    assert!(!stale("leaked"));

    // Well past the limit, but with the link up: it's still hearing from the Mac.
    now.fetch_add(11 * 60, Ordering::SeqCst);
    let mut heard = false;
    for _ in 0..250 {
        if !stale("leaked") {
            heard = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(heard, "a ping says the Mac's there");

    // The Mac goes.
    w.wires.pull();
    w.wait_link(HostLink::Down).await;
    assert!(!stale("leaked"), "not at once");
    now.fetch_add(11 * 60, Ordering::SeqCst);
    assert!(stale("leaked"));
    assert!(stale("other"));
    assert!(!stale("own"), "this host's own say what they say");

    // The leaked one is shut out here, with the Mac nowhere to be seen.
    let gone = w
        .host
        .revoke_key(KeyParams {
            name: "leaked".into(),
        })
        .unwrap();
    assert!(gone.pushed);
    assert_eq!(w.host.verify_key(&leaked.secret).unwrap(), None);
    assert!(w.host.verify_key(&other.secret).unwrap().is_some());
    assert!(w.host.verify_key(&own.secret).unwrap().is_some());

    // The Mac's back, still with it: its push doesn't bring it back here.
    w.wires.plug_in();
    w.wait_link(HostLink::Connected).await;
    let mut fresh = false;
    for _ in 0..500 {
        if !stale("other") {
            fresh = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(fresh, "heard from, nothing's stale");
    assert!(w.mac.verify_key(&leaked.secret).unwrap().is_some());
    // A new key makes it push again, with the leaked one in it.
    let newer = w
        .mac
        .create_key(KeyCreateParams {
            name: "newer".into(),
        })
        .unwrap();
    let mut pushed = false;
    for _ in 0..250 {
        if w.host.verify_key(&newer.secret).unwrap().is_some() {
            pushed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(pushed, "that push arrived");
    assert_eq!(w.host.verify_key(&leaked.secret).unwrap(), None);
    assert!(
        !w.host
            .keys()
            .unwrap()
            .keys
            .iter()
            .any(|k| k.name == "leaked")
    );
}

/// Section C, on the Mac: a revoke made with a host away says it's not there yet, until
/// it's arrived.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoke_with_a_host_away_says_its_pending_until_it_arrives() {
    let w = world().await;
    w.add_host().await;
    w.link().await;
    let key = w
        .mac
        .create_key(KeyCreateParams {
            name: "laptop".into(),
        })
        .unwrap();
    let mut pushed = false;
    for _ in 0..250 {
        if w.host.verify_key(&key.secret).unwrap().is_some() {
            pushed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(pushed);
    let listed = w.rpc("keys.list", json!({})).await;
    assert!(listed["result"].get("pending_on").is_none(), "{listed}");

    w.wires.pull();
    w.wait_link(HostLink::Down).await;
    // A listing with the link down and nothing changed has nothing pending.
    let listed = w.rpc("keys.list", json!({})).await;
    assert!(listed["result"].get("pending_on").is_none(), "{listed}");

    w.mac
        .revoke_key(KeyParams {
            name: "laptop".into(),
        })
        .unwrap();
    let listed = w.rpc("keys.list", json!({})).await;
    assert_eq!(listed["result"]["pending_on"], json!(["vps"]), "{listed}");
    assert!(
        w.host.verify_key(&key.secret).unwrap().is_some(),
        "as the host hasn't heard"
    );

    w.wires.plug_in();
    w.wait_link(HostLink::Connected).await;
    let mut arrived = false;
    for _ in 0..500 {
        if w.host.verify_key(&key.secret).unwrap().is_none() {
            arrived = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(arrived, "the revoke arrived");
    let mut cleared = false;
    for _ in 0..250 {
        let listed = w.rpc("keys.list", json!({})).await;
        if listed["result"].get("pending_on").is_none() {
            cleared = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(cleared, "and it no longer says so");
}

/// The accept deadline is for agent calls, which say they're taken. A clone on a host
/// says nothing till it's done, and takes as long as it takes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_call_thats_not_an_agents_isnt_held_to_the_accept_deadline() {
    let dir = common::short_tempdir();
    let bin = dir.path().join("bin");
    let remotes = dir.path().join("remotes");
    common::repo(&remotes.join("big"));
    let gh = common::fake_gh(&bin, &remotes);
    // Over the deadline, a clone that's slow.
    let slow = bin.join("slow-gh");
    std::fs::write(
        &slow,
        format!(
            "#!/bin/sh\ncase \"$1 $2\" in \"repo clone\") sleep 6 ;; esac\nexec {} \"$@\"\n",
            gh.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
    let w = world_with(move |c| c.gh = slow.into()).await;
    w.add_host().await;
    w.link().await;
    // A first call, so the link has a channel and knows what the host says of it.
    let listed = w.rpc("projects.list", json!({"host": "vps"})).await;
    assert!(listed["result"].is_object(), "{listed}");

    let begun = std::time::Instant::now();
    let added = w
        .rpc("projects.add", json!({"host": "vps", "repo": "o/big"}))
        .await;
    assert_eq!(added["result"]["name"], "big", "{added}");
    assert!(
        begun.elapsed() > Duration::from_secs(5),
        "it's meant to outlast the deadline: {:?}",
        begun.elapsed()
    );
}

/// A host's link stays up by its pings here, so what catches it is the deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_that_doesnt_take_a_call_fails_it_at_the_accept_deadline() {
    let w = world_timed(
        Timing {
            ping_timeout: Duration::from_secs(120),
            ..quick()
        },
        |_| {},
    )
    .await;
    w.add_host().await;
    w.link().await;
    let t = Target {
        host: "vps".into(),
        agent: "nobody".into(),
    };
    // The channel's made, and it's said it takes these.
    let unknown = Daemon::check(&*w.mac, &caller(), &t, false)
        .await
        .unwrap_err();
    assert_eq!(unknown.code, ErrorCode::NotFound.code(), "{unknown:?}");

    w.wires.freeze();
    let begun = std::time::Instant::now();
    let failed = Daemon::send(
        &*w.mac,
        &caller(),
        &t,
        Input::Keys(vec!["y".into()]),
        "frozen-1",
    )
    .await
    .unwrap_err();
    assert!(
        begun.elapsed() < Duration::from_secs(8),
        "{:?}",
        begun.elapsed()
    );
    assert_eq!(failed.code, ErrorCode::HostUnavailable.code(), "{failed:?}");
    assert!(failed.is_outcome_unknown(), "{failed:?}");
    assert!(
        failed.message.contains("didn't take agents.send"),
        "{failed:?}"
    );
    w.wires.plug_in();
}

/// Rule 5 on the Mac's side: an agent's added or deleted on a host, whoever by, and
/// the link's events say so, with no listing asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hosts_events_tell_the_mac_where_a_ref_is() {
    let w = world().await;
    w.add_host().await;
    w.link().await;
    w.host_project().await;
    let known = |name: &str| Daemon::known_hosts(&*w.mac, &caller(), name);
    let started = w
        .host
        .start_agent(AgentStartParams {
            project: "app".into(),
            profile: "software-eng-linux".into(),
            prompt: "talk".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut found = false;
    for _ in 0..500 {
        if known(&started.agent) == ["vps"] {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(found, "the event told it");
    w.host
        .delete_agent(vibe_core::rpc::AgentDeleteParams {
            host: None,
            agent: started.agent.clone(),
            force: Some(true),
        })
        .await
        .unwrap();
    let mut gone = false;
    for _ in 0..500 {
        if known(&started.agent).is_empty() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(gone, "and its delete too");
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
    assert_eq!(
        Daemon::hosts(&*w.mac, &caller()).await.unwrap(),
        ["local", "vps"]
    );
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
    // On the Mac's own version: a newer one is never installed over.
    let w = world_with(|c| c.info.version = "0.1.0".into()).await;
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
    // Nothing's reached it since this Mac started, so for all it knows, a revoke hasn't.
    let keys = rpc(&mac, "keys.list", json!({})).await;
    assert_eq!(keys["result"]["pending_on"], json!(["vps"]), "{keys}");
    wires.plug_in();
    for _ in 0..500 {
        let up = mac.link("vps").is_some_and(|l| l.connected());
        let keys = rpc(&mac, "keys.list", json!({})).await;
        if up && keys["result"].get("pending_on").is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the restarted Mac didn't link to vps");
}

/// #74: what truly needs `sudo` on a host is one button on the Mac: the host only
/// says which need it has, and the Mac writes what that takes.
#[tokio::test]
async fn what_a_host_needs_sudo_for_is_one_line_from_the_mac_and_goes_when_its_done() {
    let checks = Arc::new(vibed::doctor::FixedChecks {
        linger: Mutex::new(Ok(Some(false))),
        enable_linger: Err("loginctl enable-linger was refused: Access denied".into()),
        ..common::golden_checks()
    });
    let held = Arc::clone(&checks);
    let w = world_with(move |c| c.checks = held).await;
    w.add_host().await;
    assert_eq!(w.link().await.needs_admin, [AdminNeed::Linger]);

    let params = || HostNameParams { host: "vps".into() };
    let line = w.mac.host_admin(params()).unwrap().command;
    assert!(
        line.starts_with("ssh -t me@vps.example.com 'echo "),
        "{line}"
    );
    assert!(line.ends_with(" | base64 -d | sh'"), "{line}");

    // The host's doctor asks nothing the host could do, and names the button.
    let report = w.host.doctor().await;
    let row = report
        .checks
        .iter()
        .find(|c| c.id == CheckId::Linger)
        .unwrap();
    assert_eq!(row.state, CheckState::Fail, "{row:?}");
    assert!(row.detail.contains("Access denied"), "{row:?}");
    assert!(row.hint.as_ref().unwrap().contains("Settings → Hosts"));

    // Done, by the button or anyone else: the next ping says so.
    *checks.linger.lock().unwrap() = Ok(Some(true));
    for _ in 0..500 {
        if w.link().await.needs_admin.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(w.link().await.needs_admin, []);
    let refused = w.mac.host_admin(params()).unwrap_err();
    assert_eq!(refused.code, ErrorCode::Conflict.code());
    // This host has no such thing, nor a host that isn't there.
    let local = HostNameParams {
        host: "local".into(),
    };
    assert_eq!(
        w.mac.host_admin(local).unwrap_err().code,
        ErrorCode::InvalidParams.code()
    );
}

/// What VibeOS can do itself isn't on anyone's list at all.
#[tokio::test]
async fn lingering_turns_itself_on_at_the_hosts_next_look() {
    let checks = Arc::new(vibed::doctor::FixedChecks {
        linger: Mutex::new(Ok(Some(false))),
        enable_linger: Ok(()),
        ..common::golden_checks()
    });
    let held = Arc::clone(&checks);
    let w = world_with(move |c| c.checks = held).await;
    w.add_host().await;
    w.host.repair_setup().await;
    assert_eq!(*checks.linger.lock().unwrap(), Ok(Some(true)));
    for _ in 0..500 {
        if w.link().await.needs_admin.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the Mac still thinks it needs sudo");
}

/// #74: a host never has to be kept in step by hand.
mod upgrades {
    use super::*;

    /// The Mac's version, whatever this build's is.
    pub(super) const MAC: &str = "0.1.0";

    async fn behind(timing: Timing) -> World {
        world_timed(timing, |c| c.info.version = "0.0.1".into()).await
    }

    async fn installs_reach(w: &World, want: u32) {
        for _ in 0..500 {
            if *w.wires.installs.lock().unwrap() >= want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "{want} installs never came: {}",
            w.wires.installs.lock().unwrap()
        );
    }

    /// #143. A host still serving External Address isn't upgraded with nobody asked, as that
    /// would end what its clients reach it by. It says why, someone can choose to, and once
    /// it's stopped serving it's upgraded as any other.
    #[tokio::test]
    async fn a_host_still_serving_publicly_waits_for_someone_to_choose() {
        let w = behind(quick()).await;
        let older = w.older_host();
        w.add_host().await;
        let held = wait_held(&w).await;
        let said = held.upgrade_error.expect("it says why");
        assert!(said.contains("External Address"), "{said}");
        assert!(said.contains("vibe host upgrade vps"), "{said}");
        // Past any retry's gap, and many pings: Add Host's install is the only one.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(*w.wires.installs.lock().unwrap(), 1);
        assert!(w.link().await.upgrade_held);

        // Someone chooses: it's done, whatever the host says.
        let chosen = w.rpc("hosts.upgrade", json!({"host": "vps"})).await;
        assert_eq!(chosen["result"]["host"]["name"], "vps", "{chosen}");
        assert_eq!(*w.wires.installs.lock().unwrap(), 2);
        assert!(!w.link().await.upgrade_held);
        drop(older);
    }

    #[tokio::test]
    async fn a_held_host_is_upgraded_by_itself_once_it_stops_serving() {
        let w = behind(quick()).await;
        let older = w.older_host();
        w.add_host().await;
        wait_held(&w).await;
        assert_eq!(*w.wires.installs.lock().unwrap(), 1);
        older.serving.store(false, Ordering::SeqCst);
        installs_reach(&w, 2).await;
        let row = w.link().await;
        assert!(!row.upgrade_held, "{row:?}");
    }

    /// What's on record holds it too, once its link is down and it can't say: a dial that
    /// finds no `vibed` there, which otherwise installs one, doesn't.
    #[tokio::test]
    async fn a_host_on_record_as_serving_is_held_with_no_word_from_it() {
        let w = behind(quick()).await;
        let _older = w.older_host();
        w.add_host().await;
        wait_held(&w).await;
        w.wires.pull();
        w.wait_link(HostLink::Down).await;
        let link = w.mac.link("vps").unwrap();
        assert!(link.reported().is_none());
        w.mac.upgrade_in_background(&link);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(*w.wires.installs.lock().unwrap(), 1);
        assert!(w.link().await.upgrade_held);
    }

    async fn wait_held(w: &World) -> vibe_core::rpc::HostInfo {
        for _ in 0..500 {
            let row = w.link().await;
            if row.upgrade_held {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("it was never held: {:?}", w.link().await);
    }

    #[tokio::test]
    async fn a_host_behind_the_mac_is_upgraded_with_nobody_asked() {
        let w = behind(quick()).await;
        assert_eq!(w.mac.info().version, MAC);
        w.add_host().await;
        // The one in Add Host, then the one for being behind.
        installs_reach(&w, 2).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(w.link().await.upgrade_error, None);
        // Once: it's not behind any more, as far as that install said.
        assert_eq!(*w.wires.installs.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn one_on_this_version_or_newer_is_left_alone() {
        // Newer than the Mac: its registry is one an older one can't read.
        let w = world_timed(quick(), |c| c.info.version = "99.0.0".into()).await;
        w.add_host().await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(*w.wires.installs.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn a_host_that_was_offline_is_upgraded_when_it_next_connects() {
        let w = behind(quick()).await;
        // Installed, and then out of reach.
        w.wires.pull();
        w.mac
            .add_host(HostAddParams {
                name: "vps".into(),
                address: "me@vps.example.com".into(),
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(*w.wires.installs.lock().unwrap(), 1, "it can't be reached");
        w.wires.plug_in();
        installs_reach(&w, 2).await;
    }

    #[tokio::test]
    async fn a_failed_upgrade_says_why_with_retry_and_goes_on_trying() {
        let w = behind(Timing {
            upgrade_retry: Duration::from_millis(400),
            ..quick()
        })
        .await;
        w.add_host().await;
        installs_reach(&w, 2).await;
        *w.wires.install_fails.lock().unwrap() = Some("no space left on device".into());
        // The next one fails, and says what really went wrong.
        let mut seen = None;
        for _ in 0..500 {
            seen = w.link().await.upgrade_error;
            if seen.is_some() {
                break;
            }
            // It's tried again only if it's still behind: as after a link drop.
            w.wires.pull();
            tokio::time::sleep(Duration::from_millis(30)).await;
            w.wires.plug_in();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        let said = seen.expect("the failure shows");
        assert!(said.contains("no space left"), "{said}");
        // By itself again, in time, while the host is reachable.
        let tries = *w.wires.installs.lock().unwrap();
        installs_reach(&w, tries + 1).await;
        // Retry, as the button does: it says the same, as the call's error.
        let retried = w.rpc("hosts.upgrade", json!({"host": "vps"})).await;
        assert!(
            retried["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("no space left")),
            "{retried}"
        );
        // Put right: Retry clears it.
        *w.wires.install_fails.lock().unwrap() = None;
        let retried = w.rpc("hosts.upgrade", json!({"host": "vps"})).await;
        assert_eq!(retried["result"]["host"]["name"], "vps", "{retried}");
        assert_eq!(retried["result"]["host"].get("upgrade_error"), None);
    }

    /// The network going mid-install is a failed try like any other, and a new
    /// connection is no time to sit out an hour.
    #[tokio::test]
    async fn a_try_the_network_cut_short_goes_again_as_soon_as_its_back() {
        let w = behind(quick()).await;
        w.add_host().await;
        installs_reach(&w, 2).await;
        *w.wires.install_fails.lock().unwrap() = Some("the connection dropped".into());
        for tries in 3..=4 {
            w.wires.pull();
            w.wait_link(HostLink::Down).await;
            w.wires.plug_in();
            installs_reach(&w, tries).await;
        }
    }

    /// B1 of the review. Installing over a newer vibed would leave a host that can't
    /// start, however the need for an install came up.
    #[tokio::test]
    async fn a_host_on_a_newer_vibed_is_never_installed_over() {
        let w = world_timed(quick(), |c| c.info.version = "99.0.0".into()).await;
        w.add_host().await;
        let before = *w.wires.installs.lock().unwrap();
        // Its vibed has stopped, which would otherwise be a reason.
        let socket = w
            .wires
            .sockets
            .lock()
            .unwrap()
            .get("me@vps.example.com")
            .cloned()
            .unwrap();
        std::fs::remove_file(&socket).unwrap();
        w.wires.pull();
        tokio::time::sleep(Duration::from_millis(50)).await;
        w.wires.plug_in();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(*w.wires.installs.lock().unwrap(), before);
        // Nor by asking.
        let asked = w.rpc("hosts.upgrade", json!({"host": "vps"})).await;
        assert_eq!(
            asked["error"]["code"],
            ErrorCode::Conflict.code(),
            "{asked}"
        );
        assert_eq!(*w.wires.installs.lock().unwrap(), before);
    }

    /// B3 of the review: an install that goes well and changes nothing is no reason to
    /// go again at the next ping. With the Mac's own bundle behind it, that's said.
    #[tokio::test]
    async fn an_install_that_cant_catch_up_says_so_and_isnt_repeated() {
        let w = world_timed(
            Timing {
                upgrade_retry: Duration::from_secs(60),
                upgrade_gap: Duration::from_secs(60),
                ..quick()
            },
            |c| c.info.version = "0.0.1".into(),
        )
        .await;
        w.wires.older_bundle.store(true, Ordering::SeqCst);
        w.add_host().await;
        for _ in 0..500 {
            if w.link().await.upgrade_error.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let said = w.link().await.upgrade_error.expect("it says so");
        assert!(said.contains("older than this Mac"), "{said}");
        let tries = *w.wires.installs.lock().unwrap();
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(
            *w.wires.installs.lock().unwrap(),
            tries,
            "again at every ping"
        );
    }

    /// A build that wouldn't start is no better for a reconnect, and each try cuts the
    /// host off for half a minute: it waits out the hour, and goes on saying why.
    #[tokio::test]
    async fn a_build_that_went_back_isnt_put_back_at_every_reconnect() {
        let w = behind(Timing {
            upgrade_retry: Duration::from_secs(60),
            ..quick()
        })
        .await;
        w.add_host().await;
        installs_reach(&w, 2).await;
        *w.wires.install_fails.lock().unwrap() = Some(format!(
            "VibeOS didn't start on this build, so {}: it exited",
            vibed::remote::WENT_BACK
        ));
        // It's behind again at a reconnect, and tries, and that build won't start.
        w.wires.pull();
        w.wait_link(HostLink::Down).await;
        w.wires.plug_in();
        installs_reach(&w, 3).await;
        for _ in 0..500 {
            if w.link().await.upgrade_error.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let installs = *w.wires.installs.lock().unwrap();
        for _ in 0..5 {
            w.wires.pull();
            w.wait_link(HostLink::Down).await;
            w.wires.plug_in();
            w.wait_link(HostLink::Connected).await;
            // Well past `quick`'s gap for a reconnect.
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert_eq!(
            *w.wires.installs.lock().unwrap(),
            installs,
            "it put it back"
        );
        let said = w.link().await.upgrade_error.expect("it still says why");
        assert!(said.contains(vibed::remote::WENT_BACK), "{said}");
    }

    /// An install that worked holds nothing off, with no link coming up to say so: if the
    /// host's `vibed` goes missing straight after, it's installed again.
    #[tokio::test]
    async fn one_that_worked_doesnt_hold_off_the_next() {
        let w = world_timed(
            Timing {
                upgrade_retry: Duration::from_secs(60),
                ..quick()
            },
            |c| c.info.version = MAC.into(),
        )
        .await;
        w.wires.macs_bundle.store(true, Ordering::SeqCst);
        w.add_host().await;
        w.wait_link(HostLink::Connected).await;
        w.mac
            .upgrade_host(HostNameParams { host: "vps".into() })
            .await
            .unwrap();
        let installs = *w.wires.installs.lock().unwrap();
        let socket = w
            .wires
            .sockets
            .lock()
            .unwrap()
            .get("me@vps.example.com")
            .cloned()
            .unwrap();
        std::fs::rename(&socket, socket.with_extension("aside")).unwrap();
        w.wires.pull();
        tokio::time::sleep(Duration::from_millis(50)).await;
        w.wires.plug_in();
        installs_reach(&w, installs + 1).await;
    }

    /// A false alarm isn't left in the row of a host that's fine.
    #[tokio::test]
    async fn an_error_goes_once_the_host_is_what_it_should_be() {
        let w = world_timed(quick(), |c| c.info.version = MAC.into()).await;
        w.add_host().await;
        *w.wires.install_fails.lock().unwrap() = Some("no route".into());
        let socket = w
            .wires
            .sockets
            .lock()
            .unwrap()
            .get("me@vps.example.com")
            .cloned()
            .unwrap();
        // Its vibed is away for a moment, and the install that sets off fails.
        let aside = socket.with_extension("aside");
        std::fs::rename(&socket, &aside).unwrap();
        w.wires.pull();
        tokio::time::sleep(Duration::from_millis(50)).await;
        w.wires.plug_in();
        for _ in 0..500 {
            if w.link().await.upgrade_error.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(w.link().await.upgrade_error.is_some());
        // Back by itself: all's well, and nothing's left to say.
        std::fs::rename(&aside, &socket).unwrap();
        for _ in 0..500 {
            let h = w.link().await;
            if h.link == HostLink::Connected && h.upgrade_error.is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{:?}", w.link().await);
    }

    #[test]
    fn only_finding_no_vibed_is_taken_as_a_reason_to_install() {
        use vibed::link::vibed_is_missing as missing;
        assert!(missing(
            "vibe: can't reach vibed at /x/vibed.sock: Connection refused"
        ));
        assert!(missing("sh: 1: .local/lib/vibeos/vibe: not found"));
        assert!(missing(
            "bash: line 1: .local/lib/vibeos/vibe: No such file or directory"
        ));
        assert!(!missing(
            "ssh: connect to host vps port 22: Connection refused"
        ));
        assert!(!missing("me@vps: Permission denied (publickey)."));
        assert!(!missing("it stopped answering (no answer for 10 s)"));
    }

    #[tokio::test]
    async fn a_host_whose_vibed_has_stopped_is_installed_again() {
        let w = world_timed(quick(), |c| c.info.version = MAC.into()).await;
        w.add_host().await;
        let before = *w.wires.installs.lock().unwrap();
        // Its vibed is gone, and the machine's still there: ssh works.
        let socket = w
            .wires
            .sockets
            .lock()
            .unwrap()
            .get("me@vps.example.com")
            .cloned();
        let socket = socket.unwrap();
        std::fs::remove_file(&socket).unwrap();
        w.wires.pull();
        tokio::time::sleep(Duration::from_millis(50)).await;
        w.wires.plug_in();
        installs_reach(&w, before + 1).await;
    }
}

/// A Linux host's config, as a host that isn't linked yet has it.
fn linux_config(world: &Golden) -> vibed::core::Config {
    let mut config = world.config();
    config.info.os = "linux".into();
    config.layout = Layout::new(&world.home, "linux");
    config.toolbox = None;
    config
}

/// #143: what a build from before External Address was retired leaves on a host that
/// had it on. This build serves none of it, says so, and leaves it all where it is, as
/// the owner's to clear and what a build that's gone back would need.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_external_address_left_is_kept_and_serves_nothing() {
    let world = Golden::new();
    let layout = Layout::new(&world.home, "linux");
    drop(Core::open(linux_config(&world)).unwrap());
    assert!(
        Core::open(linux_config(&world))
            .unwrap()
            .retired_external_note()
            .is_none(),
        "a host that never had it hears nothing of it"
    );

    rusqlite::Connection::open(layout.db())
        .unwrap()
        .execute(
            "UPDATE hosts SET external = 'vps.example.com', external_changed_by = 'a Mac' \
             WHERE name = 'local'",
            [],
        )
        .unwrap();
    let certs = layout.data_dir().join("external");
    std::fs::create_dir_all(&certs).unwrap();
    let kept = certs.join("vps.example.com.key");
    std::fs::write(&kept, "a private key").unwrap();
    // sshd's end of an older Mac's `ssh -R` has no lock; an older Mac's own socket has.
    let run = layout.run_dir();
    std::fs::create_dir_all(&run).unwrap();
    let sshd_end = run.join("mac.sock");
    let mac_end = run.join("tunnel.sock");
    drop(std::os::unix::net::UnixListener::bind(&sshd_end).unwrap());
    drop(std::os::unix::net::UnixListener::bind(&mac_end).unwrap());
    std::fs::write(vibe_core::socket::lock_path(&mac_end), "").unwrap();

    let core = Arc::new(Core::open(linux_config(&world)).unwrap());
    let note = core
        .retired_external_note()
        .expect("what to tell its owner");
    assert!(note.contains("vps.example.com"), "{note}");
    assert!(note.contains(certs.to_str().unwrap()), "{note}");
    assert!(note.contains("docs/mcp-and-networking.md"), "{note}");

    // It's no way back in, nor does the host read as serving it.
    let refused = rpc(
        &core,
        "hosts.external",
        json!({"on": true, "domain": "vps.example.com"}),
    )
    .await;
    assert_eq!(
        refused["error"]["code"],
        vibe_core::rpc::ErrorCode::MethodNotFound.code(),
        "{refused}"
    );
    let listed = rpc(&core, "hosts.list", json!({})).await;
    let own = &listed["result"]["hosts"][0];
    assert_eq!(own["external"], false, "{listed}");
    assert!(own.get("url").is_none(), "{listed}");

    // A scan finds no session in either socket, and removes neither.
    let sessions = rpc(&core, "sessions.list", json!({})).await;
    assert_eq!(sessions["result"]["sessions"], json!([]), "{sessions}");
    for left in [&sshd_end, &mac_end, &kept] {
        assert!(left.exists(), "{} went", left.display());
    }
    // And the row's as it was, for a build that goes back.
    let domain: String = rusqlite::Connection::open(layout.db())
        .unwrap()
        .query_row("SELECT external FROM hosts WHERE name = 'local'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(domain, "vps.example.com");
}

/// An agent's row, as one made before 0.1.3 had a name that's since been reserved.
fn legacy_agent(home: &std::path::Path, os: &str, agent_ref: &str) {
    let layout = Layout::new(home, os);
    let mut store = vibe_core::store::Store::open(&layout.db()).unwrap();
    store
        .add_agent(&vibe_core::store::AgentRecord {
            host: vibe_core::model::LOCAL_HOST.into(),
            agent_ref: agent_ref.into(),
            project: "app".into(),
            profile: "software-eng-linux".into(),
            environment: vibe_core::model::Environment::Linux,
            model: vibe_core::model::Model::Opus,
            reasoning: vibe_core::model::Reasoning::High,
            mode: vibe_core::model::Mode::Auto,
            memory: 1 << 30,
            worktree: home
                .join("VibeOS/worktrees/app")
                .join(agent_ref)
                .to_string_lossy()
                .into_owned(),
            branch: agent_ref.into(),
            session_id: "s".into(),
            shim_socket: None,
            state: vibe_core::model::AgentState::Exited,
            created_at: 0,
            compartment: "{}".into(),
            starting: false,
            shared_services: true,
            agent: Default::default(),
            pi_model: None,
        })
        .unwrap();
}

/// An agent named `mac` or `tunnel` before 0.1.3 is still listed and can be deleted. Its
/// name's socket, if an older `vibed` left one, isn't a session's; sshd's has no lock, so
/// deleting the agent leaves it (issue #45).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_named_for_the_retired_tunnel_can_still_be_deleted() {
    let world = Golden::new();
    let layout = Layout::new(&world.home, "linux");
    let first = Core::open(linux_config(&world)).unwrap();
    let path = world.home.join("src/app");
    let added = rpc(&first, "projects.add", json!({"path": path})).await;
    assert!(added.get("result").is_some(), "{added}");
    drop(first);
    for name in ["mac", "tunnel"] {
        legacy_agent(&world.home, "linux", name);
    }
    let run = layout.run_dir();
    std::fs::create_dir_all(&run).unwrap();
    let sshd_end = run.join("mac.sock");
    drop(std::os::unix::net::UnixListener::bind(&sshd_end).unwrap());
    let core = Arc::new(Core::open(linux_config(&world)).unwrap());
    let listed = rpc(&core, "agents.list", json!({})).await;
    for name in ["mac", "tunnel"] {
        let found = listed["result"]["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["ref"] == name);
        assert!(found, "{name}: {listed}");
    }
    let sessions = rpc(&core, "sessions.list", json!({})).await;
    assert_eq!(sessions["result"]["sessions"], json!([]), "{sessions}");
    for name in ["mac", "tunnel"] {
        let deleted = rpc(&core, "agents.delete", json!({"ref": name, "force": true})).await;
        assert!(deleted.get("result").is_some(), "{name}: {deleted}");
    }
    assert!(sshd_end.exists(), "deleting `mac` removed sshd's socket");
}

/// Forgetting a host takes this Mac's keys off it, and another Mac's stay. With its link
/// down it's forgotten all the same: there's no public endpoint they could go on opening.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forgetting_a_host_takes_only_this_macs_keys_off_it() {
    let w = world().await;
    w.add_host().await;
    w.host
        .sync_keys(vibe_core::rpc::KeySyncParams {
            keys: vec![vibe_core::rpc::KeyHash {
                name: "another Mac's".into(),
                prefix: "vk_another".into(),
                hash: "a".repeat(64),
                created_at: "2026-10-30T12:00:00Z".into(),
            }],
            from: Some("another-mac".into()),
        })
        .unwrap();
    let names = || -> Vec<String> {
        let mut names: Vec<String> = w
            .host
            .keys()
            .unwrap()
            .keys
            .into_iter()
            .map(|k| k.name)
            .collect();
        names.sort();
        names
    };
    w.mac
        .create_key(vibe_core::rpc::KeyCreateParams {
            name: "laptop".into(),
        })
        .unwrap();
    for _ in 0..500 {
        if names().contains(&"laptop".to_owned()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(names(), ["another Mac's", "laptop"]);
    let removed = w.rpc("hosts.remove", json!({"host": "vps"})).await;
    assert!(removed.get("result").is_some(), "{removed}");
    assert_eq!(names(), ["another Mac's"]);

    // Added again and gone: it's forgotten with its link down.
    w.add_host().await;
    w.wires.pull();
    w.wait_link(HostLink::Down).await;
    let removed = w.rpc("hosts.remove", json!({"host": "vps"})).await;
    assert!(removed.get("result").is_some(), "{removed}");
}

/// A host on a build from before External Address was retired (#143), as the Mac sees one:
/// the real host, with a stand-in between that says what such a host says of itself, and
/// can refuse to have keys taken off.
struct OlderHost {
    serving: Arc<AtomicBool>,
    refuses_keys: Arc<AtomicBool>,
    _dir: tempfile::TempDir,
}

impl World {
    /// Puts an [`OlderHost`] on the wire in front of the host. Before [`World::add_host`].
    fn older_host(&self) -> OlderHost {
        let dir = common::short_tempdir();
        let path = dir.path().join("older.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let real = self
            .wires
            .sockets
            .lock()
            .unwrap()
            .insert("me@vps.example.com".into(), path)
            .unwrap();
        let serving = Arc::new(AtomicBool::new(true));
        let refuses_keys = Arc::new(AtomicBool::new(false));
        let (says, refuses) = (Arc::clone(&serving), Arc::clone(&refuses_keys));
        tokio::spawn(async move {
            loop {
                let Ok((mac, _)) = listener.accept().await else {
                    return;
                };
                let Ok(host) = tokio::net::UnixStream::connect(&real).await else {
                    continue;
                };
                let (from_mac, mut to_mac) = mac.into_split();
                let (from_host, mut to_host) = host.into_split();
                let (says, refuses) = (Arc::clone(&says), Arc::clone(&refuses));
                tokio::spawn(async move {
                    use tokio::io::AsyncBufReadExt;
                    let mut lines = BufReader::new(from_mac).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        // A method it doesn't know stands for a host that won't.
                        let line = if refuses.load(Ordering::SeqCst) {
                            line.replace("\"keys.sync\"", "\"keys.refused\"")
                        } else {
                            line
                        };
                        if to_host.write_all(line.as_bytes()).await.is_err()
                            || to_host.write_all(b"\n").await.is_err()
                        {
                            return;
                        }
                    }
                });
                let says = Arc::clone(&says);
                tokio::spawn(async move {
                    use tokio::io::AsyncBufReadExt;
                    let mut lines = BufReader::new(from_host).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let mut message: Value = serde_json::from_str(&line).unwrap();
                        if let Some(rows) = message["result"]["hosts"].as_array_mut() {
                            for row in rows {
                                row["external"] = json!(says.load(Ordering::SeqCst));
                            }
                        }
                        let out = format!("{message}\n");
                        if to_mac.write_all(out.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        OlderHost {
            serving,
            refuses_keys,
            _dir: dir,
        }
    }

    /// Pushes a key to the host, and waits for it there.
    async fn pushed_key(&self) {
        self.mac
            .create_key(KeyCreateParams {
                name: "laptop".into(),
            })
            .unwrap();
        for _ in 0..500 {
            if self
                .host
                .keys()
                .unwrap()
                .keys
                .iter()
                .any(|k| k.name == "laptop")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the key never reached the host");
    }

    fn set_mac_row(&self, column: &str) {
        let layout = Layout::new(&self._mac_world.home, "macos");
        rusqlite::Connection::open(layout.db())
            .unwrap()
            .execute(
                &format!("UPDATE hosts SET {column} = 'vps.example.com' WHERE name = 'vps'"),
                [],
            )
            .unwrap();
    }
}

/// #100, for what's left of it after #143. Forgotten with its link down, a host that may
/// still serve publicly would go on taking this Mac's keys, where no revoke could reach
/// them, so that's refused until the Mac can tell it, or someone has said it's gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_public_host_isnt_forgotten_with_this_macs_keys_on_it() {
    let w = world().await;
    let older = w.older_host();
    w.add_host().await;
    assert!(older.serving.load(Ordering::SeqCst));
    w.set_mac_row("external");
    w.pushed_key().await;
    w.wires.pull();
    w.wait_link(HostLink::Down).await;
    let refused = w.rpc("hosts.remove", json!({"host": "vps"})).await;
    assert_eq!(
        refused["error"]["code"],
        ErrorCode::Conflict.code(),
        "{refused}"
    );
    let said = refused["error"]["message"].as_str().unwrap();
    assert!(
        said.contains("API keys") && said.contains("gone for good"),
        "{refused}"
    );
    assert_eq!(w.link().await.name, "vps", "it's still here");
    // Gone for good, then, as far as anyone here can tell.
    let removed = w
        .rpc("hosts.remove", json!({"host": "vps", "force": true}))
        .await;
    assert!(removed.get("result").is_some(), "{removed}");
}

/// #100, the same for a host serving on its own, which this Mac only heard of from the
/// host, though with its link down it can't say so now.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_serving_on_its_own_isnt_forgotten_with_this_macs_keys_on_it() {
    let w = world().await;
    let _older = w.older_host();
    w.add_host().await;
    w.pushed_key().await;
    w.wires.pull();
    w.wait_link(HostLink::Down).await;
    let refused = w.rpc("hosts.remove", json!({"host": "vps"})).await;
    let said = refused["error"]["message"].as_str().unwrap();
    assert!(
        said.contains("API keys") && said.contains("gone for good"),
        "{refused}"
    );
    let removed = w
        .rpc("hosts.remove", json!({"host": "vps", "force": true}))
        .await;
    assert!(removed.get("result").is_some(), "{removed}");
}

/// A host that may serve publicly and won't take this Mac's keys off is refused too, with
/// its link up; and one that says it's stopped serving is forgotten as any other.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keys_a_public_host_wont_give_up_hold_it_until_its_forced_or_its_stopped() {
    let w = world().await;
    let older = w.older_host();
    w.add_host().await;
    w.pushed_key().await;
    older.refuses_keys.store(true, Ordering::SeqCst);
    let refused = w.rpc("hosts.remove", json!({"host": "vps"})).await;
    let said = refused["error"]["message"].as_str().unwrap();
    assert!(said.contains("wouldn't take them off"), "{refused}");

    // It stops serving, and the next ping says so: what was on record of it goes too, so
    // it can be forgotten with its link down.
    older.serving.store(false, Ordering::SeqCst);
    for _ in 0..500 {
        if !w.link_says_external() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!w.link_says_external(), "the host's word never came");
    w.wires.pull();
    w.wait_link(HostLink::Down).await;
    let removed = w.rpc("hosts.remove", json!({"host": "vps"})).await;
    assert!(removed.get("result").is_some(), "{removed}");
}

impl World {
    fn link_says_external(&self) -> bool {
        let link = self.mac.link("vps").unwrap();
        link.reported().is_none_or(|own| own.external)
    }
}

/// The toggle in Settings → Hosts names a host: `tailscale` runs there, its answer comes back,
/// and what it needs `sudo` for reaches the Mac's Allow button (#151).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hosts_tailnet_toggle_goes_to_the_host_and_allow_comes_back() {
    let dir = common::short_tempdir();
    let fake = common::FakeTailscale::new(dir.path());
    let ours = common::FakeTailscale::our_target(vibe_core::limits::MCP_PORT);
    fake.write(
        "after-set.json",
        &common::FakeTailscale::serving(&[("/mcp", &ours)]),
    );
    fake.write(
        "on-set.sh",
        "echo 'sending serve config: Access denied: serve config denied' >&2; exit 1\n",
    );
    let program = fake.program.clone().into_os_string();
    let w = world_with(|c| c.tailscale_cli = Some(program)).await;
    w.add_host().await;

    // Off, as the host says.
    let got = w.rpc("mcp.serve.get", json!({"host": "vps"})).await;
    assert_eq!(
        got["result"],
        json!({"on": false, "available": true}),
        "{got}"
    );

    // Refused for want of the operator: the host's error, and the Mac's button.
    let refused = w
        .rpc("mcp.serve.set", json!({"host": "vps", "on": true}))
        .await;
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("sudo tailscale set --operator="),
        "{refused}"
    );
    for _ in 0..500 {
        if !w.link().await.needs_admin.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(w.link().await.needs_admin, [AdminNeed::TailscaleOperator]);
    let line = w
        .mac
        .host_admin(HostNameParams { host: "vps".into() })
        .unwrap()
        .command;
    assert!(
        line.starts_with("ssh -t me@vps.example.com 'echo "),
        "{line}"
    );

    // HTTPS off on the tailnet: Tailscale's link comes across the link, for Settings' button.
    fake.write(
        "on-set.sh",
        "printf 'Serve is not enabled on your tailnet.\\nTo enable, visit:\\n\\n  https://login.tailscale.com/f/serve?node=abc\\n'; exit 0\n",
    );
    let refused = w
        .rpc("mcp.serve.set", json!({"host": "vps", "on": true}))
        .await;
    assert_eq!(
        refused["error"]["data"]["tailscale_enable_url"],
        json!("https://login.tailscale.com/f/serve?node=abc"),
        "{refused}"
    );

    // Allowed: on, with the URL for the setup command, and the host's flag set, not the Mac's.
    std::fs::remove_file(fake.dir.join("on-set.sh")).unwrap();
    let on = w
        .rpc("mcp.serve.set", json!({"host": "vps", "on": true}))
        .await;
    assert_eq!(
        on["result"],
        json!({"on": true, "available": true, "url": "https://vps.tailnet-example.ts.net/mcp"}),
        "{on}"
    );
    assert!(w.host.mcp_settings().unwrap().1);
    assert!(!w.mac.mcp_settings().unwrap().1);
    // The Mac's own stays as it was, and its toggle's still its own.
    let mac = w.rpc("mcp.get", json!({})).await;
    assert_eq!(mac["result"]["tailscale"], json!(false), "{mac}");

    let off = w
        .rpc("mcp.serve.set", json!({"host": "vps", "on": false}))
        .await;
    assert_eq!(off["result"]["on"], json!(false), "{off}");
    assert!(!w.host.mcp_settings().unwrap().1);
}
