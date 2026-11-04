//! The whole path, with only the far ends stood in for: an MCP client on the
//! real shim, the real hub for a VM with the relay over a socket as over vsock,
//! or the local socket for an agent on the host itself, the real broker, and a
//! stand-in for the service. Nothing reaches a real service, and everything
//! that leaves the broker is searched for secrets.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use vibe_core::model::ServiceState;
use vibe_relay::hub::{Admit, Hub, Timing};
use vibe_relay::proxy;
use vibe_services::relay;
use vibe_services::secrets::{FileStore, MemoryStore};
use vibe_services::testing::{API_KEY, FakeService, SECRET_MARK};
use vibe_services::{Deps, Place, SecretStore, Services};

const RECONNECT: &str =
    "Linear needs reconnecting on the Mac (VibeOS → Settings → Services → Reconnect).";

fn shared<T: Default>() -> Arc<Mutex<T>> {
    Arc::default()
}

struct Mac {
    fake: FakeService,
    services: Arc<Services>,
    store: Arc<MemoryStore>,
    audit: Arc<Mutex<Vec<String>>>,
    state_file: PathBuf,
    dir: tempfile::TempDir,
}

impl Mac {
    async fn start() -> Mac {
        let fake = FakeService::start().await;
        let dir = tempfile::tempdir().unwrap();
        let state_file = dir.path().join("services.json");
        let audit: Arc<Mutex<Vec<String>>> = shared();
        let a = audit.clone();
        let now = Arc::new(AtomicI64::new(1_000_000));
        let count = Arc::new(AtomicUsize::new(1));
        let store = Arc::new(MemoryStore::default());
        let services = Services::new(
            vec![fake.service()],
            store.clone(),
            state_file.clone(),
            Deps {
                clock: Arc::new(move || now.load(Ordering::SeqCst)),
                random: Arc::new(move || Ok([count.fetch_add(1, Ordering::SeqCst) as u8; 32])),
                changed: Arc::new(|_| {}),
                audit: Arc::new(move |l| a.lock().unwrap().push(l.to_owned())),
                place: Place::Mac,
            },
        );
        Mac {
            fake,
            services,
            store,
            audit,
            state_file,
            dir,
        }
    }

    async fn sign_in(&self) {
        let auth = self.services.connect("linear").await.unwrap();
        let back = self.fake.sign_in(&auth.url).await;
        self.services.complete("linear", &back).await.unwrap();
    }

    fn secrets(&self) -> Vec<String> {
        let s = self.store.load("linear").unwrap().expect("a login");
        vec![s.access, s.refresh.unwrap()]
    }
}

/// A VM on the Mac: a hub and its local socket.
struct Machine {
    hub: Hub,
    socket: PathBuf,
    dir: tempfile::TempDir,
}

fn machine() -> Machine {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("services.sock");
    let admit: Admit = Box::new(|_, _| Ok(()));
    let hub = Hub::new(admit, Timing::default());
    let listener = UnixListener::bind(&socket).unwrap();
    let h = hub.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let h = h.clone();
            let Ok(conn) = conn else { return };
            std::thread::spawn(move || h.serve(conn));
        }
    });
    Machine { hub, socket, dir }
}

/// Copies bytes one way, keeping what crossed.
fn tee(mut from: UnixStream, mut to: UnixStream, kept: Arc<Mutex<Vec<u8>>>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            let n = match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            kept.lock().unwrap().extend_from_slice(&buf[..n]);
            if to.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        let _ = to.shutdown(std::net::Shutdown::Both);
    });
}

