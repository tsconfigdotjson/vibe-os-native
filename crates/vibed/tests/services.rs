//! Shared MCP services in the daemon: how agents are offered them, a Mac's
//! relays to its VMs through the real `vibe-guest`, and a Linux host's own
//! broker, its sign-in driven from a Mac over a tapped link. A stand-in plays
//! the service; nothing reaches a real one.

mod common;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Golden, TestServer, serve, workspace_bin};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader as TokioBufReader};
use vibe_core::model::{LOCAL_HOST, ServiceState};
use vibe_core::paths::Layout;
use vibe_core::rpc::{
    AgentStartParams, HostAddParams, MAX_MESSAGE_BYTES, ProfileUpdateParams, ProjectAddParams,
};
use vibe_env::testing::TestBackend;
use vibe_relay::proxy;
use vibe_services::secrets::{FileStore, MemoryStore};
use vibe_services::testing::{API_KEY, FakeService, SECRET_MARK};
use vibe_services::{Place, SecretStore};
use vibed::agents::Backends;
use vibed::core::{Config, Core};
use vibed::dispatch::{Session, handle_message};
use vibed::link::Timing;
use vibed::remote::{BoxFuture, Channel, Installed, Remote};
use vibed::services::ServicesConfig;

/// The test backend, keeping every launch it was asked for, and if `vm` names
/// a guest's directory, acting as a compartment with a machine of its own.
#[derive(Debug)]
struct Seen {
    backend: TestBackend,
    launches: Arc<Mutex<Vec<vibe_env::Launch>>>,
    vm: Option<PathBuf>,
    relays: Arc<Mutex<usize>>,
}

impl vibe_env::Compartment for Seen {
    fn create(&self, s: &vibe_env::AgentSpec) -> vibe_env::Result<vibe_env::Handle> {
        self.backend.create(s)
    }
    fn run(
        &self,
        h: &vibe_env::Handle,
        r: &vibe_env::Run,
    ) -> vibe_env::Result<vibe_env::RunOutput> {
        self.backend.run(h, r)
    }
    fn launch(
        &self,
        h: &vibe_env::Handle,
        l: &vibe_env::Launch,
    ) -> vibe_env::Result<vibe_env::TerminalEnd> {
        self.launches.lock().unwrap().push(l.clone());
        self.backend.launch(h, l)
    }
    fn processes(&self, h: &vibe_env::Handle) -> vibe_env::Result<Vec<vibe_env::Proc>> {
        self.backend.processes(h)
    }
    fn stop(&self, h: &vibe_env::Handle) -> vibe_env::Result<()> {
        self.backend.stop(h)
    }
    fn delete(&self, h: &vibe_env::Handle) -> vibe_env::Result<()> {
        self.backend.delete(h)
    }
    fn services_program(&self) -> Option<String> {
        self.vm
            .as_ref()
            .map(|_| vibe_relay::local::GUEST_PROGRAM.to_owned())
    }
    fn services_relay(&self, _: &vibe_env::Handle) -> vibe_env::Result<Option<UnixStream>> {
        let Some(dir) = &self.vm else {
            return Ok(None);
        };
        let port = dir.join(vibe_relay::frame::VSOCK_PORT.to_string());
        let stream = UnixStream::connect(port)
            .map_err(|e| vibe_env::EnvError::new(format!("can't dial: {e}")))?;
        *self.relays.lock().unwrap() += 1;
        Ok(Some(stream))
    }
}

impl vibe_env::Limiter for Seen {
    fn apply(&self, h: &vibe_env::Handle, l: &vibe_env::Limits) -> vibe_env::Result<()> {
        self.backend.apply(h, l)
    }
    fn usage(&self, h: &vibe_env::Handle) -> vibe_env::Result<vibe_env::Usage> {
        self.backend.usage(h)
    }
}

impl vibe_env::Sleeper for Seen {
    fn sleep(&self, h: &vibe_env::Handle) -> vibe_env::Result<()> {
        self.backend.sleep(h)
    }
    fn wake(&self, h: &vibe_env::Handle) -> vibe_env::Result<vibe_env::Woke> {
        self.backend.wake(h)
    }
}

