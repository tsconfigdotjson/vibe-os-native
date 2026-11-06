//! A Linux host's MCP reaching the Mac, and the other hosts linked to it, end to end in one
//! process: a Mac and Linux hosts joined by links as `ssh <host> vibe rpc` joins them, and by
//! reverse tunnels as `ssh -R` makes them, a Unix socket on each host forwarded to that
//! host's socket on the Mac. An MCP client talks to the host's loopback MCP with a key: it
//! starts and talks to an agent on each, gets nothing without a key, and is cut off on every
//! host when the key is revoked on the Mac. The Mac checks every call that comes down a
//! tunnel itself; while it's down, the host answers for its own agents and the Mac's are
//! unavailable. Nothing listens beyond loopback, and nothing is public.
//!
//! The same flow with the real `vibed` binaries and the real install scripts is vibe-cli's
//! tests/linux_host.rs.

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{Golden, TestServer, serve};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixListener, UnixStream};
use tokio::sync::watch;
use vibe_core::model::{HostLink, TunnelState};
use vibe_core::paths::Layout;
use vibe_core::rpc::{
    ErrorCode, HostAddParams, HostParams, KeyCreateParams, KeyParams, MAX_MESSAGE_BYTES,
};
use vibed::core::{Core, OpenError};
use vibed::link::Timing;
use vibed::remote::{BoxFuture, Channel, Installed, Remote, TunnelChild};

/// `ssh`, in process: a dial connects to the host's socket, and a tunnel
/// relays the host's end to the Mac's tunnel socket. Pulling the network
/// ends both.
#[derive(Debug)]
struct Wires {
    sockets: Mutex<HashMap<String, PathBuf>>,
    up: watch::Sender<bool>,
    /// Nothing gets through, but nothing ends either, and new connections are
    /// still accepted: a Mac asleep, with sshd none the wiser.
    frozen: watch::Sender<bool>,
    /// Every call that's gone into a tunnel, by its method, oldest first.
    heard: Arc<Mutex<Vec<String>>>,
    /// The same, once the Mac has closed the connection it came on: it's done with
    /// there, whatever it did, as the Mac reads no more of a connection until a call
    /// on it has finished.
    over: Arc<Mutex<Vec<String>>>,
    /// What a Mac says back through a tunnel goes nowhere, while what's sent to
    /// it arrives: a call that's done there, with the host never hearing so.
    replies_lost: Arc<AtomicBool>,
    /// Bumped to end every dial, as stopping the host's `vibed` ends each
    /// `vibe rpc`, while tunnels stay: sshd holds those.
    hang_up: watch::Sender<u64>,
    tunnels: Mutex<u32>,
    /// Whether tunnels may be made (sshd might refuse to forward).
    forwarding: AtomicBool,
}

impl Wires {
    fn new() -> Arc<Wires> {
        Arc::new(Wires {
            sockets: Mutex::new(HashMap::new()),
            up: watch::channel(true).0,
            frozen: watch::channel(false).0,
            heard: Arc::default(),
            over: Arc::default(),
            replies_lost: Arc::new(AtomicBool::new(false)),
            hang_up: watch::channel(0).0,
            tunnels: Mutex::new(0),
            forwarding: AtomicBool::new(true),
        })
    }

    fn pull(&self) {
        self.up.send_replace(false);
    }

    fn plug_in(&self) {
        self.up.send_replace(true);
    }

    fn freeze(&self, frozen: bool) {
        self.frozen.send_replace(frozen);
    }

    fn hang_up_dials(&self) {
        self.hang_up.send_modify(|n| *n += 1);
    }

    /// Waits until the Mac is done with every call of `method` that's come through, and
    /// with at least `count`, those known to have been sent but maybe not there yet.
    async fn calls_over(&self, method: &str, count: usize) {
        let tally = |calls: &Mutex<Vec<String>>| {
            let calls = calls.lock().unwrap();
            calls.iter().filter(|m| *m == method).count()
        };
        for _ in 0..3000 {
            if tally(&self.over) >= count.max(tally(&self.heard)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "the Mac finished with {} of {count} calls of {method}; it heard {:?}",
            tally(&self.over),
            self.heard.lock().unwrap()
        );
    }
}

/// Copies bytes both ways until `b` ends or the network goes. Once `a` ends, `b` is told
/// and has its say, with nobody left to hear it.
async fn pump(
    a: UnixStream,
    b: UnixStream,
    mut up: watch::Receiver<bool>,
    mut frozen: watch::Receiver<bool>,
    b_lost: Arc<AtomicBool>,
    heard: Option<Arc<Mutex<Vec<String>>>>,
    over: Option<Arc<Mutex<Vec<String>>>>,
) {
    let (mut ar, mut aw) = a.into_split();
    let (mut br, mut bw) = b.into_split();
    let mut x = vec![0u8; 1 << 16];
    let mut y = vec![0u8; 1 << 16];
    // Its end closed: what's on the way still gets there, as with ssh, which closes the
    // other end only once that's done with too.
    let mut a_done = false;
    // What's come through this connection, for `over` when it ends.
    let mut carried = Vec::new();
    loop {
        if !*up.borrow_and_update() {
            return;
        }
        if *frozen.borrow_and_update() {
            tokio::select! {
                _ = up.changed() => {}
                _ = frozen.changed() => {}
            }
            continue;
        }
        tokio::select! {
            _ = up.changed() => {}
            _ = frozen.changed() => {}
            n = ar.read(&mut x), if !a_done => match n {
                Ok(0) | Err(_) => {
                    a_done = true;
                    let _ = bw.shutdown().await;
                }
                Ok(n) => {
                    if let Some(heard) = &heard {
                        for line in String::from_utf8_lossy(&x[..n]).lines() {
                            let call: Value = serde_json::from_str(line).unwrap_or_default();
                            if let Some(method) = call["params"]["method"].as_str() {
                                heard.lock().unwrap().push(method.to_owned());
                                carried.push(method.to_owned());
                            }
                        }
                    }
                    if bw.write_all(&x[..n]).await.is_err() { return }
                }
            },
            n = br.read(&mut y) => match n {
                Ok(0) | Err(_) => {
                    if let Some(over) = &over {
                        over.lock().unwrap().extend(carried);
                    }
                    return;
                }
                Ok(_) if b_lost.load(Ordering::SeqCst) => {}
                // With nobody left to tell, reading `a` finds its end, and that ends `b`.
                Ok(n) => { let _ = aw.write_all(&y[..n]).await; }
            },
        }
    }
}

impl Remote for Wires {
    fn dial<'a>(&'a self, address: &'a str) -> BoxFuture<'a, Result<Channel, String>> {
        Box::pin(async move {
            if !*self.up.borrow() {
                return Err(format!(
                    "ssh: connect to host {address} port 22: Network is unreachable"
                ));
            }
            let socket = self.sockets.lock().unwrap().get(address).cloned();
            let socket =
                socket.ok_or_else(|| format!("ssh: Could not resolve hostname {address}"))?;
            let host = UnixStream::connect(&socket)
                .await
                .map_err(|e| format!("vibe: can't reach vibed: {e}"))?;
            let (ours, theirs) = UnixStream::pair().map_err(|e| e.to_string())?;
            let up = self.up.subscribe();
            let frozen = self.frozen.subscribe();
            let mut hang_up = self.hang_up.subscribe();
            tokio::spawn(async move {
                tokio::select! {
                    () = pump(host, theirs, up, frozen, Arc::new(AtomicBool::new(false)), None, None) => {}
                    _ = hang_up.changed() => {}
                }
            });
            let (r, w) = ours.into_split();
            Ok(Channel::new(
                BufReader::new(r),
                w,
                (),
                Arc::new(Mutex::new(String::new())),
            ))
        })
    }

    fn install<'a>(&'a self, _address: &'a str) -> BoxFuture<'a, Result<Installed, String>> {
        Box::pin(async move {
            Ok(Installed {
                os: "linux".into(),
                arch: "x86_64".into(),
                label: "vps-1".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                missing: vec![],
                needs_admin: Vec::new(),
                changed: true,
                notes: vec![],
            })
        })
    }