/// A relay the Mac opened to a machine, returned so the test can cut it.
/// Everything crossing it is kept in `wire`.
async fn relay_to(mac: &Mac, machine: &Machine, agent: &str, wire: &Arc<Mutex<Vec<u8>>>) -> Cut {
    // Mac <-> tee <-> machine.
    let (mac_end, tee_mac) = UnixStream::pair().unwrap();
    let (tee_machine, machine_end) = UnixStream::pair().unwrap();
    tee(
        tee_mac.try_clone().unwrap(),
        tee_machine.try_clone().unwrap(),
        wire.clone(),
    );
    tee(
        tee_machine.try_clone().unwrap(),
        tee_mac.try_clone().unwrap(),
        wire.clone(),
    );
    mac_end.set_nonblocking(true).unwrap();
    let mac_end = tokio::net::UnixStream::from_std(mac_end).unwrap();
    let (read, write) = mac_end.into_split();
    let services = mac.services.clone();
    let task = tokio::spawn(relay::serve_relay(services, agent.to_owned(), read, write));
    let hub = machine.hub.clone();
    let (reader, writer, closer) = (
        machine_end.try_clone().unwrap(),
        machine_end.try_clone().unwrap(),
        machine_end.try_clone().unwrap(),
    );
    std::thread::spawn(move || {
        let _ = hub.carry(reader, writer, move || {
            let _ = closer.shutdown(std::net::Shutdown::Both);
        });
    });
    for _ in 0..500 {
        if machine.hub.connected() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(machine.hub.connected());
    Cut {
        ends: vec![tee_mac, tee_machine, machine_end],
        task,
    }
}

struct Cut {
    ends: Vec<UnixStream>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Cut {
    fn cut(&self) {
        for e in &self.ends {
            let _ = e.shutdown(std::net::Shutdown::Both);
        }
        self.task.abort();
    }
}

/// An MCP client on a real shim, on stdio pipes, keeping what it reads.
struct Agent {
    to: UnixStream,
    from: BufReader<UnixStream>,
    stdout: Arc<Mutex<Vec<u8>>>,
    next: i64,
}

impl Agent {
    fn start(socket: &Path, cache: &Path, agent: Option<&str>) -> Agent {
        let (to, shim_in) = UnixStream::pair().unwrap();
        let (from, shim_out) = UnixStream::pair().unwrap();
        from.set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let config = proxy::Config {
            service: "linear".into(),
            socket: socket.to_owned(),
            agent: agent.map(str::to_owned),
            cache: Some(cache.to_owned()),
            version: "test".into(),
            timing: proxy::Timing {
                poll_connected: Duration::from_millis(200),
                poll_away: Duration::from_millis(50),
                first_list: Duration::from_secs(3),
            },
        };
        std::thread::spawn(move || {
            let _ = proxy::run(config, BufReader::new(shim_in), shim_out);
        });
        let mut agent = Agent {
            to,
            from: BufReader::new(from),
            stdout: shared(),
            next: 10,
        };
        agent.send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}}}));
        assert_eq!(agent.next_message()["id"], 1);
        agent.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        agent
    }

    fn send(&mut self, m: Value) {
        writeln!(self.to, "{m}").unwrap();
    }

    fn next_message(&mut self) -> Value {
        let mut line = String::new();
        self.from.read_line(&mut line).expect("the shim answers");
        self.stdout
            .lock()
            .unwrap()
            .extend_from_slice(line.as_bytes());
        serde_json::from_str(&line).expect("JSON")
    }

    fn ask(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let m = self.next_message();
            if m["id"] == json!(id) {
                return m;
            }
        }
    }

    fn call(&mut self, tool: &str) -> Value {
        self.ask("tools/call", json!({"name": tool, "arguments": {}}))
    }

    fn tools(&mut self) -> HashSet<String> {
        self.ask("tools/list", json!({}))["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect()
    }

    /// Lists until the live catalog has arrived.
    fn tools_once_known(&mut self) -> HashSet<String> {
        for _ in 0..200 {
            let tools = self.tools();
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

fn is_error(answer: &Value) -> bool {
    answer["result"]["isError"] == true
}

// The shim and hub block, as they do in their own processes; each test's
// blocking part runs off the runtime's threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}

/// Every file under `dir`, as bytes.
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

fn find(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_never_leaves_the_host_that_holds_it() {
    let mac = Mac::start().await;
    // Signing in, as the app does it from wherever it is: all that crosses to
    // it, or from it, is the page's address and where the page ended.
    let auth = mac.services.connect("linear").await.unwrap();
    let back = mac.fake.sign_in(&auth.url).await;
    mac.services.complete("linear", &back).await.unwrap();
    let mut crossed_in_signing_in = auth.url.clone().into_bytes();
    crossed_in_signing_in.extend_from_slice(back.as_bytes());

    let machine = machine();
    let wire: Arc<Mutex<Vec<u8>>> = shared();
    let cut = relay_to(&mac, &machine, "quiet-amber-otter", &wire).await;
    let (socket, cache) = (machine.socket.clone(), machine.dir.path().join("cache"));

    // A full working life, across a refresh, a rotation, a failure and a
    // reconnect, so every token there's been has had the chance to leak.
    let mut agent = blocking({
        let (socket, cache) = (socket.clone(), cache.clone());
        move || {
            let mut agent = Agent::start(&socket, &cache, Some("quiet-amber-otter"));
            assert!(agent.tools_once_known().contains("list_issues"));
            assert_eq!(text(&agent.call("list_issues")), "issues on /mcp");
            agent
        }
    })
    .await;
    let mut every_secret = mac.secrets();
    mac.fake.expire_access();
    agent = blocking(move || {
        assert_eq!(text(&agent.call("list_issues")), "issues on /mcp");
        agent
    })
    .await;
    every_secret.extend(mac.secrets());
    mac.fake.expire_access();
    mac.fake.refuse_refresh("invalid_grant");
    agent = blocking(move || {
        assert_eq!(text(&agent.call("list_issues")), RECONNECT);
        agent
    })
    .await;
    mac.fake.refuse_refresh("none");
    let auth = mac.services.connect("linear").await.unwrap();
    let back = mac.fake.sign_in(&auth.url).await;
    mac.services.complete("linear", &back).await.unwrap();
    crossed_in_signing_in.extend_from_slice(auth.url.as_bytes());
    crossed_in_signing_in.extend_from_slice(back.as_bytes());
    agent = blocking(move || {
        assert_eq!(text(&agent.call("stream_issues")), "streamed");
        agent
    })
    .await;
    every_secret.extend(mac.secrets());
    assert!(every_secret.len() >= 6);
    assert!(every_secret.iter().all(|s| s.contains(SECRET_MARK)));
    let verifiers = mac.fake.verifiers();
    assert_eq!(verifiers.len(), 2, "both sign-ins brought one");

    // What ever left the broker: relay frames, a shim's stdout, what landed on
    // a VM's disk, and what the app sees of a sign-in.
    let mut places: Vec<(String, Vec<u8>)> = vec![
        ("relay frames".to_owned(), wire.lock().unwrap().clone()),
        (
            "the shim's stdout".to_owned(),
            agent.stdout.lock().unwrap().clone(),
        ),
        (
            "what the app saw of signing in".to_owned(),
            crossed_in_signing_in,
        ),
    ];
    for (path, bytes) in all_files(machine.dir.path()) {
        places.push((path.display().to_string(), bytes));
    }
    // And what stays on the host beside its login: its state and its logs.
    places.push((
        "audit lines".to_owned(),
        mac.audit.lock().unwrap().join("\n").into_bytes(),
    ));
    for (path, bytes) in all_files(mac.dir.path()) {
        places.push((path.display().to_string(), bytes));
    }

    // The search must be able to fail: the wire did carry real traffic.
    let wire_bytes = wire.lock().unwrap().clone();
    assert!(
        find(&wire_bytes, "issues on /mcp"),
        "the search saw the traffic"
    );
    assert!(find(&wire_bytes, "list_issues"));
    assert!(std::fs::read(&mac.state_file).is_ok());

    for (place, bytes) in &places {
        assert!(!find(bytes, SECRET_MARK), "a secret is in {place}");
        for secret in every_secret.iter().chain(&verifiers) {
            assert!(!find(bytes, secret), "{secret} is in {place}");
        }
    }
    // And the service did get one on every call, which no client ever sent.
    assert!(
        mac.fake
            .calls()
            .iter()
            .any(|c| c.bearer.as_deref().is_some_and(|b| b.contains(SECRET_MARK)))
    );
    cut.cut();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_mac_going_away_and_back_needs_nothing_from_a_vms_agent() {
    let mac = Mac::start().await;
    mac.sign_in().await;
    let machine = machine();
    let wire: Arc<Mutex<Vec<u8>>> = shared();
    // The relay isn't up as the session starts: tools load, calls say so.
    let (socket, cache) = (machine.socket.clone(), machine.dir.path().join("cache"));
    let mut agent = blocking(move || {
        let mut agent = Agent::start(&socket, &cache, Some("a"));
        assert!(agent.tools().is_empty());
        let down = agent.call("list_issues");
        assert!(is_error(&down), "{down}");
        assert!(
            text(&down).contains("VibeOS shared service 'linear' unavailable: Mac unreachable"),
            "{down}"
        );
        agent
    })
    .await;

    // The Mac dials: the same session has its tools and calls work.
    let cut = relay_to(&mac, &machine, "a", &wire).await;
    agent = blocking(move || {
        assert!(agent.tools_once_known().contains("create_issue"));
        assert_eq!(text(&agent.call("list_issues")), "issues on /mcp");
        agent
    })
    .await;

    // It goes, as a snapshot makes it: tools still list, calls say so, and the
    // shim hasn't died.
    cut.cut();
    for _ in 0..500 {
        if !machine.hub.connected() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    agent = blocking(move || {
        assert!(agent.tools().contains("list_issues"));
        let down = agent.call("list_issues");
        assert!(is_error(&down), "{down}");
        assert!(text(&down).contains("Mac unreachable"), "{down}");
        agent
    })
    .await;

    // Back, on a new relay. Its upstream session is the same one, since the
    // broker owns it per agent.
    let before = mac.fake.calls().pop().unwrap().session;
    let cut = relay_to(&mac, &machine, "a", &wire).await;
    agent = blocking(move || {
        assert_eq!(text(&agent.call("list_issues")), "issues on /mcp");
        agent
    })
    .await;
    assert_eq!(mac.fake.calls().pop().unwrap().session, before);
    drop(agent);
    cut.cut();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_login_reads_clearly_and_reconnect_needs_no_agent_restart() {
    let mac = Mac::start().await;
    mac.sign_in().await;
    let machine = machine();
    let wire: Arc<Mutex<Vec<u8>>> = shared();
    let cut = relay_to(&mac, &machine, "a", &wire).await;
    let (socket, cache) = (machine.socket.clone(), machine.dir.path().join("cache"));
    let mut agent = blocking(move || {
        let mut agent = Agent::start(&socket, &cache, Some("a"));
        assert!(agent.tools_once_known().contains("list_issues"));
        agent
    })
    .await;

    mac.fake.expire_access();
    mac.fake.refuse_refresh("invalid_grant");
    agent = blocking(move || {
        let lost = agent.call("list_issues");
        assert!(is_error(&lost), "{lost}");
        assert_eq!(text(&lost), RECONNECT);
        assert!(agent.tools().contains("list_issues"), "tools still list");
        agent
    })
    .await;
    assert_eq!(
        mac.services.list().services[0].state,
        ServiceState::NeedsReconnect
    );

    mac.fake.refuse_refresh("none");
    mac.sign_in().await;
    blocking(move || {
        assert_eq!(text(&agent.call("list_issues")), "issues on /mcp");
    })
    .await;
    cut.cut();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_vms_calls_are_audited_as_its_agents_whatever_a_process_in_it_claims() {
    let mac = Mac::start().await;
    mac.sign_in().await;
    let wire: Arc<Mutex<Vec<u8>>> = shared();
    let vm = machine();
    let cut = relay_to(&mac, &vm, "brave-copper-lynx", &wire).await;
    let (socket, cache) = (vm.socket.clone(), vm.dir.path().join("cache"));
    blocking(move || {
        let mut agent = Agent::start(&socket, &cache, Some("someone-else"));
        assert_eq!(text(&agent.call("list_issues")), "issues on /mcp");
    })
    .await;
    cut.cut();

    let audit = mac.audit.lock().unwrap().join("\n");
    assert!(
        audit.contains("agent=brave-copper-lynx service=linear"),
        "{audit}"
    );
    assert!(!audit.contains("someone-else"), "{audit}");
}

/// A Linux host: its own broker, its own login in a private file, and agents
/// on its local socket. There's no Mac in this at all.
struct Host {
    fake: FakeService,
    services: Arc<Services>,
    socket: PathBuf,
    logins: PathBuf,
    audit: Arc<Mutex<Vec<String>>>,
    dir: tempfile::TempDir,
}

impl Host {
    async fn start(fake: &FakeService) -> Host {
        let dir = tempfile::tempdir().unwrap();
        let logins = dir.path().join("services");
        let audit: Arc<Mutex<Vec<String>>> = shared();
        let a = audit.clone();
        let count = Arc::new(AtomicUsize::new(100));
        let services = Services::new(
            vec![fake.service()],
            Arc::new(FileStore(logins.clone())),
            dir.path().join("services.json"),
            Deps {
                clock: Arc::new(|| 1_000_000),
                random: Arc::new(move || Ok([count.fetch_add(1, Ordering::SeqCst) as u8; 32])),
                changed: Arc::new(|_| {}),
                audit: Arc::new(move |l| a.lock().unwrap().push(l.to_owned())),
                place: Place::LinuxHost,
            },
        );
        let socket = dir.path().join("services.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let s = services.clone();
        tokio::spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                let admit: relay::Admit = Arc::new(|_, _| Ok(()));
                tokio::spawn(relay::serve_local(s.clone(), conn, admit));
            }
        });
        Host {
            fake: fake.clone(),
            services,
            socket,
            logins,
            audit,
            dir,
        }
    }

    /// Signing in as the app drives it: the page comes from here, and the code
    /// comes back here.
    async fn sign_in(&self) {
        let auth = self.services.connect("linear").await.unwrap();
        let back = self.fake.sign_in(&auth.url).await;
        self.services.complete("linear", &back).await.unwrap();
    }

    fn login(&self) -> vibe_services::Secret {
        FileStore(self.logins.clone())
            .load("linear")
            .unwrap()
            .expect("a login")
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_linux_host_serves_its_agents_on_its_own_login_and_survives_a_refresh() {
    let fake = FakeService::start().await;
    let host = Host::start(&fake).await;
    host.sign_in().await;

    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&host.logins), 0o700);
    assert_eq!(mode(&host.logins.join("linear.login")), 0o600);

    let (socket, cache) = (host.socket.clone(), host.dir.path().join("cache"));
    let mut agent = blocking(move || {
        let mut agent = Agent::start(&socket, &cache, Some("quiet-amber-otter"));
        assert!(agent.tools_once_known().contains("list_issues"));
        assert_eq!(text(&agent.call("list_issues")), "issues on /mcp");
        agent
    })
    .await;
    let before = host.login();

    // Its token runs out, and it refreshes by itself: nobody else is involved.
    fake.expire_access();
    agent = blocking(move || {
        assert_eq!(text(&agent.call("list_issues")), "issues on /mcp");
        agent
    })
    .await;
    let after = host.login();
    assert_ne!(
        after.refresh, before.refresh,
        "it rotated, and kept the new one"
    );
    assert_eq!(fake.refreshes(), 1);
    drop(agent);

    let audit = host.audit.lock().unwrap().join("\n");
    assert!(
        audit.contains("agent=quiet-amber-otter service=linear"),
        "{audit}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn each_host_holds_a_login_of_its_own_and_one_refreshing_cant_hurt_another() {
    // The very thing a synced token would break: rotation on one kills the
    // others. With grace off, a copy of a used refresh token would fail here.
    let fake = FakeService::start().await;
    fake.set_replay_grace(false);
    let a = Host::start(&fake).await;
    let b = Host::start(&fake).await;
    a.sign_in().await;
    b.sign_in().await;
    assert_eq!(
        fake.registrations(),
        2,
        "each registered a client of its own"
    );
    let (la, lb) = (a.login(), b.login());
    assert_ne!(la.access, lb.access);
    assert_ne!(la.refresh, lb.refresh);
    assert_ne!(la.client_id, lb.client_id);

    // Disconnecting one leaves the other.
    b.services.disconnect("linear").await.unwrap();
    assert_eq!(a.services.list().services[0].state, ServiceState::Connected);
    assert!(a.login().refresh.is_some());
    let (socket, cache) = (a.socket.clone(), a.dir.path().join("cache"));
    blocking(move || {
        let mut agent = Agent::start(&socket, &cache, Some("x"));
        assert_eq!(text(&agent.call("list_issues")), "issues on /mcp");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_personal_key_is_how_a_host_mends_itself_with_the_mac_away() {
    let fake = FakeService::start().await;
    let host = Host::start(&fake).await;
    host.sign_in().await;
    let (socket, cache) = (host.socket.clone(), host.dir.path().join("cache"));
    let mut agent = blocking(move || {
        let mut agent = Agent::start(&socket, &cache, Some("x"));
        assert!(agent.tools_once_known().contains("list_issues"));
        agent
    })
    .await;
    fake.expire_access();
    fake.refuse_refresh("invalid_grant");
    agent = blocking(move || {
        let lost = agent.call("list_issues");
        assert!(
            text(&lost).contains("needs reconnecting on this host"),
            "{lost}"
        );
        assert!(
            text(&lost).contains("vibe service set-key linear"),
            "{lost}"
        );
        agent
    })
    .await;

    host.services.connect_key("linear", API_KEY).await.unwrap();
    assert_eq!(
        host.services.list().services[0].state,
        ServiceState::Connected
    );
    blocking(move || {
        assert_eq!(text(&agent.call("list_issues")), "issues on /mcp");
    })
    .await;
    assert_eq!(fake.calls().pop().unwrap().bearer.as_deref(), Some(API_KEY));
    assert_eq!(host.login().refresh, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn on_a_host_a_shim_reaches_the_broker_with_no_relay_at_all() {
    let mac = Mac::start().await;
    mac.sign_in().await;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("services.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let services = mac.services.clone();
    let admit: relay::Admit = Arc::new(|_, agent| match agent {
        Some("off-agent") => Err("shared services are off for this agent's profile".into()),
        _ => Ok(()),
    });
    tokio::spawn(async move {
        loop {
            let Ok((conn, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(relay::serve_local(services.clone(), conn, admit.clone()));
        }
    });
    let cache = dir.path().join("cache");
    blocking(move || {
        let mut agent = Agent::start(&socket, &cache, Some("quiet-amber-otter"));
        assert!(agent.tools_once_known().contains("list_issues"));
        assert_eq!(text(&agent.call("list_issues")), "issues on /mcp");

        let mut off = Agent::start(&socket, &cache.with_file_name("off"), Some("off-agent"));
        let refused = off.call("list_issues");
        assert!(is_error(&refused));
        assert!(
            text(&refused).contains("off for this agent's profile"),
            "{refused}"
        );
    })
    .await;
}