impl vibe_env::Backend for Seen {
    fn capabilities(&self) -> vibe_env::Capabilities {
        self.backend.capabilities()
    }
}

/// A host with a broker of its own, as `vibed`'s `main` makes one, on a
/// stand-in, with `app` added.
struct Node {
    core: Arc<Core>,
    golden: Golden,
    fake: FakeService,
    launches: Arc<Mutex<Vec<vibe_env::Launch>>>,
    relays: Arc<Mutex<usize>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Node {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

async fn node(
    fake: &FakeService,
    place: Place,
    store: Arc<dyn SecretStore>,
    vm: Option<PathBuf>,
    change: impl FnOnce(&mut Config),
) -> Node {
    let golden = Golden::new();
    let mut config = golden.config();
    let launches: Arc<Mutex<Vec<vibe_env::Launch>>> = Arc::default();
    let relays: Arc<Mutex<usize>> = Arc::default();
    config.backends = Backends::all(Arc::new(Seen {
        backend: TestBackend::new(&golden.claude),
        launches: launches.clone(),
        vm,
        relays: relays.clone(),
    }));
    config.services = Some(ServicesConfig {
        services: vec![fake.service()],
        store,
        place,
    });
    config.vibe_cli = "/opt/test/vibe".into();
    // Real randomness: two hosts mustn't share a verifier or a state.
    config.entropy = Arc::new(vibe_core::keys::random_bytes);
    if place == Place::LinuxHost {
        config.info.os = "linux".into();
        config.info.version = env!("CARGO_PKG_VERSION").into();
        config.label = "vps-1".into();
        config.layout = Layout::new(&golden.home, "linux");
        config.toolbox = None;
    }
    change(&mut config);
    let core = Arc::new(Core::open(config).unwrap());
    core.add_project(ProjectAddParams {
        path: Some(golden.home.join("src/app").to_str().unwrap().into()),
        ..Default::default()
    })
    .await
    .unwrap();
    let tasks = core.spawn_services();
    Node {
        core,
        golden,
        fake: fake.clone(),
        launches,
        relays,
        tasks,
    }
}

fn start(prompt: &str, profile: &str) -> AgentStartParams {
    AgentStartParams {
        project: "app".into(),
        profile: profile.into(),
        prompt: prompt.into(),
        ..Default::default()
    }
}

/// One JSON-RPC call through the daemon's dispatcher, as a socket's client.
async fn rpc(core: &Core, method: &str, params: Value) -> Value {
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    let answer = handle_message(core, &mut Session::default(), line.as_bytes())
        .await
        .expect("an answer");
    serde_json::from_str(&answer).unwrap()
}

async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..1500 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{what} never happened");
}

fn offered(launch: &vibe_env::Launch) -> Option<Value> {
    let at = launch.args.iter().position(|a| a == "--mcp-config")?;
    Some(serde_json::from_str(&launch.args[at + 1]).unwrap())
}

fn env_of<'a>(launch: &'a vibe_env::Launch, name: &str) -> Option<&'a str> {
    launch
        .env
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

// ---- Offering.