    fn tunnel<'a>(
        &'a self,
        address: &'a str,
        remote: &'a str,
        local: &'a Path,
    ) -> BoxFuture<'a, Result<TunnelChild, String>> {
        Box::pin(async move {
            if !*self.up.borrow() {
                return Err(format!("can't reach {address} over SSH: unreachable"));
            }
            if !self.forwarding.load(Ordering::SeqCst) {
                return Err(format!("{address}'s sshd refused to forward a Unix socket"));
            }
            // As sshd does: it binds the host's end, and won't bind over
            // one that's there.
            let listener = UnixListener::bind(remote).map_err(|e| {
                format!("remote port forwarding failed for listen path {remote}: {e}")
            })?;
            *self.tunnels.lock().unwrap() += 1;
            let local = local.to_owned();
            let mut up = self.up.subscribe();
            let pumps = self.up.clone();
            let frozen = self.frozen.clone();
            let replies_lost = Arc::clone(&self.replies_lost);
            let heard = Arc::clone(&self.heard);
            let over = Arc::clone(&self.over);
            Ok(TunnelChild {
                ended: Box::pin(async move {
                    loop {
                        tokio::select! {
                            accepted = listener.accept() => {
                                let Ok((there, _)) = accepted else { continue };
                                let Ok(here) = UnixStream::connect(&local).await else { continue };
                                tokio::spawn(pump(
                                    there,
                                    here,
                                    pumps.subscribe(),
                                    frozen.subscribe(),
                                    Arc::clone(&replies_lost),
                                    Some(Arc::clone(&heard)),
                                    Some(Arc::clone(&over)),
                                ));
                            }
                            _ = up.changed() => {
                                if !*up.borrow_and_update() {
                                    return "Connection to vps closed by remote host.".to_owned();
                                }
                            }
                        }
                    }
                }),
            })
        })
    }
}

