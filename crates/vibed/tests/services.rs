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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Golden, TestServer, serve, workspace_bin};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader as TokioBufReader};
use vibe_core::model::{LOCAL_HOST, ServiceRole, ServiceState};
use vibe_core::paths::Layout;
use vibe_core::rpc::{
    AgentStartParams, HostAddParams, MAX_MESSAGE_BYTES, ProfileUpdateParams, ProjectAddParams,
};
use vibe_env::testing::TestBackend;
use vibe_relay::proxy;
use vibe_services::secrets::{FileStore, MemoryStore};
use vibe_services::testing::{FakeService, SECRET_MARK};
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
    /// Its guest says it's too old for shared services.
    old_guest: bool,
    /// Asking its guest fails.
    check_fails: Arc<AtomicBool>,
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
    fn services_unsupported(&self, _: &vibe_env::Handle) -> vibe_env::Result<Option<String>> {
        if self.check_fails.load(Ordering::SeqCst) {
            return Err(vibe_env::EnvError::new("its guest didn't answer"));
        }
        Ok(self.old_guest.then(|| "its guest is too old".to_owned()))
    }
    fn services_relay(&self, _: &vibe_env::Handle) -> vibe_env::Result<Option<UnixStream>> {
        let Some(dir) = self.vm.as_ref().filter(|_| !self.old_guest) else {
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
    check_fails: Arc<AtomicBool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    layout: Layout,
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
    node_with(fake, place, store, vm, false, change).await
}

async fn node_with(
    fake: &FakeService,
    place: Place,
    store: Arc<dyn SecretStore>,
    vm: Option<PathBuf>,
    old_guest: bool,
    change: impl FnOnce(&mut Config),
) -> Node {
    let golden = Golden::new();
    let mut config = golden.config();
    let launches: Arc<Mutex<Vec<vibe_env::Launch>>> = Arc::default();
    let relays: Arc<Mutex<usize>> = Arc::default();
    let check_fails: Arc<AtomicBool> = Arc::default();
    config.backends = Backends::all(Arc::new(Seen {
        backend: TestBackend::new(&golden.claude),
        launches: launches.clone(),
        vm,
        relays: relays.clone(),
        old_guest,
        check_fails: check_fails.clone(),
    }));
    config.services = Some(ServicesConfig {
        services: vec![fake.service()],
        store,
        place,
        token_wait: Duration::from_millis(500),
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
    let layout = config.layout.clone();
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
        check_fails,
        tasks,
        layout,
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

/// Ends an agent's session, as Claude exiting does.
fn end_session(n: &Node, agent: &str) {
    let status = vibe_core::shim::connect(
        &n.layout.session_socket(agent),
        &vibe_core::shim::control_hello("test"),
        Duration::from_secs(10),
    )
    .unwrap()
    .status()
    .unwrap();
    vibe_env::tree::SessionTree::new(status.pid).end(Duration::from_secs(5));
}

#[tokio::test(flavor = "multi_thread")]
async fn switching_a_profile_off_takes_them_from_its_agents_next_launch() {
    let fake = FakeService::start().await;
    let n = node(
        &fake,
        Place::Mac,
        Arc::new(MemoryStore::default()),
        None,
        |_| {},
    )
    .await;
    let a = n
        .core
        .start_agent(start("on", "software-eng-linux"))
        .await
        .unwrap();
    assert!(offered(n.launches.lock().unwrap().last().unwrap()).is_some());
    let socket = n.core.services_socket();

    // Off, with the agent running: its next connection is refused at once...
    n.core
        .update_profile(ProfileUpdateParams {
            project: "app".into(),
            profile: "software-eng-linux".into(),
            shared_services: Some(false),
            ..Default::default()
        })
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (cache, agent) = (dir.path().to_owned(), a.agent.clone());
    let refused = {
        let socket = socket.clone();
        blocking(move || Shim::start(&socket, &cache, Some(&agent)).call("list_issues")).await
    };
    assert!(
        text(&refused).contains("off for this agent's profile"),
        "{refused}"
    );

    // ...and when it's next launched, by a resume, it isn't offered them.
    let before = n.launches.lock().unwrap().len();
    end_session(&n, &a.agent);
    n.core.resume_agent(None, &a.agent).await.unwrap();
    let launches = n.launches.lock().unwrap().clone();
    assert!(launches.len() > before, "it launched again");
    let last = launches.last().unwrap();
    assert!(last.args.iter().any(|x| x == "--resume"), "{:?}", last.args);
    assert_eq!(offered(last), None, "{:?}", last.args);

    // Back on, the same again.
    n.core
        .update_profile(ProfileUpdateParams {
            project: "app".into(),
            profile: "software-eng-linux".into(),
            shared_services: Some(true),
            ..Default::default()
        })
        .unwrap();
    let before = n.launches.lock().unwrap().len();
    end_session(&n, &a.agent);
    n.core.resume_agent(None, &a.agent).await.unwrap();
    let launches = n.launches.lock().unwrap().clone();
    assert!(launches.len() > before);
    assert!(offered(launches.last().unwrap()).is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_renamed_profile_goes_on_deciding_for_its_agents() {
    let fake = FakeService::start().await;
    let n = node(
        &fake,
        Place::Mac,
        Arc::new(MemoryStore::default()),
        None,
        |_| {},
    )
    .await;
    let a = n
        .core
        .start_agent(start("on", "software-eng-linux"))
        .await
        .unwrap();
    assert!(offered(n.launches.lock().unwrap().last().unwrap()).is_some());

    // Renamed and switched off at once: what the agent started with mustn't
    // decide for it, as it would if its profile only looked deleted.
    n.core
        .update_profile(ProfileUpdateParams {
            project: "app".into(),
            profile: "software-eng-linux".into(),
            name: Some("renamed".into()),
            shared_services: Some(false),
            ..Default::default()
        })
        .unwrap();
    end_session(&n, &a.agent);
    n.core.resume_agent(None, &a.agent).await.unwrap();
    assert_eq!(offered(n.launches.lock().unwrap().last().unwrap()), None);

    // And it goes by the new name from here on.
    n.core
        .update_profile(ProfileUpdateParams {
            project: "app".into(),
            profile: "renamed".into(),
            shared_services: Some(true),
            ..Default::default()
        })
        .unwrap();
    end_session(&n, &a.agent);
    n.core.resume_agent(None, &a.agent).await.unwrap();
    assert!(offered(n.launches.lock().unwrap().last().unwrap()).is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn failing_to_ask_the_guest_costs_the_services_and_not_the_launch() {
    let fake = FakeService::start().await;
    let n = node(
        &fake,
        Place::Mac,
        Arc::new(MemoryStore::default()),
        None,
        |_| {},
    )
    .await;
    n.check_fails.store(true, Ordering::SeqCst);
    n.core
        .start_agent(start("unsure", "software-eng-linux"))
        .await
        .expect("they're optional, so the agent still starts");
    let launch = n.launches.lock().unwrap().last().unwrap().clone();
    assert_eq!(offered(&launch), None, "{:?}", launch.args);
    assert_eq!(env_of(&launch, "VIBE_SERVICES_SOCKET"), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_older_guest_is_offered_nothing_and_gets_no_relay() {
    // As a template built on an older base goes on booting: it hasn't the port,
    // the socket or the program, so offering them would only break the session.
    let fake = FakeService::start().await;
    let guest = Guest::start();
    let n = node_with(
        &fake,
        Place::Mac,
        Arc::new(MemoryStore::default()),
        Some(guest.dir.path().to_owned()),
        true,
        |_| {},
    )
    .await;
    n.core
        .start_agent(start("old", "software-eng-linux"))
        .await
        .unwrap();
    let launch = n.launches.lock().unwrap().last().unwrap().clone();
    assert_eq!(offered(&launch), None, "{:?}", launch.args);
    assert_eq!(env_of(&launch, "VIBE_SERVICES_SOCKET"), None);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(*n.relays.lock().unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_runs_a_service_unasked_only_if_the_owner_opted_in() {
    let fake = FakeService::start().await;
    let n = node(
        &fake,
        Place::Mac,
        Arc::new(MemoryStore::default()),
        None,
        |_| {},
    )
    .await;
    n.core
        .start_agent(start("default", "software-eng-linux"))
        .await
        .unwrap();
    let launch = n.launches.lock().unwrap().last().unwrap().clone();
    assert!(!launch.args.iter().any(|a| a == "--allowedTools"));

    n.core
        .services_set(vibe_core::rpc::ServiceSetParams {
            service: "linear".into(),
            auto_allow: Some(true),
            ..Default::default()
        })
        .unwrap();
    n.core
        .start_agent(start("opted in", "software-eng-linux"))
        .await
        .unwrap();
    let launch = n.launches.lock().unwrap().last().unwrap().clone();
    let at = launch
        .args
        .iter()
        .position(|a| a == "--allowedTools")
        .unwrap();
    assert_eq!(launch.args[at + 1], "mcp__vibe-linear__*");
    // A flag follows at once, as it takes as many values as it can find.
    assert!(launch.args[at + 2].starts_with("--"), "{:?}", launch.args);
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
    n.sign_in_here().await;

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
    /// The network's back: the link dials again by itself.
    fn restore(&self) {
        self.down.store(false, std::sync::atomic::Ordering::SeqCst);
    }

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

impl Node {
    /// The app's Connect button, for a login on this node itself.
    async fn sign_in_here(&self) {
        let begun = rpc(&self.core, "services.connect", json!({"service": "linear"})).await;
        let url = begun["result"]["url"].as_str().expect("a page").to_owned();
        let back = self.fake.sign_in(&url).await;
        let done = rpc(
            &self.core,
            "services.complete",
            json!({"service": "linear", "callback_url": back}),
        )
        .await;
        assert_eq!(done["result"]["state"], "connected", "{done}");
    }

    fn service(&self) -> vibe_core::rpc::ServiceInfo {
        self.core
            .services_list(Default::default())
            .unwrap()
            .services
            .remove(0)
    }
}

/// A Mac and a Linux host, each with a broker, linked.
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

impl Pair {
    /// Until the Mac holds the token the host's login has now.
    async fn mac_following(&self) {
        wait_for("the Mac to be given the token", || {
            let owner = self.logins.load("linear").unwrap();
            let mac = self.mac_logins.load("linear").unwrap();
            matches!((owner, mac), (Some(o), Some(m)) if o.access == m.access)
        })
        .await;
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
async fn signing_in_on_the_mac_lands_at_the_owner_and_no_refresh_token_or_verifier_crosses() {
    let fake = FakeService::start().await;
    let p = pair(&fake).await;
    let mac = &p.mac.core;

    // A linked Linux host is the default owner, and nothing's signed in yet.
    let overview = rpc(mac, "services.overview", json!({})).await;
    let one = &overview["result"]["services"][0];
    assert_eq!(one["owner"], "vps", "{overview}");
    assert_eq!(one["signed_in"], false, "{overview}");
    assert_eq!(one["state"], "disconnected", "{overview}");

    let (url, back) = p.connect_host().await;

    // The host holds the login, in its own store. The Mac is given an access
    // token, and has nothing to refresh it with.
    let login = p.logins.load("linear").unwrap().expect("the host's login");
    assert!(login.access.contains(SECRET_MARK));
    assert!(login.refresh.is_some());
    assert_eq!(fake.registrations(), 1);
    p.mac_following().await;
    let given = p.mac_logins.load("linear").unwrap().unwrap();
    assert_eq!(given.refresh, None);
    assert_eq!(given.client_id, None);
    let host_list = rpc(mac, "services.list", json!({"host": "vps"})).await;
    assert_eq!(state_of(&host_list), "connected", "{host_list}");
    assert_eq!(host_list["result"]["services"][0]["role"], "owner");
    let mac_info = p.mac.service();
    assert_eq!(mac_info.state, ServiceState::Connected);
    assert_eq!(mac_info.role, ServiceRole::Follower);
    assert_eq!(mac_info.owner.as_deref(), Some("vps"));

    let overview = rpc(mac, "services.overview", json!({})).await;
    let one = &overview["result"]["services"][0];
    assert_eq!(one["owner"], "vps", "{overview}");
    assert_eq!(one["signed_in"], true, "{overview}");
    assert_eq!(one["state"], "connected", "{overview}");
    let hosts = one["hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 2);
    assert_eq!(hosts[0]["host"], LOCAL_HOST);
    assert_eq!(hosts[0]["service"]["role"], "follower");
    assert!(
        hosts[0]["service"]["access_until"].is_string(),
        "{overview}"
    );
    assert_eq!(hosts[1]["host"], "vps");
    assert_eq!(hosts[1]["reachable"], true);
    assert_eq!(hosts[1]["service"]["role"], "owner");

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
            if e.kind == vibe_core::model::EventKind::ServiceChanged && e.host == "vps" {
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
    p.mac_following().await;

    // What the owner may never let out, and everywhere it might have gone.
    let verifiers = fake.verifiers();
    assert_eq!(verifiers.len(), 1);
    let never: Vec<String> = [login.refresh.unwrap(), rotated.refresh.unwrap()]
        .into_iter()
        .chain(verifiers)
        .collect();
    let wire = p.tapped.wire.lock().unwrap().clone();
    // The search can fail: the wire did carry the sign-in, a code that the
    // verifier makes useless, and access tokens, as it now may.
    assert!(find(&wire, "services.complete"), "the tap saw the link");
    assert!(find(&wire, "fake-code-"), "the code crosses, as it may");
    assert!(
        find(&wire, &rotated.access),
        "access tokens cross, as they may"
    );
    let app_saw = format!("{url} {back}").into_bytes();
    let shim_said = shim.read.clone();
    for (place, bytes) in [
        ("the link", &wire),
        ("the app", &app_saw),
        ("the shim", &shim_said),
    ] {
        for secret in &never {
            assert!(!find(bytes, secret), "{secret} is in {place}");
        }
    }
    // An access token, too, goes only over the link: not to the app or an agent.
    for (place, bytes) in [("the app", &app_saw), ("the shim", &shim_said)] {
        assert!(!find(bytes, SECRET_MARK), "a secret is in {place}");
    }
    // And nothing in the clear in any file on either, logs included. The stores
    // here are a private file, encoded, and memory.
    let mut places: Vec<(String, Vec<u8>)> = Vec::new();
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
    }
    drop(shim);
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

    // And when its login goes, it says how it's mended: on the Mac, when it's back.
    fake.expire_access();
    fake.refuse_refresh("invalid_grant");
    let _shim = blocking(move || {
        let lost = shim.call("list_issues");
        assert_eq!(lost["result"]["isError"], true, "{lost}");
        assert_eq!(
            text(&lost),
            "Linear needs reconnecting on the Mac (VibeOS → Settings → Services → Reconnect)."
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
}

#[tokio::test(flavor = "multi_thread")]
async fn the_mac_as_a_follower_serves_its_agents_and_a_401_gets_it_a_new_token() {
    let fake = FakeService::start().await;
    let p = pair(&fake).await;
    p.connect_host().await;
    p.mac_following().await;

    let socket = p.mac.core.services_socket();
    let cache = p.mac.golden.home.join("cache");
    let mut shim = blocking(move || {
        let mut shim = Shim::start(&socket, &cache, Some("on-the-mac"));
        assert_eq!(text(&shim.call("list_issues")), "issues on /mcp");
        shim
    })
    .await;
    let first = p.logins.load("linear").unwrap().unwrap();
    assert_eq!(
        fake.calls().pop().unwrap().bearer.as_deref(),
        Some(first.access.as_str())
    );
    assert_eq!(fake.refreshes(), 0);

    // Turned down at the Mac: it asks once, the owner refreshes, and the same
    // call goes through with no one the wiser.
    fake.expire_access();
    shim = blocking(move || {
        let answer = shim.call("list_issues");
        assert_eq!(text(&answer), "issues on /mcp", "{answer}");
        shim
    })
    .await;
    assert_eq!(fake.refreshes(), 1, "at the owner");
    assert_ne!(
        p.logins.load("linear").unwrap().unwrap().refresh,
        first.refresh
    );
    p.mac_following().await;
    assert_eq!(p.mac_logins.load("linear").unwrap().unwrap().refresh, None);

    // With the owner away, the token it has serves until it's refused or runs
    // out, and then the agent's told why, and the tools still list.
    p.tapped.cut();
    wait_for("the link to go", || {
        !p.mac.core.link("vps").unwrap().connected()
    })
    .await;
    shim = blocking(move || {
        assert_eq!(text(&shim.call("list_issues")), "issues on /mcp");
        shim
    })
    .await;
    fake.expire_access();
    blocking(move || {
        let away = shim.call("list_issues");
        assert_eq!(away["result"]["isError"], true, "{away}");
        assert!(text(&away).contains("its login is on vps"), "{away}");
        assert!(shim.tools().contains(&"list_issues".to_owned()));
    })
    .await;
    assert_eq!(p.mac.service().state, ServiceState::OwnerUnreachable);
    assert_eq!(
        fake.refreshes(),
        1,
        "the Mac can't, and the owner wasn't asked"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn moving_the_login_is_signing_in_on_the_new_owner() {
    let fake = FakeService::start().await;
    let p = pair(&fake).await;

    // On the Mac first, so the host follows it.
    p.mac.sign_in_here().await;
    let on_mac = p.mac_logins.load("linear").unwrap().unwrap();
    let old_refresh = on_mac.refresh.clone().expect("the owner's");
    wait_for("the host to follow", || {
        p.logins
            .load("linear")
            .unwrap()
            .is_some_and(|l| l.access == on_mac.access)
    })
    .await;
    assert_eq!(p.logins.load("linear").unwrap().unwrap().refresh, None);
    assert_eq!(p.host.service().owner.as_deref(), Some("the Mac"));
    assert_eq!(p.mac.service().role, ServiceRole::Owner);

    // Moved to the host: there's still one login, and only it can refresh.
    p.connect_host().await;
    let on_host = p.logins.load("linear").unwrap().unwrap();
    assert!(on_host.refresh.is_some());
    p.mac_following().await;
    assert_eq!(p.mac_logins.load("linear").unwrap().unwrap().refresh, None);
    assert_eq!(p.mac.service().role, ServiceRole::Follower);
    assert_eq!(p.host.service().role, ServiceRole::Owner);
    assert_eq!(fake.revocations(), [old_refresh], "the old login's gone");
    let overview = rpc(&p.mac.core, "services.overview", json!({})).await;
    assert_eq!(overview["result"]["services"][0]["owner"], "vps");
}

#[tokio::test(flavor = "multi_thread")]
async fn disconnecting_forgets_it_everywhere_and_an_owner_away_is_told_when_its_back() {
    let fake = FakeService::start().await;
    let p = pair(&fake).await;
    p.connect_host().await;
    p.mac_following().await;
    let refresh = p.logins.load("linear").unwrap().unwrap().refresh.unwrap();

    // With the owner out of reach: the Mac forgets at once.
    p.tapped.cut();
    wait_for("the link to go", || {
        !p.mac.core.link("vps").unwrap().connected()
    })
    .await;
    let done = rpc(
        &p.mac.core,
        "services.disconnect",
        json!({"service": "linear"}),
    )
    .await;
    assert!(done["result"].is_object(), "{done}");
    wait_for("the Mac to forget", || {
        p.mac_logins.load("linear").unwrap().is_none()
    })
    .await;
    assert_eq!(p.mac.service().state, ServiceState::Disconnected);

    // It's back, still saying it holds it, and isn't believed: it forgets too,
    // there and at the service.
    p.tapped.restore();
    wait_for("the owner to forget", || {
        p.logins.load("linear").unwrap().is_none()
    })
    .await;
    wait_for("the service to be told", || {
        fake.revocations() == [refresh.clone()]
    })
    .await;
    assert_eq!(p.host.service().state, ServiceState::Disconnected);
    assert_eq!(p.mac.service().state, ServiceState::Disconnected);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mac_that_lost_track_finds_the_owner_and_needs_no_new_sign_in() {
    let fake = FakeService::start().await;
    let p = pair(&fake).await;
    // Signed in at the host without this Mac recording it, as a Mac reinstalled
    // or with its settings lost would find it.
    p.host.sign_in_here().await;
    p.mac_following().await;
    assert_eq!(p.mac.service().owner.as_deref(), Some("vps"));
    let overview = rpc(&p.mac.core, "services.overview", json!({})).await;
    assert_eq!(overview["result"]["services"][0]["signed_in"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_linux_host_gets_its_token_by_way_of_the_mac() {
    let fake = FakeService::start().await;
    let p = pair(&fake).await;
    let dir = tempfile::tempdir().unwrap();
    let logins = FileStore(dir.path().join("services"));
    let second = node(
        &fake,
        Place::LinuxHost,
        Arc::new(logins.clone()),
        None,
        |_| {},
    )
    .await;
    let server = serve(
        second.core.clone(),
        common::short_tempdir(),
        MAX_MESSAGE_BYTES,
    );
    p.tapped
        .sockets
        .lock()
        .unwrap()
        .insert("me@second".to_owned(), server.path.clone());
    p.mac
        .core
        .add_host(HostAddParams {
            name: "second".into(),
            address: "me@second".into(),
        })
        .await
        .unwrap();

    p.connect_host().await;
    let owner = p.logins.load("linear").unwrap().unwrap();
    wait_for("the second host to follow", || {
        logins
            .load("linear")
            .unwrap()
            .is_some_and(|l| l.access == owner.access)
    })
    .await;
    assert_eq!(logins.load("linear").unwrap().unwrap().refresh, None);
    assert_eq!(second.service().owner.as_deref(), Some("vps"));

    // The owner's refresh reaches it too, pushed through the Mac.
    let rotating = p.host.core.clone();
    fake.expire_access();
    let token = owner.access.clone();
    rotating
        .services_token(vibe_core::rpc::ServiceTokenParams {
            service: "linear".into(),
            rejected: Some(token),
        })
        .await
        .unwrap();
    let rotated = p.logins.load("linear").unwrap().unwrap();
    assert_ne!(rotated.access, owner.access);
    wait_for("the fresh token to reach it", || {
        logins
            .load("linear")
            .unwrap()
            .is_some_and(|l| l.access == rotated.access)
    })
    .await;

    // With the Mac gone there's no way between the two hosts: it goes on with
    // what it has, and only until that's no good. A limit, and a stated one.
    p.tapped.cut();
    let socket = second.core.services_socket();
    let cache = second.golden.home.join("cache");
    blocking(move || {
        let mut shim = Shim::start(&socket, &cache, Some("on-the-second-host"));
        assert_eq!(text(&shim.call("list_issues")), "issues on /mcp");
    })
    .await;
    assert_eq!(fake.refreshes(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_the_host_that_owns_it_leaves_it_to_be_signed_in_again() {
    let fake = FakeService::start().await;
    let p = pair(&fake).await;
    p.connect_host().await;
    p.mac_following().await;

    let refresh = p.logins.load("linear").unwrap().unwrap().refresh.unwrap();
    p.mac
        .core
        .remove_host(vibe_core::rpc::HostNameParams { host: "vps".into() })
        .await
        .unwrap();
    // It runs on by itself, and keeps nothing of this install's.
    assert_eq!(p.logins.load("linear").unwrap(), None);
    assert_eq!(fake.revocations(), [refresh]);
    wait_for("the Mac to forget", || {
        p.mac_logins.load("linear").unwrap().is_none()
    })
    .await;
    assert_eq!(p.mac.service().state, ServiceState::Disconnected);
    let overview = rpc(&p.mac.core, "services.overview", json!({})).await;
    let one = &overview["result"]["services"][0];
    assert_eq!(one["owner"], LOCAL_HOST, "{overview}");
    assert_eq!(one["signed_in"], false, "{overview}");

    // Signing in here is all it takes.
    p.mac.sign_in_here().await;
    assert_eq!(p.mac.service().role, ServiceRole::Owner);
}

#[tokio::test(flavor = "multi_thread")]
async fn keys_are_gone_and_tokens_only_go_between_vibeds() {
    let fake = FakeService::start().await;
    let n = node(
        &fake,
        Place::Mac,
        Arc::new(MemoryStore::default()),
        None,
        |_| {},
    )
    .await;
    let gone = rpc(
        &n.core,
        "services.connect_key",
        json!({"service": "linear", "key": "x"}),
    )
    .await;
    assert_eq!(gone["error"]["code"], -32601, "{gone}");
    // With no linked host, the login would be here.
    let overview = rpc(&n.core, "services.overview", json!({})).await;
    assert_eq!(overview["result"]["services"][0]["owner"], LOCAL_HOST);
    assert!(!vibe_core::rpc::TUNNEL_METHODS.contains(&"services.token"));
    assert!(!vibed::hosts::FORWARDED.contains(&"services.token"));
}