#[tokio::test(flavor = "multi_thread")]
async fn every_profile_offers_the_services_per_launch_until_it_is_switched_off() {
    let fake = FakeService::start().await;
    let n = node(
        &fake,
        Place::Mac,
        Arc::new(MemoryStore::default()),
        None,
        |_| {},
    )
    .await;

    // On by default, with nothing asked for.
    let on = n
        .core
        .start_agent(start("on", "software-eng-linux"))
        .await
        .unwrap();
    let launch = n.launches.lock().unwrap().last().unwrap().clone();
    assert_eq!(
        offered(&launch),
        Some(json!({"mcpServers": {"vibe-linear": {
            "type": "stdio",
            "command": "/opt/test/vibe",
            "args": ["mcp-proxy", "linear"],
        }}}))
    );
    assert_eq!(env_of(&launch, "VIBE_AGENT"), Some(on.agent.as_str()));
    let socket = n.core.services_socket();
    assert_eq!(env_of(&launch, "VIBE_SERVICES_SOCKET"), socket.to_str());
    // Before the prompt's `--`, and with the owner's own servers left alone.
    let at = launch
        .args
        .iter()
        .position(|a| a == "--mcp-config")
        .unwrap();
    let dashes = launch.args.iter().position(|a| a == "--").unwrap();
    assert!(at < dashes, "{:?}", launch.args);
    assert!(!launch.args.iter().any(|a| a == "--strict-mcp-config"));
    assert!(!launch.args.iter().any(|a| a.contains("allowedTools")));

    // Nothing was written for it: not into its worktree, nor the owner's config.
    let worktree = launch.cwd.clone();
    assert!(!worktree.join(".mcp.json").exists());
    assert!(
        !n.golden.home.join(".claude.json").exists()
            || !std::fs::read_to_string(n.golden.home.join(".claude.json"))
                .unwrap()
                .contains("vibe-linear")
    );

    // Off: nothing's offered, nor where to find it.
    n.core
        .update_profile(ProfileUpdateParams {
            project: "app".into(),
            profile: "software-eng-linux".into(),
            shared_services: Some(false),
            ..Default::default()
        })
        .unwrap();
    let off = n
        .core
        .start_agent(start("off", "software-eng-linux"))
        .await
        .unwrap();
    let launch = n.launches.lock().unwrap().last().unwrap().clone();
    assert_eq!(offered(&launch), None, "{:?}", launch.args);
    assert_eq!(env_of(&launch, "VIBE_SERVICES_SOCKET"), None);
    assert_eq!(env_of(&launch, "VIBE_AGENT"), Some(off.agent.as_str()));

    // And a shim that's in an off agent all the same is told no.
    let dir = tempfile::tempdir().unwrap();
    let (cache, agent) = (dir.path().to_owned(), off.agent.clone());
    let refused =
        blocking(move || Shim::start(&socket, &cache, Some(&agent)).call("list_issues")).await;
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    assert!(
        text(&refused).contains("off for this agent's profile"),
        "{refused}"
    );
}

// ---- Shims.

/// An MCP client on a real shim, on its stdin and stdout.
struct Shim {
    to: Box<dyn Write + Send>,
    from: std::sync::mpsc::Receiver<String>,
    next: i64,
    read: Vec<u8>,
}

impl Shim {
    /// A shim in this process, as `vibe mcp-proxy` is.
    fn start(socket: &Path, cache: &Path, agent: Option<&str>) -> Shim {
        let (to, shim_in) = UnixStream::pair().unwrap();
        let (from, shim_out) = UnixStream::pair().unwrap();
        let config = proxy::Config {
            service: "linear".into(),
            socket: socket.to_owned(),
            agent: agent.map(str::to_owned),
            cache: Some(cache.to_owned()),
            version: "test".into(),
            timing: proxy::Timing {
                poll_connected: Duration::from_millis(200),
                poll_away: Duration::from_millis(100),
                first_list: Duration::from_secs(3),
            },
        };
        std::thread::spawn(move || {
            let _ = proxy::run(config, BufReader::new(shim_in), shim_out);
        });
        Shim::over(Box::new(to), from)
    }