fn quick() -> Timing {
    Timing {
        ping_every: Duration::from_millis(200),
        ping_timeout: Duration::from_secs(3),
        connect_timeout: Duration::from_secs(5),
        min_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(300),
        ..Timing::default()
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct World {
    mac: Arc<Core>,
    host: Arc<Core>,
    wires: Arc<Wires>,
    mac_world: Golden,
    host_world: Golden,
    /// Seconds the Mac's clock is ahead of the host's, as time passing in the
    /// tunnel looks to it.
    mac_clock_ahead: Arc<AtomicI64>,
    /// The host's loopback MCP.
    public: SocketAddr,
    mcp: tokio::task::JoinHandle<()>,
    _server: TestServer,
}

/// A Mac and, at `me@vps.example.com`, a Linux host with the project `app` on each, with
/// the host's MCP on a loopback port of its own.
async fn world() -> World {
    world_timed(quick()).await
}

/// [`world`], with the Mac's link and tunnel timed by `timing`.
async fn world_timed(timing: Timing) -> World {
    let wires = Wires::new();
    let mac_world = Golden::new();
    let mut config = mac_world.config();
    config.remote = Arc::clone(&wires) as Arc<dyn Remote>;
    config.link_timing = timing;
    let mac_clock_ahead = Arc::new(AtomicI64::new(0));
    let ahead = Arc::clone(&mac_clock_ahead);
    config.clock = Arc::new(move || common::GOLDEN_NOW + ahead.load(Ordering::SeqCst));
    let mac = Arc::new(Core::open(config).unwrap());
    mac.spawn_links();
    mac.spawn_tunnels();

    let port = free_port();
    let host_world = Golden::new();
    let (host, server, mcp) = open_host(&host_world, port, &wires).await;
    let w = World {
        mac,
        host,
        wires,
        mac_world,
        host_world,
        mac_clock_ahead,
        public: SocketAddr::from(([127, 0, 0, 1], port)),
        mcp,
        _server: server,
    };
    w.mac
        .add_host(HostAddParams {
            name: "vps".into(),
            address: "me@vps.example.com".into(),
        })
        .await
        .unwrap();
    w.wait("vps's link", |h| h.link == HostLink::Connected)
        .await;
    // `app` on each.
    let ok = |v: Value| assert!(v.get("result").is_some(), "{v}");
    ok(rpc(
        &w.mac,
        "projects.add",
        json!({"path": w.mac_world.home.join("src/app")}),
    )
    .await);
    ok(rpc(
        &w.mac,
        "projects.add",
        json!({"host": "vps", "path": w.host_world.home.join("src/app")}),
    )
    .await);
    w
}

/// The Linux host's `vibed` on `host_world`, started as `vibed` starts,
/// once an earlier one there has let go of its state, and served at
/// `me@vps.example.com`.
async fn open_host(
    host_world: &Golden,
    port: u16,
    wires: &Wires,
) -> (Arc<Core>, TestServer, tokio::task::JoinHandle<()>) {
    open_host_at(host_world, port, wires, "me@vps.example.com").await
}

/// [`open_host`], served at `address`.
async fn open_host_at(
    host_world: &Golden,
    port: u16,
    wires: &Wires,
    address: &str,
) -> (Arc<Core>, TestServer, tokio::task::JoinHandle<()>) {
    let host_toml = Layout::new(&host_world.home, "linux").host_config();
    std::fs::create_dir_all(host_toml.parent().unwrap()).unwrap();
    std::fs::write(&host_toml, format!("mcp_port = {port}\n")).unwrap();
    let config = || {
        let mut config = host_world.config();
        config.info.os = "linux".into();
        config.label = "vps-1".into();
        config.layout = Layout::new(&host_world.home, "linux");
        config.toolbox = None;
        // Its own memory, unlike the golden Mac's, and nothing short.
        config.memory = Arc::new(|| {
            Some(vibe_core::rpc::MemoryUse {
                total: 16 << 30,
                used: 4 << 30,
                swap_total: 0,
                swap_used: 0,
                pressure: None,
            })
        });
        config.pressure = Arc::new(vibed::pressure::NoPressure);
        // Its own keys aren't the Mac's.
        config.entropy = Arc::new(vibe_core::keys::random_bytes);
        config.serves_mac_peer = true;
        config.mac_probe_every = Duration::from_millis(100);
        config
    };
    let mut host = None;
    for _ in 0..1000 {
        match Core::open(config()) {
            Ok(core) => {
                host = Some(Arc::new(core));
                break;
            }
            Err(OpenError::AlreadyRunning { .. }) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("{e}"),
        }
    }
    let host = host.expect("the last host's vibed let go of its state");
    host.adopt_sessions().await;
    host.spawn_links();
    host.spawn_tunnels();
    let mcp = host.spawn_mcp(Duration::from_millis(50));
    let server = serve(
        Arc::clone(&host),
        common::short_tempdir(),
        MAX_MESSAGE_BYTES,
    );
    wires
        .sockets
        .lock()
        .unwrap()
        .insert(address.into(), server.path.clone());
    (host, server, mcp)
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

impl World {
    fn vps(&self) -> vibe_core::rpc::HostInfo {
        self.mac
            .hosts()
            .unwrap()
            .hosts
            .into_iter()
            .find(|h| h.name == "vps")
            .unwrap()
    }

    /// Waits until the Mac's view of vps is `ok`.
    async fn wait(&self, what: &str, ok: impl Fn(&vibe_core::rpc::HostInfo) -> bool) {
        for _ in 0..1000 {
            if ok(&self.vps()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{what} never came: {:?}", self.vps());
    }

    fn client(&self, key: &str) -> Client {
        Client {
            addr: self.public,
            key: Some(key.to_owned()),
            next: 0,
        }
    }

    /// Waits for the tunnel the Mac opens to every host linked to it. There's nothing to turn
    /// on, and the host's MCP has `mac` to speak of from then on.
    async fn turn_on(&self) {
        self.wait("the tunnel", |h| h.tunnel == Some(TunnelState::Up))
            .await;
    }

    /// Stops the host's `vibed` and starts it again, as an upgrade does:
    /// its links end, and on start it scans `run/` for sessions. sshd, so
    /// the Mac's tunnel and its end in `run/`, carries on.
    async fn restart_host(self) -> World {
        let World {
            mac,
            host,
            wires,
            mac_world,
            host_world,
            mac_clock_ahead,
            public,
            mcp,
            _server,
        } = self;
        host.stop_tunnels();
        mcp.abort();
        drop(_server);
        drop(host);
        wires.hang_up_dials();
        let (host, server, mcp) = open_host(&host_world, public.port(), &wires).await;
        World {
            mac,
            host,
            wires,
            mac_world,
            host_world,
            mac_clock_ahead,
            public,
            mcp,
            _server: server,
        }
    }

    /// The end of the Mac's tunnel on the host.
    fn tunnel_end(&self) -> PathBuf {
        Layout::new(&self.host_world.home, "linux")
            .run_dir()
            .join(vibed::mac_peer::TUNNEL_END)
    }

    /// Waits until an outside client with `key` reaches the Mac's
    /// projects through the host.
    async fn mac_reachable(&self, key: &str) {
        for _ in 0..500 {
            if TcpStream::connect(self.public).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut c = self.client(key);
        let mut projects = String::new();
        for _ in 0..300 {
            projects = c.call("vibe_list_projects", json!({})).await;
            if projects.contains("app (on mac)") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("the Mac never came back through the tunnel: {projects}");
    }

    fn key(&self, name: &str) -> String {
        self.mac
            .create_key(KeyCreateParams { name: name.into() })
            .unwrap()
            .secret
    }

    /// Thaws the tunnel, and waits until the host has found the Mac again.
    async fn thaw(&self) {
        self.wires.freeze(false);
        for _ in 0..1000 {
            if self.host.hosts().unwrap().hosts[0].tunnel == Some(TunnelState::Up) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the tunnel never came back");
    }

    /// Waits until a look at the tunnel has failed.
    async fn tunnel_down(&self) {
        for _ in 0..1500 {
            if self.host.hosts().unwrap().hosts[0].tunnel != Some(TunnelState::Up) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the tunnel never went down");
    }

    /// Whether `what` comes up in a listing within a few seconds.
    async fn listing_shows(&self, c: &mut Client, what: &str) -> bool {
        for _ in 0..100 {
            if c.call("vibe_list_agents", json!({})).await.contains(what) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// A listing the Mac has answered for. One that came up empty-handed says it's
    /// unavailable, however briefly, and that's not what's being looked at.
    async fn listing_the_mac_answered(&self, c: &mut Client) -> String {
        let mut listed = String::new();
        for _ in 0..300 {
            listed = c.call("vibe_list_agents", json!({})).await;
            if !listed.contains("unavailable") {
                return listed;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("the Mac never answered a listing: {listed}");
    }

    /// Whether the Mac has `agent`, as it says itself, with no tunnel in between.
    async fn mac_has(&self, agent: &str) -> bool {
        let list = self.mac.agents(HostParams { host: None }).await.unwrap();
        list.agents.iter().any(|a| a.agent == agent)
    }

    /// Waits until the host takes `key`, once the Mac has pushed it.
    async fn pushed(&self, key: &str) {
        for _ in 0..500 {
            if self.host.verify_key(key).unwrap().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the key never reached the host");
    }
}

/// What an HTTP answer was.
#[derive(Debug)]
struct Reply {
    status: u16,
    headers: String,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }
}

/// An MCP client of the host's: plain HTTP to its loopback MCP, as over Tailscale.
struct Client {
    addr: SocketAddr,
    key: Option<String>,
    next: u64,
}

impl Client {
    async fn post(&self, path: &str, body: &str) -> Reply {
        let mut s = TcpStream::connect(self.addr).await.unwrap();
        let mut head = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\nMCP-Protocol-Version: 2025-06-18\r\n\
             Content-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        if let Some(key) = &self.key {
            head.push_str(&format!("Authorization: Bearer {key}\r\n"));
        }
        head.push_str("\r\n");
        s.write_all(head.as_bytes()).await.unwrap();
        s.write_all(body.as_bytes()).await.unwrap();
        let mut raw = Vec::new();
        let _ = s.read_to_end(&mut raw).await;
        let end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let headers = String::from_utf8_lossy(&raw[..end]).into_owned();
        let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
        let mut body = raw[end + 4..].to_vec();
        if headers
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            let mut out = Vec::new();
            let mut rest = &body[..];
            loop {
                let i = rest.windows(2).position(|w| w == b"\r\n").unwrap();
                let n = usize::from_str_radix(std::str::from_utf8(&rest[..i]).unwrap().trim(), 16)
                    .unwrap();
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&rest[i + 2..i + 2 + n]);
                rest = &rest[i + 4 + n..];
            }
            body = out;
        }
        Reply {
            status,
            headers,
            body: String::from_utf8(body).unwrap(),
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> Reply {
        self.next += 1;
        let body = json!({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params});
        self.post("/mcp", &body.to_string()).await
    }

    /// A tool's result: whether it's an error, and its text.
    async fn tool(&mut self, name: &str, arguments: Value) -> (bool, String) {
        let r = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await;
        assert_eq!(r.status, 200, "{name}: {r:?}");
        let v = r.json();
        let result = &v["result"];
        (
            result["isError"] == true,
            result["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        )
    }

    async fn call(&mut self, name: &str, arguments: Value) -> String {
        let (error, text) = self.tool(name, arguments.clone()).await;
        assert!(!error, "{name} {arguments}: {text}");
        text
    }

    async fn fails(&mut self, name: &str, arguments: Value) -> String {
        let (error, text) = self.tool(name, arguments.clone()).await;
        assert!(error, "{name} {arguments} should fail: {text}");
        text
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_url_and_one_key_reach_agents_on_the_host_and_the_mac() {
    let w = world().await;
    w.turn_on().await;
    let vps = w.vps();
    assert_eq!(vps.tunnel_detail, None, "{vps:?}");
    assert!(!vps.external, "{vps:?}");
    // The host says so too, of itself.
    let own = w.host.hosts().unwrap().hosts.remove(0);
    assert!(!own.external, "{own:?}");
    assert_eq!(own.tunnel, Some(TunnelState::Up));

    // Nothing without a key, or with one that isn't.
    let mut nobody = w.client("");
    nobody.key = None;
    let r = nobody.request("tools/list", json!({})).await;
    assert_eq!(r.status, 401, "{r:?}");
    assert!(r.headers.contains("WWW-Authenticate") || r.headers.contains("www-authenticate"));
    let mut wrong = w.client(&vibe_core::keys::encode(&[9; 32]));
    assert_eq!(wrong.request("tools/list", json!({})).await.status, 401);
    // Only /mcp.
    assert_eq!(nobody.post("/", "{}").await.status, 404);

    // A key made on the Mac works there once it's pushed.
    let key = w.key("laptop");
    w.pushed(&key).await;
    let mut c = w.client(&key);
    let r = c
        .request(
            "initialize",
            json!({"protocolVersion": "2025-06-18",
        "capabilities": {}, "clientInfo": {"name": "outside", "version": "1"}}),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");

    // One URL for both: projects on each, the Mac as `mac`.
    let projects = c.call("vibe_list_projects", json!({})).await;
    assert!(projects.contains("app (on local)"), "{projects}");
    assert!(projects.contains("app (on mac)"), "{projects}");

    // An agent on each, started and talked to through the one URL.
    let started = c
        .call(
            "vibe_start_agent",
            json!({"project": "app", "profile": "software-eng-linux", "prompt": "talk",
                   "host": "local", "name": "on-vps"}),
        )
        .await;
    assert!(started.contains("started on-vps"), "{started}");
    let started = c
        .call(
            "vibe_start_agent",
            json!({"project": "app", "profile": "software-eng-mac", "prompt": "talk",
                   "host": "mac", "name": "on-mac"}),
        )
        .await;
    assert!(started.contains("started mac:on-mac"), "{started}");
    // It's the Mac's own agent, in the Mac's registry.
    assert!(
        w.mac
            .agents(Default::default())
            .await
            .unwrap()
            .agents
            .iter()
            .any(|a| a.agent == "on-mac")
    );
    let listed = c.call("vibe_list_agents", json!({})).await;
    assert!(
        listed.contains("on-vps") && listed.contains("mac:on-mac"),
        "{listed}"
    );
    assert!(!listed.contains("unavailable"), "{listed}");
    // Each host's memory: the host's own, and the Mac's, down the tunnel.
    assert!(
        listed.contains("\nlocal: 4G of 16G memory in use\n"),
        "{listed}"
    );
    assert!(
        listed.contains(
            "\nmac: 58G of 64G memory in use, 3G of 4G swap. Short of memory: macOS warns of \
             memory pressure"
        ),
        "{listed}"
    );
    for agent in ["on-vps", "mac:on-mac"] {
        let waited = c
            .call("vibe_wait", json!({"agents": agent, "timeout": 60}))
            .await;
        assert!(waited.contains("has answered"), "{agent}: {waited}");
        let sent = c
            .call(
                "vibe_send",
                json!({"agent": agent, "text": format!("hello {agent}")}),
            )
            .await;
        assert!(sent.contains("submitted"), "{sent}");
        let waited = c
            .call("vibe_wait", json!({"agents": agent, "timeout": 60}))
            .await;
        assert!(waited.contains("has answered"), "{agent}: {waited}");
        let read = c
            .call("vibe_read_messages", json!({"agent": agent, "last": 1}))
            .await;
        assert!(
            read.contains(&format!("> hello {agent}")),
            "{agent}: {read}"
        );
        let screen = c.call("vibe_read_output", json!({"agent": agent})).await;
        assert!(screen.contains(&format!("hello {agent}")), "{screen}");
    }
    // Mac offline: the host answers for its own agents, and the Mac's show
    // as unavailable.
    w.wires.pull();
    w.wait("the tunnel down", |h| h.tunnel == Some(TunnelState::Down))
        .await;
    for _ in 0..200 {
        if !w.host.hosts().unwrap().hosts[0]
            .tunnel
            .is_some_and(|t| t == TunnelState::Up)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let listed = c.call("vibe_list_agents", json!({})).await;
    assert!(listed.contains("on-vps"), "{listed}");
    let mac_line = listed
        .lines()
        .find(|l| l.starts_with("mac:on-mac"))
        .unwrap();
    assert!(mac_line.contains("unavailable"), "{listed}");
    // The Mac's memory isn't known while it's away.
    assert!(!listed.contains("\nmac: "), "{listed}");
    let failed = c
        .fails(
            "vibe_send",
            json!({"agent": "mac:on-mac", "text": "anyone?"}),
        )
        .await;
    assert!(failed.contains("mac is unavailable"), "{failed}");
    let read = c
        .call("vibe_read_messages", json!({"agent": "on-vps", "last": 1}))
        .await;
    assert!(read.contains("> hello on-vps"), "{read}");
    // Back: it reconnects on its own.
    w.wires.plug_in();
    w.wait("the tunnel back", |h| h.tunnel == Some(TunnelState::Up))
        .await;
    let listed = c.call("vibe_list_agents", json!({})).await;
    assert!(!listed.contains("unavailable"), "{listed}");
    let deleted = c
        .call(
            "vibe_delete_agent",
            json!({"agent": "mac:on-mac", "force": true}),
        )
        .await;
    assert!(deleted.contains("deleted mac:on-mac"), "{deleted}");
    let deleted = c
        .call(
            "vibe_delete_agent",
            json!({"agent": "on-vps", "force": true}),
        )
        .await;
    assert!(deleted.contains("deleted on-vps"), "{deleted}");

    // Revoked on the Mac: cut off on every host.
    w.mac
        .revoke_key(KeyParams {
            name: "laptop".into(),
        })
        .unwrap();
    for _ in 0..500 {
        if w.host.verify_key(&key).unwrap().is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let r = c.request("tools/list", json!({})).await;
    assert_eq!(r.status, 401, "revoked: {r:?}");

    // Forgotten: the tunnel closes, and its end goes, so the host has no `mac` to list. It
    // serves on, on its own, as nothing about it was this Mac's.
    let removed = rpc(&w.mac, "hosts.remove", json!({"host": "vps"})).await;
    assert!(removed.get("result").is_some(), "{removed}");
    assert!(!w.tunnel_end().exists(), "its end is left on the host");
    assert_eq!(w.host.hosts().unwrap().hosts[0].tunnel, None);
    assert!(
        TcpStream::connect(w.public).await.is_ok(),
        "it stopped serving"
    );
    // Nothing of the tunnel is left on the Mac: no socket, no lock.
    let socket = w.mac.tunnel_socket("vps");
    assert!(!socket.exists(), "{} is left", socket.display());
    assert!(!vibe_core::socket::lock_path(&socket).exists());
}

/// The send a caller can't know the fate of: the Mac types it, its answer never
/// reaches the host, and the caller is told to try again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_delivered_with_its_reply_lost_arrives_once_when_retried() {
    let w = world().await;
    w.turn_on().await;
    let key = w.key("laptop");
    w.pushed(&key).await;
    let mut c = w.client(&key);
    let agent = "mac:on-mac";
    c.call(
        "vibe_start_agent",
        json!({"project": "app", "profile": "software-eng-mac", "prompt": "talk",
               "host": "mac", "name": "on-mac"}),
    )
    .await;
    c.call("vibe_wait", json!({"agents": agent, "timeout": 60}))
        .await;

    w.wires.replies_lost.store(true, Ordering::SeqCst);
    let failed = c
        .fails("vibe_send", json!({"agent": agent, "text": "only once"}))
        .await;
    assert!(failed.contains("mac is unavailable"), "{failed}");
    let id = failed
        .split("request_id \"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_else(|| panic!("no id to retry with: {failed}"))
        .to_owned();

    w.wires.replies_lost.store(false, Ordering::SeqCst);
    for _ in 0..1000 {
        if w.host.hosts().unwrap().hosts[0].tunnel == Some(TunnelState::Up) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // What came of it, as the caller can read it.
    let times = async |c: &mut Client, text: &str| {
        c.call("vibe_wait", json!({"agents": agent, "timeout": 60}))
            .await;
        c.call("vibe_read_messages", json!({"agent": agent, "last": 50}))
            .await
            .matches(&format!("> {text}"))
            .count()
    };
    assert_eq!(times(&mut c, "only once").await, 1, "it did arrive");

    // The retry is told it went, and nothing more is typed.
    let again = c
        .call(
            "vibe_send",
            json!({"agent": agent, "text": "only once", "request_id": id}),
        )
        .await;
    assert!(again.contains("sent to mac:on-mac"), "{again}");
    assert_eq!(times(&mut c, "only once").await, 1, "delivered once");

    // The id is for that send and no other.
    let other = c
        .fails(
            "vibe_send",
            json!({"agent": agent, "text": "something else", "request_id": id}),
        )
        .await;
    assert!(other.contains("different send"), "{other}");

    // Saying the same thing again on purpose is still two sends.
    c.call("vibe_send", json!({"agent": agent, "text": "only once"}))
        .await;
    assert_eq!(times(&mut c, "only once").await, 2, "a new send");
}

/// What the tunnel's socket answers a call with, having first said it has it.
async fn answer_from(r: &mut BufReader<UnixStream>) -> Value {
    loop {
        let mut out = String::new();
        tokio::io::AsyncBufReadExt::read_line(r, &mut out)
            .await
            .unwrap();
        let line: Value = serde_json::from_str(&out).unwrap();
        if line.get("id").is_some() {
            return line;
        }
        assert_eq!(line["method"], vibe_core::rpc::CALL_ACCEPTED, "{line}");
    }
}

/// The Mac sleeps with a send on its way: the caller is told nobody knows, and when
/// the Mac wakes it lands. A retry with the id it was given types nothing more.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_held_up_by_a_sleeping_mac_lands_once_when_retried() {
    let w = world().await;
    w.turn_on().await;
    let key = w.key("laptop");
    w.pushed(&key).await;
    let mut c = w.client(&key);
    let agent = "mac:on-mac";
    c.call(
        "vibe_start_agent",
        json!({"project": "app", "profile": "software-eng-mac", "prompt": "talk",
               "host": "mac", "name": "on-mac"}),
    )
    .await;
    c.call("vibe_wait", json!({"agents": agent, "timeout": 60}))
        .await;

    // Before any look has failed: the send goes out, into the frozen tunnel.
    w.wires.freeze(true);
    let failed = c
        .fails(
            "vibe_send",
            json!({"agent": agent, "text": "held up", "request_id": "mine"}),
        )
        .await;
    assert!(failed.contains("Outcome unknown"), "{failed}");
    assert!(failed.contains("check vibe_read_messages"), "{failed}");
    assert!(failed.contains("request_id \"mine\""), "{failed}");

    // Once a look has failed, a new one is known not to have gone.
    w.tunnel_down().await;
    let refused = c
        .fails("vibe_send", json!({"agent": agent, "text": "not this"}))
        .await;
    assert!(refused.contains("nothing was sent"), "{refused}");
    assert!(!refused.contains("request_id"), "{refused}");

    w.thaw().await;
    let times = async |c: &mut Client, text: &str| {
        c.call("vibe_wait", json!({"agents": agent, "timeout": 60}))
            .await;
        c.call("vibe_read_messages", json!({"agent": agent, "last": 50}))
            .await
            .matches(&format!("> {text}"))
            .count()
    };
    let again = c
        .call(
            "vibe_send",
            json!({"agent": agent, "text": "held up", "request_id": "mine"}),
        )
        .await;
    assert!(again.contains("sent to mac:on-mac"), "{again}");
    // Whichever got there first, the one held up or its retry, that's the one.
    for _ in 0..3 {
        assert_eq!(times(&mut c, "held up").await, 1, "delivered once");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert_eq!(times(&mut c, "not this").await, 0);
}

/// Rule 7 and rule 5: a listing is one call to the Mac, with its memory; a ref this
/// host has makes none at all; and one on the Mac that this key started needs no lookup.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_mac_is_only_asked_what_only_it_can_say() {
    let w = world().await;
    w.turn_on().await;
    let key = w.key("laptop");
    w.pushed(&key).await;
    let mut c = w.client(&key);
    c.call(
        "vibe_start_agent",
        json!({"project": "app", "profile": "software-eng-linux", "prompt": "talk",
               "host": "local", "name": "here"}),
    )
    .await;
    c.call("vibe_wait", json!({"agents": "here", "timeout": 60}))
        .await;
    c.call(
        "vibe_start_agent",
        json!({"project": "app", "profile": "software-eng-mac", "prompt": "talk",
               "host": "mac", "name": "there"}),
    )
    .await;

    let heard = || std::mem::take(&mut *w.wires.heard.lock().unwrap());
    heard();

    // A ref on the Mac that this key started goes straight there, no listing first.
    c.call("vibe_wait", json!({"agents": "there", "timeout": 60}))
        .await;
    let calls = heard();
    assert!(!calls.contains(&"agents.list".to_owned()), "{calls:?}");
    assert!(calls.contains(&"agents.check".to_owned()), "{calls:?}");

    // A ref this host has makes no call to the Mac at all.
    c.call("vibe_read_output", json!({"agent": "here"})).await;
    c.call("vibe_read_messages", json!({"agent": "here", "last": 1}))
        .await;
    c.call("vibe_send", json!({"agent": "here", "text": "hello"}))
        .await;
    c.call("vibe_wait", json!({"agents": "here", "timeout": 60}))
        .await;
    assert_eq!(heard(), Vec::<String>::new());

    // A listing is one call, and the Mac's memory comes with it.
    let listed = c.call("vibe_list_agents", json!({})).await;
    assert!(listed.contains("mac:there"), "{listed}");
    assert!(listed.contains("mac: "), "its memory: {listed}");
    assert!(listed.contains("memory in use"), "{listed}");
    let calls = heard();
    assert_eq!(calls, ["agents.list"], "and no hosts.list");

    // Deleted from here, it's forgotten: it's a lookup again, and it's not there.
    c.call(
        "vibe_delete_agent",
        json!({"agent": "there", "force": true}),
    )
    .await;
    heard();
    let gone = c.fails("vibe_read_output", json!({"agent": "there"})).await;
    assert!(
        gone.contains("no agent") || gone.contains("not found"),
        "{gone}"
    );
    assert!(heard().contains(&"agents.list".to_owned()));

    // While the Mac can't answer, its memory is left out of the listing.
    w.wires.freeze(true);
    let listed = c.call("vibe_list_agents", json!({})).await;
    assert!(!listed.contains("mac: "), "{listed}");
}

/// Rule 5, with the Mac asleep: a ref this key started there is known to be there, so
/// it says so at once, rather than that it can't be found.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_ref_started_on_the_mac_is_known_while_the_mac_sleeps() {
    let w = world().await;
    w.turn_on().await;
    let key = w.key("laptop");
    w.pushed(&key).await;
    let mut c = w.client(&key);
    c.call(
        "vibe_start_agent",
        json!({"project": "app", "profile": "software-eng-mac", "prompt": "talk",
               "host": "mac", "name": "there"}),
    )
    .await;
    w.wires.freeze(true);
    let failed = c.fails("vibe_read_output", json!({"agent": "there"})).await;
    assert!(failed.contains("mac is unavailable"), "{failed}");
    assert!(!failed.contains("isn't on any host"), "{failed}");
}

/// A start held in a frozen tunnel: reported unknown. It runs if the Mac wakes in time,
/// and not if it wakes past the window, when its caller may well have tried again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_start_held_up_by_a_sleeping_mac_doesnt_run_once_its_too_old() {
    let w = world().await;
    w.turn_on().await;
    let key = w.key("laptop");
    w.pushed(&key).await;
    let mut c = w.client(&key);
    let start = |name: &str| {
        json!({"project": "app", "profile": "software-eng-mac", "prompt": "talk",
               "host": "mac", "name": name})
    };

    // In time: the outcome really was unknown, and it runs.
    w.wires.freeze(true);
    let failed = c.fails("vibe_start_agent", start("in-time")).await;
    assert!(failed.contains("Outcome unknown"), "{failed}");
    assert!(failed.contains("check vibe_list_agents"), "{failed}");
    w.thaw().await;
    assert!(
        w.listing_shows(&mut c, "mac:in-time").await,
        "held for less than the window, it runs"
    );

    // Past the window: the same, but it doesn't.
    w.wires.freeze(true);
    let failed = c.fails("vibe_start_agent", start("too-late")).await;
    assert!(failed.contains("Outcome unknown"), "{failed}");
    w.mac_clock_ahead.store(61, Ordering::SeqCst);
    w.thaw().await;
    assert!(
        !w.listing_shows(&mut c, "too-late").await,
        "held past the window, it must not run"
    );

    // A clock that far out is the same to the Mac, and that's what it says.
    let refused = c.fails("vibe_start_agent", start("skewed")).await;
    assert!(refused.contains("didn't run"), "{refused}");
    assert!(refused.contains("clocks"), "{refused}");
    assert!(!w.listing_shows(&mut c, "skewed").await);

    // With the clocks agreeing, trying again works.
    w.mac_clock_ahead.store(0, Ordering::SeqCst);
    c.call("vibe_start_agent", start("too-late")).await;
    assert!(w.listing_shows(&mut c, "mac:too-late").await);
}

/// A delete held the same way: the agent is still there, past the window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delete_held_up_by_a_sleeping_mac_doesnt_run_once_its_too_old() {
    let w = world().await;
    w.turn_on().await;
    let key = w.key("laptop");
    w.pushed(&key).await;
    let mut c = w.client(&key);
    c.call(
        "vibe_start_agent",
        json!({"project": "app", "profile": "software-eng-mac", "prompt": "talk",
               "host": "mac", "name": "on-mac"}),
    )
    .await;
    let delete = json!({"agent": "mac:on-mac", "force": true});

    w.wires.freeze(true);
    let failed = c.fails("vibe_delete_agent", delete.clone()).await;
    assert!(failed.contains("Outcome unknown"), "{failed}");
    assert!(failed.contains("check vibe_list_agents"), "{failed}");
    w.mac_clock_ahead.store(61, Ordering::SeqCst);
    w.thaw().await;
    // Only now can the clocks agree again, or it would run after all.
    w.wires.calls_over("agents.delete", 1).await;
    assert!(w.mac_has("on-mac").await, "held past the window, it ran");
    let listed = w.listing_the_mac_answered(&mut c).await;
    assert!(listed.contains("mac:on-mac"), "{listed}");

    w.mac_clock_ahead.store(0, Ordering::SeqCst);
    // A Mac this short of time can miss the deadline for taking a call, or a look at it,
    // and a caller is told to look and try again. A look that missed can also refuse a
    // call, which sends nothing. Anything else fails, as does a Mac that never takes it.
    let mut sent = 1;
    let mut failed = String::new();
    for _ in 0..20 {
        w.thaw().await;
        let (error, text) = c.tool("vibe_delete_agent", delete.clone()).await;
        if !error {
            failed.clear();
            break;
        }
        let missed = text.contains("didn't take agents.delete");
        let looked = text.contains("it stopped answering");
        assert!(
            missed || looked || text.contains("so nothing was sent"),
            "{text}"
        );
        if missed || looked {
            // A missed deadline is sure to have been sent. The other may not have been.
            sent += usize::from(missed);
            w.wires.calls_over("agents.delete", sent).await;
            if !w.mac_has("on-mac").await {
                failed.clear();
                break;
            }
        }
        failed = text;
    }
    assert!(failed.is_empty(), "the Mac never took it: {failed}");
    assert!(!w.mac_has("on-mac").await);
    let listed = w.listing_the_mac_answered(&mut c).await;
    assert!(!listed.contains("mac:on-mac"), "{listed}");
}

/// A send held the same way: nothing's typed, past the window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_held_up_by_a_sleeping_mac_isnt_typed_once_its_too_old() {
    let w = world().await;
    w.turn_on().await;
    let key = w.key("laptop");
    w.pushed(&key).await;
    let mut c = w.client(&key);
    let agent = "mac:on-mac";
    c.call(
        "vibe_start_agent",
        json!({"project": "app", "profile": "software-eng-mac", "prompt": "talk",
               "host": "mac", "name": "on-mac"}),
    )
    .await;
    c.call("vibe_wait", json!({"agents": agent, "timeout": 60}))
        .await;

    w.wires.freeze(true);
    let failed = c
        .fails("vibe_send", json!({"agent": agent, "text": "too late"}))
        .await;
    assert!(failed.contains("Outcome unknown"), "{failed}");
    w.mac_clock_ahead.store(61, Ordering::SeqCst);
    w.thaw().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let read = c
        .call("vibe_read_messages", json!({"agent": agent, "last": 50}))
        .await;
    assert!(!read.contains("> too late"), "{read}");
}

/// A send, start or delete a minute late, as the bytes of one might be after a long
/// sleep: it doesn't run. Reads don't care.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_change_too_old_when_it_reaches_the_mac_doesnt_run() {
    let w = world().await;
    w.turn_on().await;
    let key = w.key("laptop");
    w.pushed(&key).await;
    let mut c = w.client(&key);
    let agent = "mac:on-mac";
    c.call(
        "vibe_start_agent",
        json!({"project": "app", "profile": "software-eng-mac", "prompt": "talk",
               "host": "mac", "name": "on-mac"}),
    )
    .await;
    c.call("vibe_wait", json!({"agents": agent, "timeout": 60}))
        .await;

    let socket = w.host_world.home.join(".local/share/vibeos/run/mac.sock");
    let send = async |text: &str, age: i64| {
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": "tunnel.call", "params": {
            "key": key, "method": "agents.send", "sent_at": common::GOLDEN_NOW - age,
            "params": {"ref": "on-mac", "text": text},
        }});
        let mut s = UnixStream::connect(&socket).await.unwrap();
        s.write_all(format!("{line}\n").as_bytes()).await.unwrap();
        let mut r = BufReader::new(s);
        answer_from(&mut r).await
    };
    let r = send("too late", 61).await;
    assert_eq!(r["error"]["code"], ErrorCode::Conflict.code(), "{r}");
    let why = r["error"]["message"].as_str().unwrap();
    assert!(why.contains("agents.send took 61 s"), "{why}");
    assert!(why.contains("didn't run"), "{why}");
    assert!(why.contains("clocks"), "{why}");
    let raw = async |method: &str, params: Value, age: i64| {
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": "tunnel.call", "params": {
            "key": key, "method": method, "sent_at": common::GOLDEN_NOW - age, "params": params,
        }});
        let mut s = UnixStream::connect(&socket).await.unwrap();
        s.write_all(format!("{line}\n").as_bytes()).await.unwrap();
        let mut r = BufReader::new(s);
        answer_from(&mut r).await
    };
    for (method, params) in [
        (
            "agents.start",
            json!({"project": "app", "profile": "software-eng-mac", "prompt": "talk",
                   "name": "too-late"}),
        ),
        ("agents.delete", json!({"ref": "on-mac", "force": true})),
    ] {
        let r = raw(method, params, 61).await;
        assert_eq!(r["error"]["code"], ErrorCode::Conflict.code(), "{r}");
        let why = r["error"]["message"].as_str().unwrap();
        assert!(why.contains(&format!("{method} took 61 s")), "{why}");
    }
    let r = raw("agents.list", json!({}), 61).await;
    assert!(r["result"].is_object(), "a read has nothing to fear: {r}");
    let names = r["result"]["agents"].to_string();
    assert!(names.contains("on-mac"), "{names}");
    assert!(!names.contains("too-late"), "{names}");
    let r = send("in time", 30).await;
    assert!(r["result"].is_object(), "{r}");

    c.call("vibe_wait", json!({"agents": agent, "timeout": 60}))
        .await;
    let read = c
        .call("vibe_read_messages", json!({"agent": agent, "last": 50}))
        .await;
    assert!(read.contains("> in time"), "{read}");
    assert!(!read.contains("> too late"), "{read}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sleeping_mac_holds_up_nothing_on_the_host() {
    let w = world().await;
    w.turn_on().await;
    let key = w.key("laptop");
    w.pushed(&key).await;
    let mut c = w.client(&key);
    for (host, name, profile) in [
        ("local", "on-vps", "software-eng-linux"),
        ("mac", "on-mac", "software-eng-mac"),
        ("local", "on-both", "software-eng-linux"),
        ("mac", "on-both", "software-eng-mac"),
    ] {
        c.call(
            "vibe_start_agent",
            json!({"project": "app", "profile": profile, "prompt": "talk",
                   "host": host, "name": name}),
        )
        .await;
    }
    for (agent, whose) in [
        ("on-vps", "local"),
        ("mac:on-mac", "mac"),
        ("local:on-both", "local"),
        ("mac:on-both", "mac"),
    ] {
        c.call("vibe_wait", json!({"agents": agent, "timeout": 60}))
            .await;
        c.call(
            "vibe_send",
            json!({"agent": agent, "text": format!("hello {whose}")}),
        )
        .await;
        c.call("vibe_wait", json!({"agents": agent, "timeout": 60}))
            .await;
    }
    // A key's first listing of the Mac is what later ones fall back on. It has nothing
    // to fall back on itself, so a busy machine may need a second go.
    assert!(w.listing_shows(&mut c, "mac:on-mac").await);
    let listed = c.call("vibe_list_agents", json!({})).await;
    assert!(listed.contains("mac:on-mac"), "{listed}");
    assert!(!listed.contains("unavailable"), "{listed}");

    // This host wins a bare ref that's on both, with no asking.
    let read = c
        .call("vibe_read_messages", json!({"agent": "on-both", "last": 1}))
        .await;
    assert!(read.contains("> hello local"), "{read}");
    let read = c
        .call(
            "vibe_read_messages",
            json!({"agent": "mac:on-both", "last": 1}),
        )
        .await;
    assert!(read.contains("> hello mac"), "{read}");

    // Which projects are where is known from here on.
    c.call("vibe_list_projects", json!({})).await;

    // The Mac sleeps. Its tunnel's end goes on accepting, with nothing behind it,
    // and before anything's noticed that costs a listing the peer deadline at most.
    w.wires.freeze(true);
    let started = Instant::now();
    let listed = c.call("vibe_list_agents", json!({})).await;
    assert!(
        started.elapsed() < Duration::from_millis(vibe_core::limits::PEER_READ * 1000 + 500),
        "{:?}",
        started.elapsed()
    );
    assert!(listed.contains("on-vps"), "{listed}");
    let mac_line = listed
        .lines()
        .find(|l| l.starts_with("mac:on-mac"))
        .unwrap_or_else(|| panic!("{listed}"));
    assert!(
        mac_line.contains("unavailable") && mac_line.contains("last seen at 20"),
        "{mac_line}"
    );

    // Once the look at the tunnel has failed, nothing waits at all.
    for _ in 0..1000 {
        if w.host.hosts().unwrap().hosts[0].tunnel != Some(TunnelState::Up) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_ne!(
        w.host.hosts().unwrap().hosts[0].tunnel,
        Some(TunnelState::Up)
    );
    let started = Instant::now();
    let listed = c.call("vibe_list_agents", json!({})).await;
    assert!(listed.contains("on-vps"), "{listed}");
    assert!(listed.contains("unavailable"), "{listed}");
    let projects = c.call("vibe_list_projects", json!({})).await;
    assert!(projects.contains("app (on local)"), "{projects}");
    assert!(projects.contains("mac is unavailable"), "{projects}");
    let read = c
        .call("vibe_read_messages", json!({"agent": "on-vps", "last": 1}))
        .await;
    assert!(read.contains("> hello local"), "{read}");
    // Where a bare ref was last seen is known, so it's not looked for.
    let failed = c
        .fails("vibe_read_messages", json!({"agent": "on-mac", "last": 1}))
        .await;
    assert!(failed.contains("mac is unavailable"), "{failed}");
    let failed = c
        .fails(
            "vibe_send",
            json!({"agent": "mac:on-mac", "text": "anyone?"}),
        )
        .await;
    assert!(failed.contains("mac is unavailable"), "{failed}");
    // A project on both is still on both, however long the Mac takes to answer.
    let ambiguous = c
        .fails(
            "vibe_start_agent",
            json!({"project": "app", "profile": "software-eng-linux", "prompt": "talk",
                   "name": "while-away"}),
        )
        .await;
    assert!(ambiguous.contains("app is a project on"), "{ambiguous}");
    let started_here = c
        .call(
            "vibe_start_agent",
            json!({"project": "app", "profile": "software-eng-linux", "prompt": "talk",
                   "host": "local", "name": "while-away"}),
        )
        .await;
    assert!(
        started_here.contains("started while-away"),
        "{started_here}"
    );
    // A call already on its way when the Mac went waits for two looks to fail.
    assert!(
        started.elapsed() < Duration::from_secs(25),
        "all of that together: {:?}",
        started.elapsed()
    );

    // It wakes, and it all snaps back with nothing done.
    w.wires.freeze(false);
    for _ in 0..1000 {
        if w.host.hosts().unwrap().hosts[0].tunnel == Some(TunnelState::Up) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let listed = c.call("vibe_list_agents", json!({})).await;
    assert!(listed.contains("mac:on-mac"), "{listed}");
    assert!(!listed.contains("unavailable"), "{listed}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_mac_checks_every_key_itself() {
    let w = world().await;
    w.turn_on().await;
    // A key of the host's own works there, for its agents, but the Mac
    // doesn't take it.
    let own = w
        .host
        .create_key(KeyCreateParams {
            name: "host-own".into(),
        })
        .unwrap()
        .secret;
    let mut c = w.client(&own);
    // Nor does it tell such a key its memory.
    let listed = c.call("vibe_list_agents", json!({})).await;
    assert!(listed.contains("\nlocal: "), "{listed}");
    assert!(!listed.contains("\nmac: "), "{listed}");
    let projects = c.call("vibe_list_projects", json!({})).await;
    assert!(projects.contains("app (on local)"), "{projects}");
    assert!(!projects.contains("app (on mac)"), "{projects}");
    assert!(projects.contains("mac is unavailable"), "{projects}");
    assert!(projects.contains("doesn't take that API key"), "{projects}");
    let refused = c
        .fails(
            "vibe_start_agent",
            json!({"project": "app", "profile": "software-eng-mac", "prompt": "talk", "host": "mac"}),
        )
        .await;
    assert!(refused.contains("doesn't take that API key"), "{refused}");
    assert!(
        w.mac
            .agents(Default::default())
            .await
            .unwrap()
            .agents
            .is_empty(),
        "nothing started on the Mac"
    );

    // A host that's been taken over can use the tunnel, but it gets
    // nothing from the Mac without a key the Mac knows, whatever it asks.
    let socket = w.host_world.home.join(".local/share/vibeos/run/mac.sock");
    let ask = |line: Value| {
        let socket = socket.clone();
        async move {
            let mut s = UnixStream::connect(&socket).await.unwrap();
            s.write_all(format!("{line}\n").as_bytes()).await.unwrap();
            let mut r = BufReader::new(s);
            answer_from(&mut r).await
        }
    };
    let call = |key: &str, method: &str, params: Value| {
        json!({"jsonrpc": "2.0", "id": 1, "method": "tunnel.call",
               "params": {"key": key, "method": method, "params": params}})
    };
    let r = ask(call(&own, "agents.list", json!({}))).await;
    assert_eq!(r["error"]["code"], ErrorCode::Unauthorized.code(), "{r}");
    let r = ask(call("not a key", "agents.list", json!({}))).await;
    assert_eq!(r["error"]["code"], ErrorCode::Unauthorized.code(), "{r}");
    // Only the agent calls MCP makes, for the Mac's own agents, and never
    // the socket API's other methods, keyed or not.
    let key = w.key("laptop");
    for method in [
        "keys.create",
        "keys.list",
        "hosts.add",
        "events.subscribe",
        "sessions.attach",
    ] {
        let r = ask(call(&key, method, json!({}))).await;
        assert_eq!(
            r["error"]["code"],
            ErrorCode::MethodNotFound.code(),
            "{method}: {r}"
        );
    }
    let r =
        ask(json!({"jsonrpc": "2.0", "id": 1, "method": "keys.create", "params": {"name": "x"}}))
            .await;
    assert_eq!(r["error"]["code"], ErrorCode::MethodNotFound.code(), "{r}");
    let r = ask(call(&key, "agents.list", json!({"host": "vps"}))).await;
    assert_eq!(r["error"]["code"], ErrorCode::InvalidParams.code(), "{r}");
    let r = ask(call(&key, "agents.list", json!({}))).await;
    assert_eq!(r["result"]["agents"], json!([]), "{r}");
    let r = ask(call(&key, "agents.list", json!({"host": "local"}))).await;
    assert!(r["result"].is_object(), "{r}");
    // hosts.list there is the Mac's own row alone, for its memory: never
    // the hosts linked to it, or their addresses.
    let r = ask(call(&key, "hosts.list", json!({}))).await;
    let hosts = r["result"]["hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 1, "{r}");
    assert_eq!(hosts[0]["name"], "local", "{r}");
    assert_eq!(hosts[0]["memory"]["total"], 64u64 << 30, "{r}");
    assert!(!r.to_string().contains("vps.example.com"), "{r}");
    let r = ask(call(&own, "hosts.list", json!({}))).await;
    assert_eq!(r["error"]["code"], ErrorCode::Unauthorized.code(), "{r}");
    // Revoked on the Mac, refused by the Mac, even before any host hears.
    w.mac
        .revoke_key(KeyParams {
            name: "laptop".into(),
        })
        .unwrap();
    let r = ask(call(&key, "agents.list", json!({}))).await;
    assert_eq!(r["error"]["code"], ErrorCode::Unauthorized.code(), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_tunnel_says_why_it_is_down() {
    let w = world().await;
    w.wires.forwarding.store(false, Ordering::SeqCst);
    // It's what the next tunnel finds.
    w.wires.pull();
    w.wait("the link down", |h| h.link == HostLink::Down).await;
    w.wires.plug_in();
    w.wait("the tunnel's trouble", |h| {
        h.tunnel == Some(TunnelState::Down)
            && h.tunnel_detail
                .as_deref()
                .is_some_and(|d| d.contains("refused to forward"))
    })
    .await;
    // Fixed on the server: it comes up on its own.
    w.wires.forwarding.store(true, Ordering::SeqCst);
    w.wait("the tunnel", |h| h.tunnel == Some(TunnelState::Up))
        .await;
    assert_eq!(w.vps().tunnel_detail, None);
    // A second tunnel replaced the first's end: the host cleared it.
    w.wires.pull();
    w.wait("the tunnel down", |h| h.tunnel == Some(TunnelState::Down))
        .await;
    w.wires.plug_in();
    w.wait("the tunnel back", |h| h.tunnel == Some(TunnelState::Up))
        .await;
    assert!(*w.wires.tunnels.lock().unwrap() >= 2);
}

/// Issue #45: the host's `vibed` restarted, as an upgrade does, while the
/// tunnel was up, took the tunnel's end in `run/` for a dead session's
/// socket and removed it, and the Mac was unavailable from then on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_host_keeps_the_macs_tunnel() {
    let w = world().await;
    w.turn_on().await;
    let key = w.key("laptop");
    w.pushed(&key).await;
    w.mac_reachable(&key).await;
    let tunnels = *w.wires.tunnels.lock().unwrap();

    let w = w.restart_host().await;
    // Its scan for sessions on start, and every one after, leaves the
    // tunnel's end alone: it's no session.
    let sessions = rpc(&w.host, "sessions.list", json!({})).await;
    assert_eq!(sessions["result"]["sessions"], json!([]), "{sessions}");
    assert!(w.tunnel_end().exists(), "the scan removed the tunnel's end");
    w.pushed(&key).await;
    w.mac_reachable(&key).await;
    assert!(w.tunnel_end().exists());
    // Through the same tunnel: nothing had to start it over.
    assert_eq!(*w.wires.tunnels.lock().unwrap(), tunnels);
    assert_eq!(w.vps().tunnel, Some(TunnelState::Up));

    // Nor can an agent take the tunnel's end, or any socket of vibed's.
    let mut c = w.client(&key);
    for (host, profile, name) in [
        ("local", "software-eng-linux", "mac"),
        ("local", "software-eng-linux", "tunnel"),
        ("mac", "software-eng-mac", "tunnel"),
        ("mac", "software-eng-mac", "vibed"),
    ] {
        let refused = c
            .fails(
                "vibe_start_agent",
                json!({"project": "app", "profile": profile, "prompt": "talk",
                       "host": host, "name": name}),
            )
            .await;
        assert!(
            refused.contains(&format!(
                "`{name}` is a name VibeOS keeps for itself; pick another name"
            )),
            "{refused}"
        );
    }
}

/// A tunnel whose end on the host is gone while `ssh` runs on (a cleanup
/// of `run/`, or the scan of issue #45) has the host stop coming through
/// it: the Mac starts it over, so it heals without anyone ending `ssh`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tunnel_whose_end_is_gone_starts_over() {
    let timing = Timing {
        tunnel_quiet: Duration::from_secs(2),
        ..quick()
    };
    let w = world_timed(timing).await;
    w.turn_on().await;
    let key = w.key("laptop");
    w.pushed(&key).await;
    w.mac_reachable(&key).await;
    let tunnels = *w.wires.tunnels.lock().unwrap();

    std::fs::remove_file(w.tunnel_end()).unwrap();
    for _ in 0..1000 {
        if *w.wires.tunnels.lock().unwrap() > tunnels {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        *w.wires.tunnels.lock().unwrap() > tunnels,
        "the Mac never started the tunnel over: {:?}",
        w.vps()
    );
    w.wait("the tunnel back", |h| h.tunnel == Some(TunnelState::Up))
        .await;
    assert!(w.tunnel_end().exists());
    w.mac_reachable(&key).await;
}

/// A second Linux host linked to the same Mac, with the project `app`. It has no SSH to the
/// first, and needs none.
struct Edge {
    core: Arc<Core>,
    _world: Golden,
    _server: TestServer,
    _mcp: tokio::task::JoinHandle<()>,
}

impl World {
    async fn add_edge(&self) -> Edge {
        let world = Golden::new();
        let (core, server, mcp) =
            open_host_at(&world, free_port(), &self.wires, "me@edge.example.com").await;
        let added = rpc(
            &self.mac,
            "hosts.add",
            json!({"name": "edge", "address": "me@edge.example.com"}),
        )
        .await;
        assert!(added.get("result").is_some(), "{added}");
        for _ in 0..500 {
            let up = self
                .mac
                .hosts()
                .unwrap()
                .hosts
                .iter()
                .any(|h| h.name == "edge" && h.link == HostLink::Connected);
            if up {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let path = world.home.join("src/app");
        let project = rpc(
            &self.mac,
            "projects.add",
            json!({"host": "edge", "path": path}),
        )
        .await;
        assert!(project.get("result").is_some(), "{project}");
        Edge {
            core,
            _world: world,
            _server: server,
            _mcp: mcp,
        }
    }
}

/// One line down the tunnel, as what's on the host sends it, and the answer.
async fn down_the_tunnel(end: &Path, method: &str, key: &str, params: Value) -> Value {
    let stream = UnixStream::connect(end).await.unwrap();
    let (read, mut write) = stream.into_split();
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "tunnel.call",
                      "params": {"key": key, "method": method, "params": params}});
    write
        .write_all(format!("{line}\n").as_bytes())
        .await
        .unwrap();
    let mut read = BufReader::new(read.reunite(write).unwrap());
    loop {
        let answer = answer_from(&mut read).await;
        if answer.get("id").is_some() {
            return answer;
        }
    }
}

/// #143, as Lee has it: one MCP on one Linux host reaches every host in the Mac's hub while
/// the Mac is there, its own, the Mac's and another host's, through the Mac alone. With the
/// Mac away, what went through it is unavailable and says so, and the host's own goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_ingress_reaches_every_host_in_the_macs_hub() {
    let w = world().await;
    let edge = w.add_edge().await;
    w.turn_on().await;
    let key = w.key("laptop");
    w.pushed(&key).await;
    for _ in 0..500 {
        if edge.core.verify_key(&key).unwrap().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    w.mac_reachable(&key).await;
    let mut c = w.client(&key);

    // All three, by what they go by here; never this host under the Mac's name for it.
    let projects = c.call("vibe_list_projects", json!({})).await;
    assert!(projects.contains("app (on local)"), "{projects}");
    assert!(projects.contains("app (on mac)"), "{projects}");
    assert!(projects.contains("app (on edge)"), "{projects}");
    assert!(!projects.contains("(on vps)"), "{projects}");

    // An agent on each, started and talked to here.
    for (host, profile, name, shown) in [
        ("local", "software-eng-linux", "on-vps", "on-vps"),
        ("mac", "software-eng-mac", "on-mac", "mac:on-mac"),
        ("edge", "software-eng-linux", "on-edge", "edge:on-edge"),
    ] {
        let started = c
            .call(
                "vibe_start_agent",
                json!({"project": "app", "profile": profile, "prompt": "talk", "host": host,
                       "name": name}),
            )
            .await;
        assert!(started.contains(&format!("started {shown}")), "{started}");
        c.call("vibe_wait", json!({"agents": shown, "timeout": 60}))
            .await;
        let line = format!("hello to {shown}");
        c.call("vibe_send", json!({"agent": shown, "text": line}))
            .await;
        c.call("vibe_wait", json!({"agents": shown, "timeout": 60}))
            .await;
        let read = c
            .call("vibe_read_messages", json!({"agent": shown, "last": 2}))
            .await;
        assert!(read.contains(&line), "{shown}: {read}");
    }
    // It's the other host's own agent, on that host, with no SSH from this one.
    let on_edge = edge.core.agents(Default::default()).await.unwrap();
    assert!(on_edge.agents.iter().any(|a| a.agent == "on-edge"));
    // A bare ref finds it too.
    let read = c
        .call("vibe_read_messages", json!({"agent": "on-edge", "last": 1}))
        .await;
    assert!(!read.contains("unavailable"), "{read}");
    let listed = w.listing_the_mac_answered(&mut c).await;
    for shown in ["on-vps", "mac:on-mac", "edge:on-edge"] {
        assert!(listed.contains(shown), "{shown}: {listed}");
    }

    // The tunnel itself: nothing of what's linked for want of a key, never the host asking,
    // and no addresses.
    let end = w.tunnel_end();
    let refused = down_the_tunnel(&end, "hosts.list", "vk_wrong", json!({})).await;
    assert_eq!(
        refused["error"]["code"],
        ErrorCode::Unauthorized.code(),
        "{refused}"
    );
    let rows = down_the_tunnel(&end, "hosts.list", &key, json!({})).await;
    let names: Vec<&str> = rows["result"]["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["local", "edge"], "{rows}");
    assert!(!rows.to_string().contains("example.com"), "{rows}");
    let back = down_the_tunnel(&end, "agents.list", &key, json!({"host": "vps"})).await;
    assert_eq!(
        back["error"]["code"],
        ErrorCode::InvalidParams.code(),
        "{back}"
    );
    let no_such = down_the_tunnel(&end, "agents.list", &key, json!({"host": "nope"})).await;
    assert_eq!(
        no_such["error"]["code"],
        ErrorCode::InvalidParams.code(),
        "{no_such}"
    );

    // A key of the host's own stays on the host: the Mac doesn't take it, so it hears of no
    // other host, and can start nothing there.
    let own = w
        .host
        .create_key(KeyCreateParams {
            name: "host-own".into(),
        })
        .unwrap()
        .secret;
    let mut only_here = w.client(&own);
    let projects = only_here.call("vibe_list_projects", json!({})).await;
    assert!(!projects.contains("(on edge)"), "{projects}");
    let refused = only_here
        .fails(
            "vibe_start_agent",
            json!({"project": "app", "profile": "software-eng-linux", "prompt": "talk",
                   "host": "edge", "name": "nope"}),
        )
        .await;
    assert!(refused.contains("edge"), "{refused}");
    assert!(
        !edge
            .core
            .agents(Default::default())
            .await
            .unwrap()
            .agents
            .iter()
            .any(|a| a.agent == "nope")
    );

    // The Mac goes to sleep: what's through it is unavailable, never claimed; the host's own
    // answers, and new work can start on it.
    w.wires.freeze(true);
    w.tunnel_down().await;
    let listed = c.call("vibe_list_agents", json!({})).await;
    let row = |listed: &str, shown: &str| {
        listed
            .lines()
            .find(|l| l.starts_with(shown))
            .unwrap_or_else(|| panic!("no {shown} in {listed}"))
            .to_owned()
    };
    assert!(!row(&listed, "on-vps").contains("unavailable"), "{listed}");
    assert!(
        row(&listed, "mac:on-mac").contains("unavailable"),
        "{listed}"
    );
    assert!(
        row(&listed, "edge:on-edge").contains("unavailable"),
        "{listed}"
    );
    let read = c
        .call("vibe_read_messages", json!({"agent": "on-vps", "last": 1}))
        .await;
    assert!(read.contains("on-vps"), "{read}");
    let refused = c
        .fails(
            "vibe_start_agent",
            json!({"project": "app", "profile": "software-eng-linux", "prompt": "talk",
                   "host": "edge", "name": "while-away"}),
        )
        .await;
    assert!(refused.contains("unavailable"), "{refused}");
    let started = c
        .call(
            "vibe_start_agent",
            json!({"project": "app", "profile": "software-eng-linux", "prompt": "talk",
                   "host": "local", "name": "while-away"}),
        )
        .await;
    assert!(started.contains("started while-away"), "{started}");

    // It's back, with nothing done, and it all merges again.
    w.thaw().await;
    let listed = w.listing_the_mac_answered(&mut c).await;
    for shown in ["on-vps", "while-away", "mac:on-mac", "edge:on-edge"] {
        assert!(listed.contains(shown), "{shown}: {listed}");
    }
    c.call(
        "vibe_read_messages",
        json!({"agent": "edge:on-edge", "last": 1}),
    )
    .await;

    // A revoke on the Mac reaches all of it.
    for agent in ["on-vps", "while-away", "mac:on-mac", "edge:on-edge"] {
        c.call("vibe_delete_agent", json!({"agent": agent, "force": true}))
            .await;
    }
    w.mac
        .revoke_key(KeyParams {
            name: "laptop".into(),
        })
        .unwrap();
    let refused = down_the_tunnel(&end, "hosts.list", &key, json!({})).await;
    assert_eq!(
        refused["error"]["code"],
        ErrorCode::Unauthorized.code(),
        "{refused}"
    );
}

/// A Linux host no Mac has opened a tunnel to has no `mac` to speak of, and says nothing's
/// unavailable, however it's reached. Loopback serves it all the same, with a key.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_with_no_mac_only_has_itself() {
    let wires = Wires::new();
    let world = Golden::new();
    let port = free_port();
    let (host, _server, _mcp) = open_host(&world, port, &wires).await;
    let key = host
        .create_key(KeyCreateParams { name: "own".into() })
        .unwrap()
        .secret;
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    for _ in 0..500 {
        if TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut c = Client {
        addr,
        key: Some(key),
        next: 0,
    };
    let projects = c.call("vibe_list_projects", json!({})).await;
    assert!(!projects.contains("mac"), "{projects}");
    assert!(!projects.contains("unavailable"), "{projects}");
    assert_eq!(host.hosts().unwrap().hosts[0].tunnel, None);
    let mut nobody = Client {
        addr,
        key: None,
        next: 0,
    };
    assert_eq!(nobody.request("tools/list", json!({})).await.status, 401);
}
