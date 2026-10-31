//! External access, end to end in one process (PRD: External access): a
//! Mac and a Linux host joined by a link as `ssh <host> vibe rpc` joins
//! them, and by a reverse tunnel as `ssh -R` makes it, a Unix socket on the
//! host forwarded to the Mac's tunnel socket. An outside MCP client talks
//! HTTPS to the host's public endpoint with a key: it starts and talks to
//! an agent on each, gets nothing without a key, and is cut off on every
//! host when the key is revoked on the Mac. The Mac checks every call that
//! comes down the tunnel itself; while the tunnel is down, the host answers
//! for its own agents and the Mac's are unavailable.
//!
//! The same flow with the real `vibed` binaries and the real install
//! scripts is vibe-cli's tests/linux_host.rs; against a real server and
//! Let's Encrypt, scripts/check-full.sh's external step.

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Golden, TestServer, serve};
use rustls::pki_types::{CertificateDer, ServerName};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixListener, UnixStream};
use tokio::sync::watch;
use vibe_core::model::{HostLink, TunnelState};
use vibe_core::paths::Layout;
use vibe_core::rpc::{ErrorCode, HostAddParams, KeyCreateParams, KeyParams, MAX_MESSAGE_BYTES};
use vibe_mcp::public::Limits;
use vibed::core::Core;
use vibed::external::ExternalConfig;
use vibed::link::Timing;
use vibed::remote::{BoxFuture, Channel, Installed, Remote, TunnelChild};

const DOMAIN: &str = "vps.test";

/// `ssh`, in process: a dial connects to the host's socket, and a tunnel
/// relays the host's end to the Mac's tunnel socket. Pulling the network
/// ends both.
#[derive(Debug)]
struct Wires {
    sockets: Mutex<HashMap<String, PathBuf>>,
    up: watch::Sender<bool>,
    tunnels: Mutex<u32>,
    /// Whether tunnels may be made (sshd might refuse to forward).
    forwarding: AtomicBool,
}