    /// One over whatever it writes to and reads from.
    fn over(to: Box<dyn Write + Send>, from: impl std::io::Read + Send + 'static) -> Shim {
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(from).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        let mut shim = Shim {
            to,
            from: lines,
            next: 10,
            read: Vec::new(),
        };
        let init = shim.ask(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}}),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "vibe-linear");
        writeln!(
            shim.to,
            r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
        )
        .unwrap();
        shim
    }

    fn ask(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        let m = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.to, "{m}").unwrap();
        loop {
            let line = self
                .from
                .recv_timeout(Duration::from_secs(30))
                .expect("the shim answers");
            self.read.extend_from_slice(line.as_bytes());
            self.read.push(b'\n');
            let v: Value = serde_json::from_str(&line).expect("JSON");
            if v["id"] == json!(id) {
                return v;
            }
        }
    }

    fn call(&mut self, tool: &str) -> Value {
        self.ask("tools/call", json!({"name": tool, "arguments": {}}))
    }

    /// Lists until it's seen tools, or says so.
    fn tools(&mut self) -> Vec<String> {
        for _ in 0..300 {
            let v = self.ask("tools/list", json!({}));
            let tools: Vec<String> = v["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_owned())
                .collect();
            if !tools.is_empty() {
                return tools;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("no tools came");
    }
}

fn text(answer: &Value) -> String {
    answer["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}

// ---- A Mac's VMs.

/// The real guest, on Unix sockets standing in for its vsock ports.
struct Guest {
    dir: tempfile::TempDir,
    child: Child,
}

impl Guest {
    fn start() -> Guest {
        let dir = tempfile::tempdir().unwrap();
        let child = Command::new(workspace_bin("vibe-guest"))
            .arg("--vsock-dir")
            .arg(dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // Dropped, it ends the child, however this goes.
        let guest = Guest { dir, child };
        let port = guest
            .dir
            .path()
            .join(vibe_relay::frame::VSOCK_PORT.to_string());
        for _ in 0..500 {
            if port.exists() && guest.dir.path().join("services.sock").exists() {
                return guest;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the guest never served its ports");
    }

    /// `vibe mcp-proxy linear`, as an agent in the VM runs it: the guest's own
    /// binary, a real process.
    fn shim(&self, agent: &str) -> (Shim, Child) {
        let mut child = Command::new(workspace_bin("vibe-guest"))
            .args(["mcp-proxy", "linear"])
            .env(
                vibe_relay::local::SOCKET_ENV,
                self.dir.path().join("services.sock"),
            )
            .env(
                vibe_relay::local::CACHE_ENV,
                self.dir.path().join(format!("cache-{agent}")),
            )
            .env(vibe_relay::local::AGENT_ENV, agent)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        (Shim::over(Box::new(stdin), stdout), child)
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_vm_reaches_the_macs_broker_over_the_relay_the_mac_dials() {
    let fake = FakeService::start().await;
    let guest = Guest::start();
    let n = node(
        &fake,
        Place::Mac,
        Arc::new(MemoryStore::default()),
        Some(guest.dir.path().to_owned()),
        |_| {},
    )
    .await;
    n.core
        .services_connect_key(vibe_core::rpc::ServiceKeyParams {
            service: "linear".into(),
            key: API_KEY.into(),
        })
        .await
        .unwrap();

    // Offered as the guest's own program, and with no host socket to find.
    n.core
        .start_agent(start("in a vm", "software-eng-linux"))
        .await
        .unwrap();
    let launch = n.launches.lock().unwrap().last().unwrap().clone();
    assert_eq!(
        offered(&launch).unwrap()["mcpServers"]["vibe-linear"]["command"],
        vibe_relay::local::GUEST_PROGRAM
    );
    assert_eq!(env_of(&launch, "VIBE_SERVICES_SOCKET"), None);

    // The Mac dials, by itself.
    let relays = n.relays.clone();
    wait_for("the Mac to dial the VM", || *relays.lock().unwrap() >= 1).await;

    // A process in the VM claims to be someone else: it's still this agent.
    let (mut shim, mut child) = guest.shim("someone-else");
    let answer = blocking(move || {
        assert!(shim.tools().contains(&"list_issues".to_owned()));
        shim.call("list_issues")
    })
    .await;
    assert_eq!(text(&answer), "issues on /mcp", "{answer}");
    let _ = child.kill();
    let _ = child.wait();
    assert!(!answer.to_string().contains(SECRET_MARK));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_with_the_services_off_gets_no_relay() {
    let fake = FakeService::start().await;
    let guest = Guest::start();
    let n = node(
        &fake,
        Place::Mac,
        Arc::new(MemoryStore::default()),
        Some(guest.dir.path().to_owned()),
        |_| {},
    )
    .await;
    n.core
        .update_profile(ProfileUpdateParams {
            project: "app".into(),
            profile: "software-eng-linux".into(),
            shared_services: Some(false),
            ..Default::default()
        })
        .unwrap();
    n.core
        .start_agent(start("off", "software-eng-linux"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(*n.relays.lock().unwrap(), 0);
    // So what's in that VM has nothing to reach.
    let (mut shim, mut child) = guest.shim("x");
    let answer = blocking(move || shim.call("list_issues")).await;
    assert_eq!(answer["result"]["isError"], true);
    assert!(text(&answer).contains("Mac unreachable"), "{answer}");
    let _ = child.kill();
    let _ = child.wait();
}

// ---- A Linux host's own login, driven from the Mac.

/// `ssh`, in process, keeping everything that crosses the link either way.
#[derive(Debug, Default)]
struct Tapped {
    sockets: Mutex<HashMap<String, PathBuf>>,
    wire: Arc<Mutex<Vec<u8>>>,
    down: std::sync::atomic::AtomicBool,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Tapped {
    fn cut(&self) {
        self.down.store(true, std::sync::atomic::Ordering::SeqCst);
        for t in self.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
    }
}

impl Remote for Tapped {
    fn dial<'a>(&'a self, address: &'a str) -> BoxFuture<'a, Result<Channel, String>> {
        Box::pin(async move {
            if self.down.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("ssh: Network is unreachable".to_owned());
            }
            let socket = self
                .sockets
                .lock()
                .unwrap()
                .get(address)
                .cloned()
                .ok_or("no such host")?;
            let stream = tokio::net::UnixStream::connect(&socket)
                .await
                .map_err(|e| e.to_string())?;
            let (ours, theirs) = tokio::io::duplex(1 << 20);
            let wire = self.wire.clone();
            let task = tokio::spawn(async move {
                let (mut sr, mut sw) = stream.into_split();
                let (mut tr, mut tw) = tokio::io::split(theirs);
                let (mut a, mut b) = (vec![0u8; 1 << 16], vec![0u8; 1 << 16]);
                loop {
                    tokio::select! {
                        n = sr.read(&mut a) => match n {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                wire.lock().unwrap().extend_from_slice(&a[..n]);
                                if tw.write_all(&a[..n]).await.is_err() { return }
                            }
                        },
                        n = tr.read(&mut b) => match n {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                wire.lock().unwrap().extend_from_slice(&b[..n]);
                                if sw.write_all(&b[..n]).await.is_err() { return }
                            }
                        },
                    }
                }
            });
            self.tasks.lock().unwrap().push(task);
            let (r, w) = tokio::io::split(ours);
            Ok(Channel::new(
                TokioBufReader::new(r),
                w,
                (),
                Arc::new(Mutex::new(String::new())),
            ))
        })
    }

    fn install<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<Installed, String>> {
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
}

/// A Mac and a Linux host, each with a broker and a login of its own, linked.
struct Pair {
    mac: Node,
    host: Node,
    logins: FileStore,
    mac_logins: Arc<MemoryStore>,
    tapped: Arc<Tapped>,
    _server: TestServer,
    _dir: tempfile::TempDir,
}

async fn pair(fake: &FakeService) -> Pair {
    let dir = tempfile::tempdir().unwrap();
    let logins = FileStore(dir.path().join("services"));
    let host = node(
        fake,
        Place::LinuxHost,
        Arc::new(logins.clone()),
        None,
        |_| {},
    )
    .await;
    let server = serve(
        host.core.clone(),
        common::short_tempdir(),
        MAX_MESSAGE_BYTES,
    );
    let tapped = Arc::new(Tapped::default());
    tapped
        .sockets
        .lock()
        .unwrap()
        .insert("me@vps".to_owned(), server.path.clone());
    let mac_logins = Arc::new(MemoryStore::default());
    let t = tapped.clone();
    let mac = node(fake, Place::Mac, mac_logins.clone(), None, move |config| {
        config.remote = t;
        config.link_timing = Timing {
            ping_every: Duration::from_millis(200),
            ping_timeout: Duration::from_millis(400),
            connect_timeout: Duration::from_secs(5),
            min_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_millis(300),
            ..Timing::default()
        };
    })
    .await;
    mac.core.spawn_links();
    mac.core
        .add_host(HostAddParams {
            name: "vps".into(),
            address: "me@vps".into(),
        })
        .await
        .unwrap();
    for _ in 0..500 {
        if rpc(&mac.core, "services.list", json!({"host": "vps"})).await["result"].is_object() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Pair {
        mac,
        host,
        logins,
        mac_logins,
        tapped,
        _server: server,
        _dir: dir,
    }
}

impl Pair {
    /// The app's Connect button for the host: it opens the page the host gave,
    /// and hands back where it ended. Says what the app saw.
    async fn connect_host(&self) -> (String, String) {
        let begun = rpc(
            &self.mac.core,
            "services.connect",
            json!({"host": "vps", "service": "linear"}),
        )
        .await;
        let url = begun["result"]["url"].as_str().expect("a page").to_owned();
        let back = self.mac.fake.sign_in(&url).await;
        let done = rpc(
            &self.mac.core,
            "services.complete",
            json!({"host": "vps", "service": "linear", "callback_url": back}),
        )
        .await;
        assert_eq!(done["result"]["state"], "connected", "{done}");
        (url, back)
    }
}

fn state_of(list: &Value) -> &str {
    list["result"]["services"][0]["state"]
        .as_str()
        .unwrap_or("")
}

fn find(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// Every file under `dir`.
fn all_files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            out.extend(all_files(&path));
        } else if let Ok(bytes) = std::fs::read(&path) {
            out.push((path, bytes));
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_signs_in_for_itself_and_no_token_or_verifier_crosses_the_link() {
    let fake = FakeService::start().await;
    let p = pair(&fake).await;
    let mac = &p.mac.core;

    // Each is on its own: the Mac's own says nothing of the host's.
    let host_list = rpc(mac, "services.list", json!({"host": "vps"})).await;
    assert_eq!(state_of(&host_list), "disconnected", "{host_list}");

    let (url, back) = p.connect_host().await;

    // The host holds it: in its own store, and nothing on the Mac.
    let login = p.logins.load("linear").unwrap().expect("the host's login");
    assert!(login.access.contains(SECRET_MARK));
    assert_eq!(p.mac_logins.load("linear").unwrap(), None);
    let host_list = rpc(mac, "services.list", json!({"host": "vps"})).await;
    assert_eq!(state_of(&host_list), "connected", "{host_list}");
    let mac_list = rpc(mac, "services.list", json!({"host": LOCAL_HOST})).await;
    assert_eq!(state_of(&mac_list), "disconnected", "{mac_list}");

    // A host's change reaches the Mac's event stream under its name.
    let (_, mut events) = mac.subscribe();
    let on_host = p.host.core.services_set(vibe_core::rpc::ServiceSetParams {
        service: "linear".into(),
        read_only: Some(true),
        ..Default::default()
    });
    assert!(on_host.unwrap().read_only);
    let event = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let e = events.recv().await.unwrap();
            if e.kind == vibe_core::model::EventKind::ServiceChanged {
                return e;
            }
        }
    })
    .await
    .expect("the Mac hears of it");
    assert_eq!(event.host, "vps");
    assert_eq!(event.message.as_deref(), Some("linear"));

    // An agent on the host uses it, through that host's broker alone.
    let socket = p.host.core.services_socket();
    let cache = p.host.golden.home.join("cache");
    let mut shim = blocking(move || {
        let mut shim = Shim::start(&socket, &cache, Some("quiet-amber-otter"));
        assert!(shim.tools().contains(&"list_issues".to_owned()));
        assert_eq!(text(&shim.call("list_issues")), "issues on /mcp/readonly");
        shim
    })
    .await;
    fake.expire_access();
    shim = blocking(move || {
        assert_eq!(text(&shim.call("list_issues")), "issues on /mcp/readonly");
        shim
    })
    .await;
    assert_eq!(fake.refreshes(), 1);
    let rotated = p.logins.load("linear").unwrap().unwrap();
    assert_ne!(rotated.refresh, login.refresh);

    // What the host may never let out, and everywhere it might have gone.
    let verifiers = fake.verifiers();
    assert_eq!(verifiers.len(), 1);
    let secrets: Vec<String> = [
        login.access,
        login.refresh.unwrap(),
        rotated.access,
        rotated.refresh.unwrap(),
    ]
    .into_iter()
    .chain(verifiers)
    .collect();
    let wire = p.tapped.wire.lock().unwrap().clone();
    // The search can fail: the wire did carry the sign-in, and only a code that
    // the verifier makes useless.
    assert!(find(&wire, "services.complete"), "the tap saw the link");
    assert!(find(&wire, "fake-code-"), "the code crosses, as it may");
    let mut places = vec![
        ("the link".to_owned(), wire),
        (
            "what the app saw".to_owned(),
            format!("{url} {back}").into_bytes(),
        ),
        ("the shim's stdout".to_owned(), shim.read.clone()),
    ];
    places.extend(
        all_files(p.mac.golden.dir.path())
            .into_iter()
            .map(|(f, b)| (format!("the Mac's {}", f.display()), b)),
    );
    places.extend(
        all_files(p.host.golden.dir.path())
            .into_iter()
            .map(|(f, b)| (format!("the host's {}", f.display()), b)),
    );
    assert!(places.len() > 3);
    for (place, bytes) in &places {
        assert!(!find(bytes, SECRET_MARK), "a secret is in {place}");
        for secret in &secrets {
            assert!(!find(bytes, secret), "{secret} is in {place}");
        }
    }
    drop(shim);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_personal_key_never_crosses_a_link() {
    let fake = FakeService::start().await;
    let p = pair(&fake).await;
    let refused = rpc(
        &p.mac.core,
        "services.connect_key",
        json!({"host": "vps", "service": "linear", "key": API_KEY}),
    )
    .await;
    assert_eq!(refused["error"]["code"], -32602, "{refused}");
    assert!(!find(&p.tapped.wire.lock().unwrap(), API_KEY));
    assert_eq!(p.logins.load("linear").unwrap(), None);
    assert_eq!(p.mac_logins.load("linear").unwrap(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn with_the_mac_gone_a_hosts_agent_lists_calls_and_survives_a_refresh() {
    let fake = FakeService::start().await;
    let p = pair(&fake).await;
    p.connect_host().await;
    let kept = p.logins.load("linear").unwrap().unwrap();

    // The network goes, and the Mac's vibed with it.
    p.tapped.cut();
    let list = rpc(&p.mac.core, "services.list", json!({"host": "vps"})).await;
    assert_eq!(list["error"]["code"], -32005, "{list}");
    let Pair {
        mac,
        host,
        logins,
        _server,
        ..
    } = p;
    drop(mac);

    let socket = host.core.services_socket();
    let cache = host.golden.home.join("cache");
    let mut shim = blocking(move || {
        let mut shim = Shim::start(&socket, &cache, Some("quiet-amber-otter"));
        assert!(shim.tools().contains(&"create_issue".to_owned()));
        assert_eq!(text(&shim.call("list_issues")), "issues on /mcp");
        shim
    })
    .await;

    // Its token runs out, and it refreshes by itself, and keeps what it's given.
    fake.expire_access();
    shim = blocking(move || {
        let answer = shim.call("list_issues");
        assert_eq!(text(&answer), "issues on /mcp", "{answer}");
        shim
    })
    .await;
    assert_eq!(fake.refreshes(), 1);
    assert_ne!(
        logins.load("linear").unwrap().unwrap().refresh,
        kept.refresh
    );

    // And when its login goes, it says how to mend it here, with no Mac.
    fake.expire_access();
    fake.refuse_refresh("invalid_grant");
    shim = blocking(move || {
        let lost = shim.call("list_issues");
        assert_eq!(lost["result"]["isError"], true, "{lost}");
        assert!(
            text(&lost).contains("needs reconnecting on this host"),
            "{lost}"
        );
        assert!(
            text(&lost).contains("vibe service set-key linear"),
            "{lost}"
        );
        assert!(
            shim.tools().contains(&"list_issues".to_owned()),
            "tools still list"
        );
        shim
    })
    .await;
    assert_eq!(
        host.core
            .services_list(Default::default())
            .unwrap()
            .services[0]
            .state,
        ServiceState::NeedsReconnect
    );

    host.core
        .services_connect_key(vibe_core::rpc::ServiceKeyParams {
            service: "linear".into(),
            key: API_KEY.into(),
        })
        .await
        .unwrap();
    blocking(move || {
        assert_eq!(text(&shim.call("list_issues")), "issues on /mcp");
    })
    .await;
    assert_eq!(fake.calls().pop().unwrap().bearer.as_deref(), Some(API_KEY));
}