impl Wires {
    fn new() -> Arc<Wires> {
        Arc::new(Wires {
            sockets: Mutex::new(HashMap::new()),
            up: watch::channel(true).0,
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
}

/// Copies bytes both ways until either side ends or the network goes.
async fn pump(a: UnixStream, b: UnixStream, mut up: watch::Receiver<bool>) {
    let (mut ar, mut aw) = a.into_split();
    let (mut br, mut bw) = b.into_split();
    let mut x = vec![0u8; 1 << 16];
    let mut y = vec![0u8; 1 << 16];
    loop {
        if !*up.borrow_and_update() {
            return;
        }
        tokio::select! {
            _ = up.changed() => {}
            n = ar.read(&mut x) => match n {
                Ok(0) | Err(_) => return,
                Ok(n) => if bw.write_all(&x[..n]).await.is_err() { return },
            },
            n = br.read(&mut y) => match n {
                Ok(0) | Err(_) => return,
                Ok(n) => if aw.write_all(&y[..n]).await.is_err() { return },
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
            tokio::spawn(pump(host, theirs, self.up.subscribe()));
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
            Ok(TunnelChild {
                ended: Box::pin(async move {
                    loop {
                        tokio::select! {
                            accepted = listener.accept() => {
                                let Ok((there, _)) = accepted else { continue };
                                let Ok(here) = UnixStream::connect(&local).await else { continue };
                                tokio::spawn(pump(there, here, pumps.subscribe()));
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
        ping_timeout: Duration::from_millis(600),
        connect_timeout: Duration::from_secs(5),
        min_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(300),
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A certificate for the domain, as Let's Encrypt would give, in `dir`.
fn certificate(dir: &Path) -> (PathBuf, PathBuf, CertificateDer<'static>) {
    let key = rcgen::KeyPair::generate().unwrap();
    let params = rcgen::CertificateParams::new(vec![DOMAIN.to_owned()]).unwrap();
    let cert = params.self_signed(&key).unwrap();
    let (c, k) = (dir.join("cert.pem"), dir.join("key.pem"));
    std::fs::write(&c, cert.pem()).unwrap();
    std::fs::write(&k, key.serialize_pem()).unwrap();
    (c, k, cert.der().clone())
}

struct World {
    mac: Arc<Core>,
    host: Arc<Core>,
    wires: Arc<Wires>,
    mac_world: Golden,
    host_world: Golden,
    public: SocketAddr,
    cert: CertificateDer<'static>,
    _server: TestServer,
    _certs: tempfile::TempDir,
}

/// A Mac and, at `me@vps.example.com`, a Linux host, both with the project
/// `app` and running agents under the test backend. The host serves its
/// public endpoint, once it's on, on a port of its own with a fixed
/// certificate, and a client gets `limits` there.
async fn world(limits: Limits) -> World {
    let wires = Wires::new();
    let mac_world = Golden::new();
    let mut config = mac_world.config();
    config.remote = Arc::clone(&wires) as Arc<dyn Remote>;
    config.link_timing = quick();
    let mac = Arc::new(Core::open(config).unwrap());
    mac.spawn_links();
    mac.spawn_external();

    let certs = common::short_tempdir();
    let (cert_pem, key_pem, cert) = certificate(certs.path());
    let port = free_port();
    let host_world = Golden::new();
    let mut config = host_world.config();
    config.info.os = "linux".into();
    config.label = "vps-1".into();
    config.layout = Layout::new(&host_world.home, "linux");
    config.toolbox = None;
    // Its own keys aren't the Mac's.
    config.entropy = Arc::new(vibe_core::keys::random_bytes);
    config.external = ExternalConfig {
        serve_here: true,
        port,
        // Loopback only: the tests open nothing to the network.
        listen: Some(std::net::Ipv4Addr::LOCALHOST.into()),
        fixed_cert: Some((cert_pem, key_pem)),
        probe_every: Duration::from_millis(100),
        limits,
        ..ExternalConfig::default()
    };
    let host = Arc::new(Core::open(config).unwrap());
    host.spawn_links();
    host.spawn_external();
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
    let w = World {
        mac,
        host,
        wires,
        mac_world,
        host_world,
        public: SocketAddr::from(([127, 0, 0, 1], port)),
        cert,
        _server: server,
        _certs: certs,
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
            cert: self.cert.clone(),
            key: Some(key.to_owned()),
            next: 0,
        }
    }

    async fn turn_on(&self) {
        let on = rpc(
            &self.mac,
            "hosts.external",
            json!({"host": "vps", "on": true, "domain": DOMAIN}),
        )
        .await;
        assert_eq!(on["result"]["external"], true, "{on}");
        assert_eq!(on["result"]["url"], format!("https://{DOMAIN}/mcp"));
        self.wait("the tunnel", |h| h.tunnel == Some(TunnelState::Up))
            .await;
    }

    fn key(&self, name: &str) -> String {
        self.mac
            .create_key(KeyCreateParams { name: name.into() })
            .unwrap()
            .secret
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

/// An MCP client on the internet: HTTPS to the public endpoint, trusting
/// its certificate as it would Let's Encrypt's.
struct Client {
    addr: SocketAddr,
    cert: CertificateDer<'static>,
    key: Option<String>,
    next: u64,
}

fn tls(cert: &CertificateDer<'static>, alpn: &[u8]) -> tokio_rustls::TlsConnector {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.clone()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![alpn.to_vec()];
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

impl Client {
    async fn post(&self, path: &str, body: &str) -> Reply {
        let tcp = TcpStream::connect(self.addr).await.unwrap();
        let name = ServerName::try_from(DOMAIN).unwrap();
        let mut s = tls(&self.cert, b"http/1.1")
            .connect(name, tcp)
            .await
            .unwrap();
        let mut head = format!(
            "POST {path} HTTP/1.1\r\nHost: {DOMAIN}\r\nContent-Type: application/json\r\n\
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

fn limits() -> Limits {
    Limits {
        burst: 1000.0,
        per_second: 1000.0,
        ..Limits::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_url_and_one_key_reach_agents_on_the_host_and_the_mac() {
    let w = world(limits()).await;
    w.turn_on().await;
    let vps = w.vps();
    assert_eq!(vps.external_detail, None, "{vps:?}");
    // The host says so too, of itself.
    let own = w.host.hosts().unwrap().hosts.remove(0);
    assert!(own.external, "{own:?}");
    assert_eq!(own.tunnel, Some(TunnelState::Up));
    assert_eq!(own.url.as_deref(), Some("https://vps.test/mcp"));

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

    // Off: the tunnel closes, and the host stops serving.
    let off = rpc(
        &w.mac,
        "hosts.external",
        json!({"host": "vps", "on": false}),
    )
    .await;
    assert_eq!(off["result"]["external"], false, "{off}");
    assert_eq!(off["result"].get("tunnel"), None);
    for _ in 0..500 {
        if TcpStream::connect(w.public).await.is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(TcpStream::connect(w.public).await.is_err(), "still serving");
    // Nothing of the tunnel is left on the Mac: no socket, no lock.
    let socket = w.mac.tunnel_socket();
    assert!(!socket.exists(), "{} is left", socket.display());
    assert!(!vibe_core::socket::lock_path(&socket).exists());
    assert!(!w.host.hosts().unwrap().hosts[0].external);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_mac_checks_every_key_itself() {
    let w = world(limits()).await;
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
            let mut out = String::new();
            tokio::io::AsyncBufReadExt::read_line(&mut r, &mut out)
                .await
                .unwrap();
            serde_json::from_str::<Value>(&out).unwrap()
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
async fn the_public_endpoint_limits_each_client() {
    let w = world(Limits {
        burst: 5.0,
        per_second: 0.5,
        refused_keys: 3.0,
        refused_every: Duration::from_secs(600),
        ..Limits::default()
    })
    .await;
    w.turn_on().await;
    let key = w.key("laptop");
    w.pushed(&key).await;
    // Wrong keys, again and again: past three, the client is locked out,
    // even with a good key.
    let mut wrong = w.client(&vibe_core::keys::encode(&[9; 32]));
    for _ in 0..3 {
        assert_eq!(wrong.request("tools/list", json!({})).await.status, 401);
    }
    let r = wrong.request("tools/list", json!({})).await;
    assert_eq!(r.status, 429, "{r:?}");
    assert!(
        r.headers.to_ascii_lowercase().contains("retry-after:"),
        "{r:?}"
    );
    let mut good = w.client(&key);
    assert_eq!(good.request("tools/list", json!({})).await.status, 429);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_tunnel_says_why_it_is_down() {
    let w = world(limits()).await;
    w.wires.forwarding.store(false, Ordering::SeqCst);
    let on = rpc(
        &w.mac,
        "hosts.external",
        json!({"host": "vps", "on": true, "domain": "VPS.Test."}),
    )
    .await;
    assert_eq!(on["result"]["url"], "https://vps.test/mcp", "{on}");
    w.wait("the tunnel's trouble", |h| {
        h.tunnel == Some(TunnelState::Down)
            && h.external_detail
                .as_deref()
                .is_some_and(|d| d.contains("refused to forward"))
    })
    .await;
    // Fixed on the server: it comes up on its own.
    w.wires.forwarding.store(true, Ordering::SeqCst);
    w.wait("the tunnel", |h| h.tunnel == Some(TunnelState::Up))
        .await;
    assert_eq!(w.vps().external_detail, None);
    // A second tunnel replaced the first's end: the host cleared it.
    w.wires.pull();
    w.wait("the tunnel down", |h| h.tunnel == Some(TunnelState::Down))
        .await;
    w.wires.plug_in();
    w.wait("the tunnel back", |h| h.tunnel == Some(TunnelState::Up))
        .await;
    assert!(*w.wires.tunnels.lock().unwrap() >= 2);
    // Removing the host turns it off there, and the tunnel goes.
    let removed = rpc(&w.mac, "hosts.remove", json!({"host": "vps"})).await;
    assert!(removed.get("result").is_some(), "{removed}");
    for _ in 0..500 {
        if !w.host.hosts().unwrap().hosts[0].external {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!w.host.hosts().unwrap().hosts[0].external);
}

/// Reads one HTTP response off a kept-alive connection: its status, or
/// none once the server has closed it.
async fn one_response(s: &mut (impl AsyncReadExt + Unpin)) -> Option<u16> {
    let mut raw = Vec::new();
    let mut byte = [0u8; 1];
    while !raw.ends_with(b"\r\n\r\n") {
        match s.read(&mut byte).await {
            Ok(1) => raw.push(byte[0]),
            _ => return None,
        }
    }
    let head = String::from_utf8_lossy(&raw).to_ascii_lowercase();
    let status = head.split_whitespace().nth(1)?.parse().ok()?;
    if let Some(len) = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|n| n.trim().parse::<usize>().ok())
    {
        let mut body = vec![0u8; len];
        s.read_exact(&mut body).await.ok()?;
    } else if head.contains("transfer-encoding: chunked") {
        let mut tail = Vec::new();
        while !tail.ends_with(b"0\r\n\r\n") {
            match s.read(&mut byte).await {
                Ok(1) => tail.push(byte[0]),
                _ => return None,
            }
        }
    }
    Some(status)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turning_it_off_cuts_connections_already_open() {
    let w = world(limits()).await;
    w.turn_on().await;
    let key = w.key("laptop");
    w.pushed(&key).await;
    // A client with a key keeps its connection open between calls.
    let tcp = TcpStream::connect(w.public).await.unwrap();
    let mut s = tls(&w.cert, b"http/1.1")
        .connect(ServerName::try_from(DOMAIN).unwrap(), tcp)
        .await
        .unwrap();
    let request = |id: u32| {
        let body =
            json!({"jsonrpc": "2.0", "id": id, "method": "tools/list", "params": {}}).to_string();
        format!(
            "POST /mcp HTTP/1.1\r\nHost: {DOMAIN}\r\nContent-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\nMCP-Protocol-Version: 2025-06-18\r\n\
             Authorization: Bearer {key}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    };
    s.write_all(request(1).as_bytes()).await.unwrap();
    assert_eq!(one_response(&mut s).await, Some(200));
    s.write_all(request(2).as_bytes()).await.unwrap();
    assert_eq!(one_response(&mut s).await, Some(200), "kept alive");
    // Off: that connection ends too, not only the listener.
    let off = rpc(
        &w.mac,
        "hosts.external",
        json!({"host": "vps", "on": false}),
    )
    .await;
    assert_eq!(off["result"]["external"], false, "{off}");
    let _ = s.write_all(request(3).as_bytes()).await;
    let after = tokio::time::timeout(Duration::from_secs(10), one_response(&mut s))
        .await
        .expect("the connection should be closed, not left hanging");
    assert_eq!(after, None, "a call went through after it was turned off");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refusal_closes_the_connection_and_keeps_no_body() {
    let w = world(limits()).await;
    w.turn_on().await;
    let tcp = TcpStream::connect(w.public).await.unwrap();
    let mut s = tls(&w.cert, b"http/1.1")
        .connect(ServerName::try_from(DOMAIN).unwrap(), tcp)
        .await
        .unwrap();
    // A large body without a key: refused, and the connection closes.
    let body = format!("[{}{{}}]", "{\"a\":0},".repeat(300_000));
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: {DOMAIN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await.unwrap();
    s.write_all(body.as_bytes()).await.unwrap();
    assert_eq!(one_response(&mut s).await, Some(401));
    assert_eq!(one_response(&mut s).await, None, "closed after the refusal");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_challenge_handshake_gets_the_challenge_and_nothing_else() {
    let w = world(limits()).await;
    w.turn_on().await;
    // A client asking for acme-tls/1 isn't Let's Encrypt here (there's no
    // challenge pending), so it gets no certificate at all.
    let tcp = TcpStream::connect(w.public).await.unwrap();
    let name = ServerName::try_from(DOMAIN).unwrap();
    let r = tls(&w.cert, vibe_mcp::public::ACME_TLS_ALPN)
        .connect(name, tcp)
        .await;
    assert!(r.is_err(), "a handshake with no challenge pending");
    // Another name gets nothing either.
    let tcp = TcpStream::connect(w.public).await.unwrap();
    let r = tls(&w.cert, b"http/1.1")
        .connect(ServerName::try_from("other.test").unwrap(), tcp)
        .await;
    assert!(r.is_err());
}
