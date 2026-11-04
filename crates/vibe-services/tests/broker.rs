//! The broker against a stand-in for a service and its OAuth server: the
//! lifecycle, rotation, sessions, policy and audit. Nothing here reaches a real
//! service.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use vibe_core::model::{ServiceRole, ServiceState};
use vibe_core::rpc::{ServiceFollowParams, ServiceInfo, ServiceSetParams};
use vibe_services::secrets::MemoryStore;
use vibe_services::testing::{FakeService, SECRET_MARK};
use vibe_services::{Deps, Place, Secret, SecretStore, Services};

/// The stand-in's tools.
const TOOLS: usize = 7;

const RECONNECT: &str =
    "Linear needs reconnecting on the Mac (VibeOS → Settings → Services → Reconnect).";

struct World {
    fake: FakeService,
    services: Arc<Services>,
    store: Arc<dyn SecretStore>,
    audit: Arc<Mutex<Vec<String>>>,
    changes: Arc<Mutex<Vec<ServiceInfo>>>,
    now: Arc<AtomicI64>,
    state_file: PathBuf,
    _dir: tempfile::TempDir,
}

fn deps(
    now: &Arc<AtomicI64>,
    audit: &Arc<Mutex<Vec<String>>>,
    changes: &Arc<Mutex<Vec<ServiceInfo>>>,
) -> Deps {
    let count = Arc::new(AtomicUsize::new(1));
    let (now, audit, changes) = (now.clone(), audit.clone(), changes.clone());
    Deps {
        clock: Arc::new(move || now.load(Ordering::SeqCst)),
        random: Arc::new(move || {
            let n = count.fetch_add(1, Ordering::SeqCst) as u8;
            Ok([n; 32])
        }),
        changed: Arc::new(move |info| changes.lock().unwrap().push(info.clone())),
        audit: Arc::new(move |line| audit.lock().unwrap().push(line.to_owned())),
        place: Place::Mac,
        token_wait: Duration::from_millis(300),
    }
}

impl World {
    async fn start(store: Arc<dyn SecretStore>) -> World {
        let fake = FakeService::start().await;
        let dir = tempfile::tempdir().unwrap();
        let state_file = dir.path().join("services.json");
        let now = Arc::new(AtomicI64::new(1_000_000));
        let audit = Arc::default();
        let changes = Arc::default();
        let services = Services::new(
            vec![fake.service()],
            store.clone(),
            state_file.clone(),
            deps(&now, &audit, &changes),
        );
        World {
            fake,
            services,
            store,
            audit,
            changes,
            now,
            state_file,
            _dir: dir,
        }
    }

    async fn new() -> World {
        World::start(Arc::new(MemoryStore::default())).await
    }

    /// The broker as a restarted daemon has it, on the same store and state.
    fn restarted(&self) -> Arc<Services> {
        Services::new(
            vec![self.fake.service()],
            self.store.clone(),
            self.state_file.clone(),
            deps(&self.now, &self.audit, &self.changes),
        )
    }

    async fn sign_in(&self) -> ServiceInfo {
        let auth = self.services.connect("linear").await.unwrap();
        assert_eq!(auth.callback_scheme, "vibeos");
        let back = self.fake.sign_in(&auth.url).await;
        let info = self.services.complete("linear", &back).await.unwrap();
        // Its tools are read in the background: wait, so that's not going on
        // beside what a test does next.
        for _ in 0..500 {
            if self.info().tools > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        info
    }

    fn info(&self) -> ServiceInfo {
        self.services.list().services.remove(0)
    }

    fn secret(&self) -> Secret {
        self.store.load("linear").unwrap().expect("a login")
    }

    fn agent(&self, name: &str) -> Shim {
        Shim::open(&self.services, name)
    }
}

/// A shim's conversation with the broker.
struct Shim {
    tx: mpsc::Sender<Vec<u8>>,
    rx: mpsc::Receiver<Vec<u8>>,
    next: i64,
}

impl Shim {
    fn open(services: &Arc<Services>, agent: &str) -> Shim {
        let (out, rx) = mpsc::channel(64);
        let tx = services.open("linear", agent.to_owned(), out).unwrap();
        Shim { tx, rx, next: 100 }
    }

    async fn send(&self, m: Value) {
        self.tx.send(m.to_string().into_bytes()).await.unwrap();
    }

    async fn recv(&mut self) -> Value {
        let bytes = tokio::time::timeout(Duration::from_secs(20), self.rx.recv())
            .await
            .expect("an answer in time")
            .expect("the stream is open");
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn ask(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        loop {
            let m = self.recv().await;
            if m["id"] == json!(id) {
                return m;
            }
        }
    }

    async fn call(&mut self, tool: &str) -> Value {
        self.ask(
            "tools/call",
            json!({"name": tool, "arguments": {"token": "an-argument"}}),
        )
        .await
    }

    async fn tools(&mut self) -> Vec<String> {
        let list = self.ask("tools/list", json!({})).await;
        list["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect()
    }
}

fn text(answer: &Value) -> &str {
    answer["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
}

fn is_error(answer: &Value) -> bool {
    answer["result"]["isError"] == true
}

#[tokio::test(flavor = "multi_thread")]
async fn signing_in_keeps_the_login_in_the_store_and_only_there() {
    let w = World::new().await;
    assert_eq!(w.info().state, ServiceState::Disconnected);
    let info = w.sign_in().await;
    assert_eq!(info.state, ServiceState::Connected);
    assert_eq!(info.role, ServiceRole::Owner);
    assert_eq!(w.fake.registrations(), 1);
    let secret = w.secret();
    assert!(secret.access.contains(SECRET_MARK));
    assert!(secret.refresh.as_deref().unwrap().contains(SECRET_MARK));
    assert_eq!(secret.expires_at, Some(1_000_000 + 86_400));
    assert_eq!(secret.client_id.as_deref(), Some("fake-client-1"));
    assert_eq!(secret.scopes, ["read", "write"]);
    // And not in what's told to the app, or kept on disk beside it.
    assert!(!serde_json::to_string(&info).unwrap().contains(SECRET_MARK));
    let kept = std::fs::read_to_string(&w.state_file).unwrap();
    assert!(!kept.contains(SECRET_MARK), "{kept}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sign_in_must_be_the_one_under_way_and_from_who_it_should_be() {
    let w = World::new().await;
    let auth = w.services.connect("linear").await.unwrap();
    let back = w.fake.sign_in(&auth.url).await;

    let not_ours = w
        .services
        .complete("linear", "https://evil.example/?code=1")
        .await;
    assert!(
        not_ours
            .unwrap_err()
            .message
            .contains("where a sign-in ends")
    );

    // Each failure ends the sign-in: start again.
    let forged = back.replace("state=", "state=x");
    let err = w.services.complete("linear", &forged).await.unwrap_err();
    assert!(err.message.contains("isn't the one under way"), "{err:?}");
    let err = w.services.complete("linear", &back).await.unwrap_err();
    assert!(err.message.contains("No sign-in"), "{err:?}");

    let auth = w.services.connect("linear").await.unwrap();
    let back = w.fake.sign_in(&auth.url).await;
    let wrong_iss = format!(
        "{}&iss=https%3A%2F%2Fevil.example",
        back.split("&iss=").next().unwrap()
    );
    let err = w.services.complete("linear", &wrong_iss).await.unwrap_err();
    assert!(err.message.contains("evil.example"), "{err:?}");

    let auth = w.services.connect("linear").await.unwrap();
    let back = w.fake.sign_in(&auth.url).await;
    let no_iss = back.split("&iss=").next().unwrap().to_owned();
    let err = w.services.complete("linear", &no_iss).await.unwrap_err();
    assert!(err.message.contains("who it came from"), "{err:?}");

    // The owner saying no.
    let auth = w.services.connect("linear").await.unwrap();
    let state = auth
        .url
        .split("state=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();
    let denied = format!(
        "vibeos://oauth/callback?error=access_denied&error_description=you+said+no&state={state}&iss={}",
        w.fake.base.replace(':', "%3A").replace('/', "%2F")
    );
    let err = w.services.complete("linear", &denied).await.unwrap_err();
    assert!(err.message.contains("you said no"), "{err:?}");

    assert_eq!(w.info().state, ServiceState::Disconnected);
    assert_eq!(w.store.load("linear").unwrap(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_broker_adds_the_bearer_and_owns_the_session() {
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("quiet-amber-otter");
    let answer = a.call("list_issues").await;
    assert_eq!(text(&answer), "issues on /mcp", "{answer}");

    // Its session was opened as MCP says, for this agent or the catalog.
    let seen = w.fake.seen();
    let opened = seen.iter().position(|s| s.method == "initialize").unwrap();
    assert!(
        seen[opened + 1..]
            .iter()
            .any(|s| s.method == "notifications/initialized")
    );
    let call = w.fake.calls().pop().unwrap();
    assert_eq!(call.bearer.as_deref(), Some(w.secret().access.as_str()));
    assert!(call.session.is_some());

    // Another call on it keeps the session; another agent gets its own.
    a.call("list_issues").await;
    let mut b = w.agent("brave-copper-lynx");
    b.call("list_issues").await;
    let calls = w.fake.calls();
    let n = calls.len();
    assert_eq!(calls[n - 3].session, calls[n - 2].session);
    assert_ne!(calls[n - 2].session, calls[n - 1].session);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_event_stream_comes_through_as_it_comes() {
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("a");
    w.fake.hold_streams();
    a.send(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "stream_issues"}}))
        .await;
    // Its progress arrives while the stream is held open: nothing's buffered.
    let progress = a.recv().await;
    assert_eq!(progress["method"], "notifications/progress", "{progress}");
    w.fake.release();
    let done = a.recv().await;
    assert_eq!(done["id"], 1);
    assert_eq!(text(&done), "streamed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_servers_own_request_reaches_the_client_and_comes_back() {
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("a");
    a.send(
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "ask_user"}}),
    )
    .await;
    let asked = a.recv().await;
    assert_eq!(asked["method"], "elicitation/create", "{asked}");
    a.send(json!({"jsonrpc": "2.0", "id": asked["id"], "result": {"action": "accept"}}))
        .await;
    let done = a.recv().await;
    assert_eq!(done["id"], 1);
    assert_eq!(text(&done), "told {\"action\":\"accept\"}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_401_refreshes_rotates_and_retries_unseen_by_the_agent() {
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("a");
    a.call("list_issues").await;
    let before = w.secret();
    w.fake.expire_access();
    let answer = a.call("list_issues").await;
    assert_eq!(text(&answer), "issues on /mcp", "{answer}");
    assert_eq!(w.fake.refreshes(), 1);
    let after = w.secret();
    assert_ne!(before.access, after.access);
    assert_ne!(before.refresh, after.refresh, "the refresh token rotated");
    assert_eq!(w.info().state, ServiceState::Connected);
    // The app saw it refreshing, then connected.
    let states: Vec<_> = w.changes.lock().unwrap().iter().map(|c| c.state).collect();
    assert!(states.contains(&ServiceState::Refreshing), "{states:?}");
    assert_eq!(states.last(), Some(&ServiceState::Connected));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_about_to_run_out_is_refreshed_first() {
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("a");
    a.call("list_issues").await;
    let first = w.secret().access;
    w.now.fetch_add(86_400 - 30, Ordering::SeqCst);
    a.call("list_issues").await;
    assert_eq!(w.fake.refreshes(), 1);
    let call = w.fake.calls().pop().unwrap();
    assert_ne!(call.bearer.as_deref(), Some(first.as_str()));
    // No call went out on the old token after that, and no 401 was needed.
    assert!(w.fake.seen().iter().all(|s| s.status != 401));
}

#[tokio::test(flavor = "multi_thread")]
async fn only_one_refresh_runs_however_many_calls_want_one() {
    let w = World::new().await;
    w.sign_in().await;
    let mut shims = Vec::new();
    for i in 0..8 {
        let mut s = w.agent(&format!("agent-{i}"));
        s.call("list_issues").await;
        shims.push(s);
    }
    w.fake.expire_access();
    w.fake.slow_refreshes(Duration::from_millis(100));
    let mut tasks = Vec::new();
    for mut s in shims {
        tasks.push(tokio::spawn(async move { s.call("list_issues").await }));
    }
    for t in tasks {
        let answer = t.await.unwrap();
        assert_eq!(text(&answer), "issues on /mcp", "{answer}");
    }
    assert_eq!(w.fake.refreshes(), 1, "the rest used what it made");
}

/// A store that can fail, and says when it wrote.
#[derive(Debug)]
struct Recording {
    inner: MemoryStore,
    fake: Mutex<Option<FakeService>>,
    fail: AtomicBool,
    delay: Mutex<Option<Duration>>,
}

impl Recording {
    fn new() -> Arc<Recording> {
        Arc::new(Recording {
            inner: MemoryStore::default(),
            fake: Mutex::new(None),
            fail: AtomicBool::new(false),
            delay: Mutex::new(None),
        })
    }
}

impl SecretStore for Recording {
    fn load(&self, service: &str) -> io::Result<Option<Secret>> {
        self.inner.load(service)
    }

    fn store(&self, service: &str, secret: &Secret) -> io::Result<()> {
        if let Some(delay) = *self.delay.lock().unwrap() {
            std::thread::sleep(delay);
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err(io::Error::other("the Keychain is locked"));
        }
        self.inner.store(service, secret)?;
        if let Some(fake) = &*self.fake.lock().unwrap() {
            fake.log(format!("stored {}", secret.access));
        }
        Ok(())
    }

    fn delete(&self, service: &str) -> io::Result<()> {
        self.inner.delete(service)
    }
}

fn position(log: &[String], line: &str) -> usize {
    log.iter()
        .position(|l| l == line)
        .unwrap_or_else(|| panic!("no `{line}` in {log:#?}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rotated_token_is_persisted_before_it_is_used() {
    let store = Recording::new();
    let w = World::start(store.clone()).await;
    *store.fake.lock().unwrap() = Some(w.fake.clone());
    // A slow Keychain would show a use that doesn't wait for it.
    *store.delay.lock().unwrap() = Some(Duration::from_millis(150));
    w.sign_in().await;
    let mut a = w.agent("a");
    w.fake.expire_access();
    a.call("list_issues").await;
    let new = w.secret().access;
    let log = w.fake.log_lines();
    let gave = position(&log, &format!("refresh gave {new}"));
    let stored = position(&log, &format!("stored {new}"));
    let used = position(&log, &format!("used {new}"));
    assert!(gave < stored && stored < used, "{log:#?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crash_before_the_write_strands_nothing() {
    // Crash injected at the worst moment: the service has rotated, and the
    // store never heard. The new token is never used, the old refresh token is
    // what's kept, and it's taken again within the replay grace.
    let store = Recording::new();
    let w = World::start(store.clone()).await;
    w.sign_in().await;
    let mut a = w.agent("a");
    // The catalog's read, so nothing else is after a refresh just now.
    assert_eq!(a.tools().await.len(), TOOLS);
    let kept = w.secret();
    w.fake.expire_access();
    store.fail.store(true, Ordering::SeqCst);
    let answer = a.call("list_issues").await;
    assert!(is_error(&answer), "{answer}");
    assert!(
        text(&answer).contains("can't keep the refreshed login"),
        "{answer}"
    );
    assert_eq!(w.fake.refreshes(), 1);
    // The token the service gave was never used, for anything.
    let log = w.fake.log_lines();
    let given = log
        .iter()
        .find_map(|l| l.strip_prefix("refresh gave "))
        .expect("the service rotated")
        .to_owned();
    assert!(
        w.fake
            .seen()
            .iter()
            .all(|s| s.bearer.as_deref() != Some(given.as_str())),
        "{log:#?}"
    );
    assert_eq!(w.secret(), kept, "what's kept is untouched");
    assert_eq!(w.info().state, ServiceState::Connected);

    // The daemon comes back, with the Keychain open.
    store.fail.store(false, Ordering::SeqCst);
    let services = w.restarted();
    let mut a = Shim::open(&services, "a");
    let answer = a.call("list_issues").await;
    assert_eq!(text(&answer), "issues on /mcp", "{answer}");
    assert_ne!(w.secret().refresh, kept.refresh);
    assert!(
        w.fake
            .log_lines()
            .iter()
            .any(|l| l.starts_with("replay gave"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rotated_login_that_cant_be_written_is_held_and_only_the_write_tried_again() {
    let store = Recording::new();
    let w = World::start(store.clone()).await;
    w.sign_in().await;
    let mut a = w.agent("a");
    assert_eq!(a.tools().await.len(), TOOLS);
    let kept = w.secret();
    // With no grace for the old token: a second refresh would kill the login.
    w.fake.set_replay_grace(false);
    w.fake.expire_access();
    store.fail.store(true, Ordering::SeqCst);
    let answer = a.call("list_issues").await;
    assert!(
        text(&answer).contains("can't keep the refreshed login"),
        "{answer}"
    );
    assert_eq!(w.fake.refreshes(), 1);

    // However long the disk goes on failing, and whatever asks: no more refreshes.
    let life = kept.expires_at.unwrap() - kept.obtained_at.unwrap();
    w.now.fetch_add(life / 2 + 1, Ordering::SeqCst);
    for _ in 0..3 {
        w.services.refresh_due().await;
        let answer = a.call("list_issues").await;
        assert!(is_error(&answer), "{answer}");
        assert_eq!(w.fake.refreshes(), 1, "only the write is tried again");
        assert_eq!(w.secret(), kept);
    }
    assert!(
        w.info()
            .detail
            .unwrap()
            .contains("can't keep the refreshed login")
    );

    // It's writable again: the next pass keeps it, and that's the login from then on.
    store.fail.store(false, Ordering::SeqCst);
    w.services.refresh_due().await;
    assert_eq!(w.fake.refreshes(), 1);
    assert_ne!(w.secret().refresh, kept.refresh);
    assert_eq!(w.info().detail, None);
    assert_eq!(w.info().state, ServiceState::Connected);
    let answer = a.call("list_issues").await;
    assert_eq!(text(&answer), "issues on /mcp", "{answer}");
    assert_eq!(w.fake.refreshes(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rotated_login_not_yet_written_is_what_a_disconnect_revokes() {
    let store = Recording::new();
    let w = World::start(store.clone()).await;
    w.sign_in().await;
    let mut a = w.agent("a");
    assert_eq!(a.tools().await.len(), TOOLS);
    w.fake.expire_access();
    store.fail.store(true, Ordering::SeqCst);
    a.call("list_issues").await;
    let given = w
        .fake
        .log_lines()
        .iter()
        .find_map(|l| l.strip_prefix("refresh gave ").map(str::to_owned))
        .expect("the service rotated");
    store.fail.store(false, Ordering::SeqCst);
    w.services.disconnect("linear").await.unwrap();
    assert!(w.services.list().services[0].state == ServiceState::Disconnected);
    let rotated_refresh = given.replace("access", "refresh");
    assert_eq!(
        w.fake.revocations(),
        [rotated_refresh],
        "the one the service now knows"
    );
    // Nothing is left to write: it can't come back from here.
    w.services.refresh_due().await;
    assert!(store.load("linear").unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn signing_in_again_on_the_same_host_revokes_the_old_grant() {
    let w = World::new().await;
    w.sign_in().await;
    let old = w.secret();
    w.sign_in().await;
    let new = w.secret();
    assert_ne!(new.refresh, old.refresh);
    assert_eq!(w.fake.revocations(), [old.refresh.unwrap()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_followers_refusals_cant_make_the_owner_refresh_as_fast_as_it_can_ask() {
    let w = World::new().await;
    w.sign_in().await;
    let first = w
        .services
        .token("linear", None)
        .await
        .unwrap()
        .access
        .unwrap();

    // The first refusal of the current token refreshes.
    let second = w
        .services
        .token("linear", Some(&first))
        .await
        .unwrap()
        .access
        .unwrap();
    assert_ne!(second, first);
    assert_eq!(w.fake.refreshes(), 1);

    // Another right after gets what's current, with nothing used up.
    let again = w
        .services
        .token("linear", Some(&second))
        .await
        .unwrap()
        .access
        .unwrap();
    assert_eq!(again, second);
    assert_eq!(w.fake.refreshes(), 1);

    // After a while it may again.
    w.now.fetch_add(61, Ordering::SeqCst);
    let third = w
        .services
        .token("linear", Some(&second))
        .await
        .unwrap()
        .access
        .unwrap();
    assert_ne!(third, second);
    assert_eq!(w.fake.refreshes(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follower_whose_owner_has_no_login_says_to_connect_at_once() {
    let w = World::new().await;
    let f = w.follower();
    f.services
        .follow(ServiceFollowParams {
            service: "linear".into(),
            owner: Some("vps-1".into()),
            state: ServiceState::Disconnected,
            access: None,
            expires_at: None,
            detail: None,
        })
        .await
        .unwrap();
    assert!(!f.info().wants_token);
    let mut a = Shim::open(&f.services, "a");
    let answer = a.call("list_issues").await;
    assert!(is_error(&answer), "{answer}");
    let said = text(&answer);
    assert!(said.contains("isn't connected on the Mac"), "{said}");
    assert!(!said.contains("once it's back"), "{said}");
    assert!(!f.info().wants_token, "there's nothing to ask for");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crash_after_the_write_strands_nothing() {
    // Crash just after it's kept, before it's used: a new daemon reads the new
    // tokens, and they go on working and rotating.
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("a");
    w.fake.expire_access();
    a.call("list_issues").await;
    let rotated = w.secret();
    drop(a);

    let services = w.restarted();
    let mut a = Shim::open(&services, "a");
    w.fake.set_replay_grace(false);
    assert_eq!(text(&a.call("list_issues").await), "issues on /mcp");
    w.fake.expire_access();
    let answer = a.call("list_issues").await;
    assert_eq!(text(&answer), "issues on /mcp", "{answer}");
    assert_ne!(w.secret().refresh, rotated.refresh);
    assert_eq!(w.fake.refreshes(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_grant_needs_reconnecting_and_reconnect_needs_no_agent_restart() {
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("a");
    assert_eq!(a.tools().await.len(), TOOLS);
    w.fake.expire_access();
    w.fake.refuse_refresh("invalid_grant");

    let answer = a.call("list_issues").await;
    assert!(is_error(&answer), "{answer}");
    assert_eq!(text(&answer), RECONNECT);
    let info = w.info();
    assert_eq!(info.state, ServiceState::NeedsReconnect);
    assert!(
        info.detail.as_deref().unwrap().contains("invalid_grant"),
        "{info:?}"
    );
    assert_eq!(info.detail_at.as_deref(), Some("1970-01-12T13:46:40Z"));

    // Tools still list, and it doesn't keep bothering the service.
    assert_eq!(a.tools().await.len(), TOOLS);
    let refreshes = w.fake.refreshes();
    assert_eq!(text(&a.call("list_issues").await), RECONNECT);
    assert_eq!(w.fake.refreshes(), refreshes);

    // Reconnect. The same conversation just works: no /mcp in the agent.
    w.fake.set_replay_grace(true);
    w.fake.refuse_refresh("none");
    let info = w.sign_in().await;
    assert_eq!(info.state, ServiceState::Connected);
    assert_eq!(info.detail, None);
    assert_eq!(info.detail_at, None);
    let answer = a.call("list_issues").await;
    assert_eq!(text(&answer), "issues on /mcp", "{answer}");
    // Reconnecting starts afresh, as the client may be what was wrong.
    assert_eq!(w.fake.registrations(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_the_service_has_forgotten_is_registered_anew() {
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("a");
    w.fake.expire_access();
    w.fake.forget_clients();
    assert_eq!(text(&a.call("list_issues").await), RECONNECT);
    let info = w.info();
    assert_eq!(info.state, ServiceState::NeedsReconnect);
    assert!(
        info.detail.as_deref().unwrap().contains("invalid_client"),
        "{info:?}"
    );

    let info = w.sign_in().await;
    assert_eq!(info.state, ServiceState::Connected);
    assert_eq!(w.fake.registrations(), 2);
    assert_eq!(text(&a.call("list_issues").await), "issues on /mcp");
    assert_eq!(
        w.secret()
            .client_id
            .as_deref()
            .map(|c| c != "fake-client-1"),
        Some(true)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_turned_down_after_a_fresh_refresh_means_revoked() {
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("a");
    assert_eq!(a.tools().await.len(), TOOLS);
    w.fake.reject_all_tokens(true);
    let answer = a.call("list_issues").await;
    assert!(is_error(&answer), "{answer}");
    assert_eq!(text(&answer), RECONNECT);
    assert_eq!(w.fake.refreshes(), 1, "it tried a fresh one first");
    let info = w.info();
    assert_eq!(info.state, ServiceState::Revoked);
    assert!(
        info.detail.as_deref().unwrap().contains("revoked"),
        "{info:?}"
    );
    assert_eq!(a.tools().await.len(), TOOLS, "tools still list");

    w.fake.reject_all_tokens(false);
    assert_eq!(w.sign_in().await.state, ServiceState::Connected);
    assert_eq!(text(&a.call("list_issues").await), "issues on /mcp");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_the_service_forgot_is_made_again_unseen() {
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("a");
    a.call("list_issues").await;
    let first = w.fake.calls().pop().unwrap().session;
    w.fake.forget_sessions();
    let answer = a.call("list_issues").await;
    assert_eq!(text(&answer), "issues on /mcp", "{answer}");
    let calls = w.fake.calls();
    let last = calls.last().unwrap();
    assert_eq!(last.status, 200);
    assert_ne!(last.session, first);
    assert_eq!(calls[calls.len() - 2].status, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shim_reconnecting_keeps_the_agents_session() {
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("a");
    a.call("list_issues").await;
    let first = w.fake.calls().pop().unwrap().session;
    drop(a);
    let mut a = w.agent("a");
    a.call("list_issues").await;
    assert_eq!(w.fake.calls().pop().unwrap().session, first);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sign_in_given_up_on_leaves_the_connection_as_it_was() {
    let w = World::new().await;
    w.sign_in().await;
    let kept = w.secret();
    w.services.connect("linear").await.unwrap();
    let mut a = w.agent("a");
    assert_eq!(text(&a.call("list_issues").await), "issues on /mcp");
    assert_eq!(w.info().state, ServiceState::Connected);
    assert_eq!(w.secret(), kept);
}

#[tokio::test(flavor = "multi_thread")]
async fn disconnecting_forgets_the_login_and_tells_the_service() {
    let w = World::new().await;
    w.sign_in().await;
    let refresh = w.secret().refresh.unwrap();
    let mut a = w.agent("a");
    assert_eq!(a.tools().await.len(), TOOLS);
    let info = w.services.disconnect("linear").await.unwrap();
    assert_eq!(info.state, ServiceState::Disconnected);
    assert_eq!(info.role, ServiceRole::Follower);
    assert_eq!(info.tools, 0);
    assert_eq!(w.store.load("linear").unwrap(), None);
    assert_eq!(w.fake.revocations(), [refresh]);
    assert_eq!(a.tools().await.len(), 0);
    let answer = a.call("list_issues").await;
    assert!(is_error(&answer));
    assert_eq!(
        text(&answer),
        "Linear isn't connected on the Mac (VibeOS → Settings → Services → Connect)."
    );
    assert_eq!(
        w.services.disconnect("linear").await.unwrap().state,
        ServiceState::Disconnected
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_owners_policy_applies_to_what_lists_and_what_calls() {
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("a");

    // A tool denied: not listed, not called.
    let info = w
        .services
        .set(ServiceSetParams {
            service: "linear".into(),
            denied_tools: Some(vec!["create_issue".into()]),
            ..ServiceSetParams::default()
        })
        .unwrap();
    assert_eq!(info.denied_tools, ["create_issue"]);
    assert!(!a.tools().await.iter().any(|t| t == "create_issue"));
    let before = w.fake.calls().len();
    let denied = a.call("create_issue").await;
    assert!(is_error(&denied));
    assert!(text(&denied).contains("isn't allowed"), "{denied}");
    assert_eq!(w.fake.calls().len(), before, "it never reached the service");

    // Read-only: its other endpoint, and fewer tools.
    w.services
        .set(ServiceSetParams {
            service: "linear".into(),
            denied_tools: Some(Vec::new()),
            read_only: Some(true),
            ..ServiceSetParams::default()
        })
        .unwrap();
    assert_eq!(
        text(&a.call("list_issues").await),
        "issues on /mcp/readonly"
    );
    assert_eq!(a.tools().await, ["list_issues"]);
    let write = a.call("create_issue").await;
    assert!(write.get("error").is_some(), "{write}");

    // The kill switch: nothing lists, nothing calls.
    w.services
        .set(ServiceSetParams {
            service: "linear".into(),
            enabled: Some(false),
            read_only: Some(false),
            ..ServiceSetParams::default()
        })
        .unwrap();
    assert_eq!(a.tools().await.len(), 0);
    let before = w.fake.calls().len();
    let off = a.call("list_issues").await;
    assert!(is_error(&off));
    assert!(text(&off).contains("turned off"), "{off}");
    assert_eq!(w.fake.calls().len(), before);

    // And back on, kept over a restart.
    w.services
        .set(ServiceSetParams {
            service: "linear".into(),
            enabled: Some(true),
            ..ServiceSetParams::default()
        })
        .unwrap();
    assert_eq!(text(&a.call("list_issues").await), "issues on /mcp");
    assert!(w.restarted().list().services[0].enabled);
}

#[tokio::test(flavor = "multi_thread")]
async fn every_call_leaves_an_audit_line_with_no_secret_or_argument() {
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("vps:quiet-amber-otter");
    a.call("list_issues").await;
    a.call("boom").await;
    w.services
        .set(ServiceSetParams {
            service: "linear".into(),
            denied_tools: Some(vec!["create_issue".into()]),
            ..ServiceSetParams::default()
        })
        .unwrap();
    a.call("create_issue").await;
    w.fake.expire_access();
    w.fake.refuse_refresh("invalid_grant");
    a.call("list_issues").await;

    let audit = w.audit.lock().unwrap().clone();
    let outcomes: Vec<&str> = audit
        .iter()
        .map(|l| {
            l.split("outcome=")
                .nth(1)
                .unwrap()
                .split(' ')
                .next()
                .unwrap()
        })
        .collect();
    assert_eq!(
        outcomes,
        ["ok", "tool_error", "denied", "needs_reconnect"],
        "{audit:#?}"
    );
    assert!(
        audit[0].starts_with(
            "services: agent=vps:quiet-amber-otter service=linear tool=list_issues outcome=ok ms="
        ),
        "{audit:#?}"
    );
    for line in &audit {
        assert!(!line.contains(SECRET_MARK), "{line}");
        assert!(!line.contains("an-argument"), "{line}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_cut_short_still_leaves_its_audit_line() {
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("a");
    w.fake.hold_streams();
    a.send(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "stream_issues"}}))
        .await;
    assert_eq!(a.recv().await["method"], "notifications/progress");
    assert!(w.audit.lock().unwrap().is_empty(), "it's still under way");
    // The shim goes, as a client's cancel or a dropped link makes it.
    drop(a);
    for _ in 0..500 {
        if !w.audit.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let audit = w.audit.lock().unwrap().clone();
    assert_eq!(audit.len(), 1, "{audit:#?}");
    assert!(
        audit[0].contains("tool=stream_issues outcome=cancelled"),
        "{audit:#?}"
    );
    w.fake.release();
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_are_kept_for_when_the_service_is_away() {
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("a");
    assert_eq!(a.tools().await.len(), TOOLS);
    let services = w.restarted();
    w.fake.reject_all_tokens(true);
    let mut a = Shim::open(&services, "a");
    assert_eq!(a.tools().await.len(), TOOLS);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_that_ends_unanswered_says_so_plainly() {
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("a");
    let answer = a.call("no_answer").await;
    assert!(is_error(&answer), "{answer}");
    assert!(
        text(&answer).contains("Linear can't be reached from the Mac now"),
        "{answer}"
    );
    assert_eq!(
        w.info().state,
        ServiceState::Connected,
        "it's no login problem"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_error_from_the_service_is_passed_on_as_it_is() {
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("a");
    let answer = a.call("boom").await;
    assert!(is_error(&answer));
    assert_eq!(text(&answer), "it broke");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_linux_host_says_a_login_gets_mended_on_the_mac() {
    let fake = FakeService::start().await;
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore::default());
    let (now, audit, changes) = (Arc::default(), Arc::default(), Arc::default());
    let services = Services::new(
        vec![fake.service()],
        store,
        dir.path().join("services.json"),
        Deps {
            place: Place::LinuxHost,
            ..deps(&now, &audit, &changes)
        },
    );
    let mut a = Shim::open(&services, "a");
    let not_yet = a.call("list_issues").await;
    assert!(is_error(&not_yet));
    assert_eq!(
        text(&not_yet),
        "Linear isn't connected on the Mac (VibeOS → Settings → Services → Connect)."
    );
    let auth = services.connect("linear").await.unwrap();
    let back = fake.sign_in(&auth.url).await;
    services.complete("linear", &back).await.unwrap();
    fake.reject_all_tokens(true);
    let lost = a.call("list_issues").await;
    assert!(is_error(&lost));
    assert_eq!(text(&lost), RECONNECT);
    fake.reject_all_tokens(false);
    services
        .set(ServiceSetParams {
            service: "linear".into(),
            enabled: Some(false),
            ..ServiceSetParams::default()
        })
        .unwrap();
    let off = a.call("list_issues").await;
    assert!(is_error(&off));
    assert_eq!(
        text(&off),
        "Linear is turned off on this host (`vibe service on linear` here, or VibeOS → Settings \
         → Services on the Mac)."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_service_nobody_supports_is_refused() {
    let w = World::new().await;
    let (out, _rx) = mpsc::channel(1);
    assert!(w.services.open("jira", "a".into(), out).is_err());
    assert!(w.services.connect("jira").await.is_err());
    assert!(w.services.disconnect("jira").await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_outage_at_refresh_is_no_reason_to_reconnect() {
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("a");
    a.call("list_issues").await;
    let kept = w.secret();
    w.fake.expire_access();
    w.fake.token_outage(true);

    let answer = a.call("list_issues").await;
    assert!(is_error(&answer), "{answer}");
    assert!(
        text(&answer).contains("Linear can't be reached from the Mac now"),
        "{answer}"
    );
    assert!(text(&answer).contains("503"), "{answer}");
    let info = w.info();
    assert_eq!(info.state, ServiceState::Connected, "{info:?}");
    assert!(
        info.detail.as_deref().unwrap().contains("can't refresh"),
        "{info:?}"
    );
    assert_eq!(w.secret(), kept);

    // It passes by itself, and the note goes with it.
    w.fake.token_outage(false);
    assert_eq!(text(&a.call("list_issues").await), "issues on /mcp");
    assert_eq!(w.info().detail, None);
}

// ---- What a review found.

/// Sends something as it is, and returns whatever comes back to it.
async fn raw(a: &mut Shim, message: Value) -> Value {
    a.send(message).await;
    a.recv().await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shim_going_away_mid_refresh_cant_lose_the_rotated_token() {
    let w = World::new().await;
    w.sign_in().await;
    // With no replay grace, a used token lost here could never be recovered.
    w.fake.set_replay_grace(false);
    let before = w.secret();
    w.fake.expire_access();
    w.fake.slow_refreshes(Duration::from_millis(300));

    let a = w.agent("a");
    a.send(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "list_issues"}}))
        .await;
    // The refresh is under way at the service, and the shim goes.
    for _ in 0..500 {
        if w.info().state == ServiceState::Refreshing {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(w.info().state, ServiceState::Refreshing);
    drop(a);

    // It finishes all the same, and what it got is kept.
    for _ in 0..500 {
        if w.info().state == ServiceState::Connected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        w.info().state,
        ServiceState::Connected,
        "not stuck refreshing"
    );
    assert_ne!(
        w.secret().refresh,
        before.refresh,
        "the rotated token was kept"
    );
    assert_eq!(w.fake.refreshes(), 1);

    // And the next shim uses it, with no refresh of its own.
    let mut b = w.agent("b");
    assert_eq!(text(&b.call("list_issues").await), "issues on /mcp");
    assert_eq!(w.fake.refreshes(), 1);
    assert_eq!(
        w.fake.calls().pop().unwrap().bearer,
        Some(w.secret().access)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_gets_past_the_policy_and_audit_by_how_its_shaped() {
    let w = World::new().await;
    w.sign_in().await;
    w.services
        .set(ServiceSetParams {
            service: "linear".into(),
            denied_tools: Some(vec!["create_issue".into()]),
            ..ServiceSetParams::default()
        })
        .unwrap();
    let mut a = w.agent("a");
    a.call("list_issues").await;
    let (seen, audited) = (w.fake.seen().len(), w.audit.lock().unwrap().len());
    let denied = json!({"name": "create_issue", "arguments": {}});

    // A denied tool with no id, which a catch-all would send on with the Bearer.
    let answer = raw(
        &mut a,
        json!({"jsonrpc": "2.0", "method": "tools/call", "params": denied}),
    )
    .await;
    assert_eq!(answer["error"]["code"], -32600, "{answer}");
    // In a batch.
    let batch = json!([{"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": denied}]);
    let answer = raw(&mut a, batch).await;
    assert_eq!(answer["error"]["code"], -32600, "{answer}");
    // An id with no method, which isn't an answer to anything.
    let answer = raw(&mut a, json!({"jsonrpc": "2.0", "id": 6, "params": denied})).await;
    assert_eq!(answer["error"]["code"], -32600, "{answer}");
    // A method that isn't a word.
    let answer = raw(&mut a, json!({"jsonrpc": "2.0", "id": 7, "method": 1})).await;
    assert_eq!(answer["error"]["code"], -32600, "{answer}");
    // A notification only by its lack of an id.
    let answer = raw(
        &mut a,
        json!({"jsonrpc": "2.0", "method": "resources/read", "params": {}}),
    )
    .await;
    assert_eq!(answer["error"]["code"], -32600, "{answer}");
    // Requests nothing offers.
    let answer = raw(
        &mut a,
        json!({"jsonrpc": "2.0", "id": 8, "method": "resources/list"}),
    )
    .await;
    assert_eq!(answer["error"]["code"], -32601, "{answer}");
    assert_eq!(w.fake.seen().len(), seen, "none of it reached the service");
    assert_eq!(w.audit.lock().unwrap().len(), audited);

    // A call with no name can't be judged, so it doesn't go, and it's audited.
    let answer = a.ask("tools/call", json!({"arguments": {}})).await;
    assert_eq!(answer["error"]["code"], -32602, "{answer}");
    assert_eq!(w.fake.seen().len(), seen);
    assert!(
        w.audit
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("outcome=invalid")
    );

    // What's meant to pass still does.
    a.send(
        json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 1}}),
    )
    .await;
    for _ in 0..500 {
        if w.fake
            .seen()
            .iter()
            .any(|s| s.method == "notifications/cancelled")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        w.fake
            .seen()
            .iter()
            .any(|s| s.method == "notifications/cancelled")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_refusal_of_the_login_itself_means_reconnecting() {
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("a");
    w.fake.expire_access();
    for passing in ["temporarily_unavailable", "server_error", "slow_down"] {
        w.fake.refuse_refresh(passing);
        let answer = a.call("list_issues").await;
        assert!(is_error(&answer), "{answer}");
        assert!(text(&answer).contains(passing), "{answer}");
        assert_ne!(text(&answer), RECONNECT);
        assert_eq!(w.info().state, ServiceState::Connected, "{passing}");
    }
    for fatal in ["invalid_grant", "invalid_client", "unauthorized_client"] {
        let w = World::new().await;
        w.sign_in().await;
        let mut a = w.agent("a");
        w.fake.expire_access();
        w.fake.refuse_refresh(fatal);
        assert_eq!(text(&a.call("list_issues").await), RECONNECT, "{fatal}");
        assert_eq!(w.info().state, ServiceState::NeedsReconnect, "{fatal}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_corrupt_state_file_starts_every_service_off_and_says_why() {
    let w = World::new().await;
    w.sign_in().await;
    w.services
        .set(ServiceSetParams {
            service: "linear".into(),
            denied_tools: Some(vec!["create_issue".into()]),
            ..ServiceSetParams::default()
        })
        .unwrap();
    std::fs::write(&w.state_file, "{ half a file").unwrap();

    let services = w.restarted();
    let info = services.list().services.remove(0);
    assert!(!info.enabled, "it can't have quietly lost its policy");
    assert!(
        info.detail.as_deref().unwrap().contains("couldn't be read"),
        "{info:?}"
    );
    let mut a = Shim::open(&services, "a");
    assert_eq!(a.tools().await.len(), 0);
    let off = a.call("create_issue").await;
    assert!(is_error(&off) && text(&off).contains("turned off"), "{off}");
    assert_eq!(
        std::fs::read_to_string(w.state_file.with_extension("corrupt")).unwrap(),
        "{ half a file"
    );
    assert!(
        w.audit
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("can't be read"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_setting_that_cant_be_kept_isnt_made() {
    let w = World::new().await;
    w.sign_in().await;
    // Where the file's written first is taken, so the write can't happen.
    let blocker = w.state_file.with_extension("new");
    for _ in 0..500 {
        // The background read of its tools may be writing there just now.
        if std::fs::create_dir(&blocker).is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(blocker.is_dir());
    let err = w
        .services
        .set(ServiceSetParams {
            service: "linear".into(),
            denied_tools: Some(vec!["create_issue".into()]),
            ..ServiceSetParams::default()
        })
        .unwrap_err();
    assert!(err.message.contains("nothing changed"), "{err:?}");
    assert!(w.info().denied_tools.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_left_open_after_its_answer_holds_nothing_up() {
    let w = World::new().await;
    w.fake.linger_streams();
    w.sign_in().await;
    let mut a = w.agent("a");
    // Both `initialize`, which holds its agent's session cell, and a call.
    let answer = tokio::time::timeout(Duration::from_secs(15), a.call("stream_issues"))
        .await
        .expect("it doesn't wait for the stream's end");
    assert_eq!(text(&answer), "streamed");
    // And the catalog, whose gate would hold up every other agent.
    let tools = tokio::time::timeout(Duration::from_secs(15), a.tools())
        .await
        .unwrap();
    assert_eq!(tools.len(), TOOLS);
}

#[derive(Debug, Default)]
struct Counting {
    inner: MemoryStore,
    loads: AtomicUsize,
}

impl SecretStore for Counting {
    fn load(&self, service: &str) -> io::Result<Option<Secret>> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        self.inner.load(service)
    }
    fn store(&self, service: &str, secret: &Secret) -> io::Result<()> {
        self.inner.store(service, secret)
    }
    fn delete(&self, service: &str) -> io::Result<()> {
        self.inner.delete(service)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn starting_doesnt_wait_on_the_store() {
    let store = Arc::new(Counting::default());
    let w = World::start(store.clone()).await;
    assert_eq!(store.loads.load(Ordering::SeqCst), 0, "not while starting");
    // What's there comes to light once it's read.
    store
        .inner
        .store("linear", &{
            let mut s = secret_like();
            s.client_id = None;
            s
        })
        .unwrap();
    assert_eq!(w.info().state, ServiceState::Disconnected);
    w.services.load_logins().await;
    assert_eq!(store.loads.load(Ordering::SeqCst), 1);
    assert_eq!(w.info().state, ServiceState::Connected);
}

fn secret_like() -> Secret {
    Secret {
        access: "FAKESECRET-a-token".into(),
        refresh: None,
        obtained_at: None,
        expires_at: None,
        client_id: None,
        token_endpoint: None,
        revocation_endpoint: None,
        scopes: Vec::new(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sign_in_given_up_on_doesnt_register_another_client_each_time() {
    let w = World::new().await;
    for _ in 0..3 {
        w.services.connect("linear").await.unwrap();
    }
    assert_eq!(w.fake.registrations(), 1);
    let auth = w.services.connect("linear").await.unwrap();
    let back = w.fake.sign_in(&auth.url).await;
    w.services.complete("linear", &back).await.unwrap();
    assert_eq!(w.fake.registrations(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn one_shim_cant_have_without_limit_under_way() {
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("a");
    a.tools().await;
    w.fake.hold_streams();
    for id in 0..40 {
        a.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "stream_issues"}}))
            .await;
    }
    let mut refused = 0;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while refused == 0 && std::time::Instant::now() < deadline {
        if a.recv().await["error"]["code"] == -32000 {
            refused += 1;
        }
    }
    assert!(refused >= 1, "the ones over the limit were told so");
    w.fake.release();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_only_accepted_doesnt_leave_the_agent_waiting() {
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("a");
    let answer = a.call("accepts_only").await;
    assert!(is_error(&answer), "{answer}");
    assert!(text(&answer).contains("won't answer"), "{answer}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_service_repeating_a_token_in_its_error_doesnt_get_it_shown() {
    let w = World::new().await;
    w.sign_in().await;
    let refresh = w.secret().refresh.unwrap();
    w.fake.echo_refresh_in_errors();
    w.fake.expire_access();
    w.fake.refuse_refresh("invalid_grant");
    let mut a = w.agent("a");
    a.call("list_issues").await;
    let detail = w.info().detail.unwrap();
    assert!(detail.contains("[redacted]"), "{detail}");
    assert!(!detail.contains(&refresh), "{detail}");
    assert!(!detail.contains(SECRET_MARK), "{detail}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_turned_down_after_the_login_changed_says_nothing_of_the_new_one() {
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("a");
    a.call("list_issues").await;
    // A call finds its token refused, and is refreshing, while the owner signs in
    // again. The refreshed token's refused too, but it's no longer the login.
    w.fake.reject_old_logins();
    w.fake.slow_refreshes(Duration::from_secs(2));
    let calling = tokio::spawn(async move { a.call("list_issues").await });
    for _ in 0..500 {
        if w.info().state == ServiceState::Refreshing {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    w.sign_in().await;
    let answer = calling.await.unwrap();
    assert_eq!(text(&answer), "issues on /mcp", "{answer}");
    assert_eq!(w.info().state, ServiceState::Connected);
}

#[tokio::test(flavor = "multi_thread")]
async fn what_a_service_tells_clients_reaches_them() {
    let w = World::new().await;
    w.fake.set_instructions("Be kind to the issues.");
    w.sign_in().await;
    let mut a = w.agent("a");
    let list = a.ask("tools/list", json!({})).await;
    assert_eq!(
        list["result"]["instructions"], "Be kind to the issues.",
        "{list}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_last_calls_can_be_read_back_and_outlast_a_restart() {
    let w = World::new().await;
    w.sign_in().await;
    let mut a = w.agent("vps:quiet");
    a.call("list_issues").await;
    let entries = w.services.audit().entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].agent, "vps:quiet");
    assert_eq!(entries[0].tool, "list_issues");
    assert_eq!(entries[0].outcome, "ok");
    assert_eq!(w.restarted().audit().entries, entries);
    let kept = std::fs::read_to_string(w.state_file.with_file_name("services-audit.log")).unwrap();
    assert!(!kept.contains(SECRET_MARK), "{kept}");
    assert!(!kept.contains("an-argument"), "{kept}");
}

#[tokio::test(flavor = "multi_thread")]
async fn running_unasked_is_a_choice_per_service() {
    let w = World::new().await;
    assert!(w.services.auto_allowed().is_empty());
    let info = w
        .services
        .set(ServiceSetParams {
            service: "linear".into(),
            auto_allow: Some(true),
            ..ServiceSetParams::default()
        })
        .unwrap();
    assert!(info.auto_allow);
    assert_eq!(w.services.auto_allowed(), ["linear"]);
    assert_eq!(w.restarted().auto_allowed(), ["linear"]);
}

/// A store whose next read takes its time, as `security` can at a daemon's
/// start: what it returns is what was there when it began.
#[derive(Debug)]
struct SlowFirstRead {
    inner: MemoryStore,
    slow: AtomicBool,
}

impl SlowFirstRead {
    fn new() -> Arc<SlowFirstRead> {
        Arc::new(SlowFirstRead {
            inner: MemoryStore::default(),
            slow: AtomicBool::new(false),
        })
    }
}

impl SecretStore for SlowFirstRead {
    fn load(&self, service: &str) -> io::Result<Option<Secret>> {
        let read = self.inner.load(service);
        if self.slow.swap(false, Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(800));
        }
        read
    }
    fn store(&self, service: &str, secret: &Secret) -> io::Result<()> {
        self.inner.store(service, secret)
    }
    fn delete(&self, service: &str) -> io::Result<()> {
        self.inner.delete(service)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_first_read_cant_bring_back_a_login_rotated_since() {
    let store = SlowFirstRead::new();
    let w = World::start(store.clone()).await;
    w.sign_in().await;
    // With no grace, using the old refresh token again would lose the login.
    w.fake.set_replay_grace(false);
    let again = w.restarted();
    store.slow.store(true, Ordering::SeqCst);
    let loading = {
        let again = again.clone();
        tokio::spawn(async move { again.load_logins().await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;

    // While that read is out, a call reads for itself and rotates the login.
    w.fake.expire_access();
    let mut a = Shim::open(&again, "a");
    let first = a.call("list_issues").await;
    assert!(!is_error(&first), "{first}");
    let rotated = w.secret();
    assert_eq!(w.fake.refreshes(), 1);

    // The old read lands, and is thrown away.
    loading.await.unwrap();
    let second = a.call("list_issues").await;
    assert!(!is_error(&second), "{second}");
    assert_eq!(
        w.fake.calls().pop().unwrap().bearer.as_deref(),
        Some(rotated.access.as_str())
    );
    let info = again.list().services.remove(0);
    assert_eq!(info.state, ServiceState::Connected, "{info:?}");
    assert_eq!(w.secret().access, rotated.access);
    assert_eq!(w.fake.refreshes(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_first_read_cant_undo_a_login_made_since() {
    let store = SlowFirstRead::new();
    let w = World::start(store.clone()).await;
    let again = w.restarted();
    store.slow.store(true, Ordering::SeqCst);
    let loading = {
        let again = again.clone();
        tokio::spawn(async move { again.load_logins().await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The read found no login, and the owner makes one before it comes back.
    let auth = again.connect("linear").await.unwrap();
    let back = w.fake.sign_in(&auth.url).await;
    let info = again.complete("linear", &back).await.unwrap();
    assert_eq!(info.state, ServiceState::Connected);
    loading.await.unwrap();

    let info = again.list().services.remove(0);
    assert_eq!(info.state, ServiceState::Connected, "{info:?}");
    assert_eq!(info.role, ServiceRole::Owner);
    let made = w.secret();
    let mut a = Shim::open(&again, "a");
    let answer = a.call("list_issues").await;
    assert!(!is_error(&answer), "{answer}");
    assert_eq!(
        w.fake.calls().pop().unwrap().bearer.as_deref(),
        Some(made.access.as_str())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_corrupt_state_file_stays_off_across_restarts_until_its_turned_on() {
    let w = World::new().await;
    w.services
        .set(ServiceSetParams {
            service: "linear".into(),
            denied_tools: Some(vec!["create_issue".into()]),
            ..ServiceSetParams::default()
        })
        .unwrap();
    std::fs::write(&w.state_file, "{ half a file").unwrap();

    // Off is written down by the first restart, so the next finds it too.
    let first = w.restarted().list().services.remove(0);
    assert!(!first.enabled, "{first:?}");
    assert!(w.state_file.exists(), "off has to outlast this run");
    serde_json::from_slice::<Value>(&std::fs::read(&w.state_file).unwrap()).unwrap();
    assert_eq!(
        std::fs::read_to_string(w.state_file.with_extension("corrupt")).unwrap(),
        "{ half a file",
        "the bad file is kept to look at"
    );
    let second = w.restarted();
    let info = second.list().services.remove(0);
    assert!(!info.enabled, "{info:?}");
    assert_eq!(info.detail, first.detail, "it still says why");
    assert!(
        info.detail
            .as_deref()
            .unwrap()
            .contains("VibeOS → Settings")
    );

    // Turning it on is the answer to that, and it lasts.
    let on = second
        .set(ServiceSetParams {
            service: "linear".into(),
            enabled: Some(true),
            ..ServiceSetParams::default()
        })
        .unwrap();
    assert!(on.enabled);
    assert_eq!((on.detail, on.detail_at), (None, None));
    let info = w.restarted().list().services.remove(0);
    assert!(info.enabled, "{info:?}");
    assert_eq!(info.detail, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn off_that_cant_be_written_leaves_the_corrupt_file_to_find_again() {
    let w = World::new().await;
    std::fs::write(&w.state_file, "{ half a file").unwrap();
    // Where its next version would be written: a directory, so that fails.
    let blocked = w.state_file.with_extension("new");
    std::fs::create_dir(&blocked).unwrap();

    assert!(!w.restarted().list().services.remove(0).enabled);
    assert_eq!(
        std::fs::read_to_string(&w.state_file).unwrap(),
        "{ half a file",
        "with no file a restart would take it that nothing was ever chosen"
    );
    assert!(
        w.audit
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("can't keep"))
    );
    assert!(!w.restarted().list().services.remove(0).enabled);

    // Once it can be written, it is.
    std::fs::remove_dir(&blocked).unwrap();
    assert!(!w.restarted().list().services.remove(0).enabled);
    serde_json::from_slice::<Value>(&std::fs::read(&w.state_file).unwrap()).unwrap();
    assert!(!w.restarted().list().services.remove(0).enabled);
}

#[tokio::test(flavor = "multi_thread")]
async fn at_the_limit_a_cancel_and_an_answer_still_get_through_and_no_refusal_is_lost() {
    let w = World::new().await;
    w.sign_in().await;
    let mut first = w.agent("first");
    first.tools().await;
    w.fake.hold_streams();

    // Room for one message at a time, and nobody reading it yet.
    let (out, mut rx) = mpsc::channel(1);
    let tx = w.services.open("linear", "a".to_owned(), out).unwrap();
    let say = |m: Value| {
        let tx = tx.clone();
        async move { tx.send(m.to_string().into_bytes()).await.unwrap() }
    };
    for id in 0..32 {
        say(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": "stream_issues"}}))
        .await;
    }
    for id in 1000..1003 {
        say(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": "stream_issues"}}))
        .await;
    }
    let seen = w.fake.seen().len();
    say(
        json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": {"requestId": 0}}),
    )
    .await;
    say(json!({"jsonrpc": "2.0", "id": "from-the-service", "result": {}})).await;

    // Both reach the service, though every call's still held.
    for _ in 0..1000 {
        if w.fake.seen().len() >= seen + 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        w.fake.seen().len() >= seen + 2,
        "{:?}",
        &w.fake.seen()[seen..]
    );

    // Every one over the limit is told, however slowly this end reads.
    let mut refused = Vec::new();
    while refused.len() < 3 {
        let bytes = tokio::time::timeout(Duration::from_secs(20), rx.recv())
            .await
            .expect("every refusal in time")
            .expect("the stream is open");
        let answer: Value = serde_json::from_slice(&bytes).unwrap();
        if answer["error"]["code"] == -32000 {
            refused.push(answer["id"].as_i64().unwrap());
        }
    }
    refused.sort_unstable();
    assert_eq!(refused, vec![1000, 1001, 1002]);
    w.fake.release();
}

// ---- Followers: a host that holds only an access token, given by way of the Mac.

/// A host that doesn't own the login, on a store and state of its own.
struct Follower {
    services: Arc<Services>,
    store: Arc<MemoryStore>,
    state_file: PathBuf,
    now: Arc<AtomicI64>,
    audit: Arc<Mutex<Vec<String>>>,
    _dir: tempfile::TempDir,
}

impl World {
    fn follower(&self) -> Follower {
        let dir = tempfile::tempdir().unwrap();
        let state_file = dir.path().join("services.json");
        let store = Arc::new(MemoryStore::default());
        let audit: Arc<Mutex<Vec<String>>> = Arc::default();
        let services = Services::new(
            vec![self.fake.service()],
            store.clone(),
            state_file.clone(),
            deps(&self.now, &audit, &Arc::default()),
        );
        Follower {
            services,
            store,
            state_file,
            now: self.now.clone(),
            audit,
            _dir: dir,
        }
    }
}

impl Follower {
    fn info(&self) -> ServiceInfo {
        self.services.list().services.remove(0)
    }

    /// Until it's read the service's tools, which it does in the background.
    async fn knows_the_tools(&self) {
        for _ in 0..1000 {
            if self.info().tools > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("it never read the tools");
    }

    /// What the Mac does: asks the owner for its token, and hands it over.
    async fn given_by(&self, owner: &Services, rejected: Option<&str>) -> String {
        let access = owner.token("linear", rejected).await.unwrap();
        let token = access.access.clone().expect("a token");
        self.services
            .follow(ServiceFollowParams {
                service: "linear".into(),
                owner: Some("vps-1".into()),
                state: access.state,
                access: access.access,
                expires_at: access.expires_at,
                detail: None,
            })
            .await
            .unwrap();
        token
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follower_uses_the_token_it_was_given_and_holds_no_refresh_token() {
    let w = World::new().await;
    w.sign_in().await;
    let f = w.follower();
    assert_eq!(f.info().role, ServiceRole::Follower);
    assert_eq!(f.info().state, ServiceState::Disconnected);

    let token = f.given_by(&w.services, None).await;
    assert_eq!(token, w.secret().access);
    let info = f.info();
    assert_eq!(info.state, ServiceState::Connected);
    assert_eq!(info.role, ServiceRole::Follower);
    assert_eq!(info.owner.as_deref(), Some("vps-1"));
    assert!(info.access_until.is_some(), "{info:?}");

    let mut a = Shim::open(&f.services, "a");
    assert_eq!(text(&a.call("list_issues").await), "issues on /mcp");
    assert_eq!(
        w.fake.calls().pop().unwrap().bearer.as_deref(),
        Some(token.as_str())
    );
    let held = f.store.load("linear").unwrap().expect("its token");
    assert_eq!(held.access, token);
    assert_eq!(held.refresh, None);
    assert_eq!(held.client_id, None);
    assert_eq!(held.token_endpoint, None);
    assert_eq!(w.fake.refreshes(), 0);

    // It outlasts a restart, with the owner nowhere to be seen.
    let again = Services::new(
        vec![w.fake.service()],
        f.store.clone(),
        f.state_file.clone(),
        deps(&f.now, &f.audit, &Arc::default()),
    );
    let mut b = Shim::open(&again, "b");
    assert_eq!(text(&b.call("list_issues").await), "issues on /mcp");
    let info = again.list().services.remove(0);
    assert_eq!(info.role, ServiceRole::Follower);
    assert_eq!(info.owner.as_deref(), Some("vps-1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follower_turned_down_asks_once_and_the_owner_refreshes() {
    let w = World::new().await;
    w.sign_in().await;
    let f = w.follower();
    let first = f.given_by(&w.services, None).await;
    let mut a = Shim::open(&f.services, "a");
    a.call("list_issues").await;

    // The Mac sees it wanting one, and passes on that this token was turned down.
    w.fake.expire_access();
    let mac = {
        let (owner, follower) = (w.services.clone(), f.services.clone());
        let first = first.clone();
        tokio::spawn(async move {
            for _ in 0..500 {
                if follower.list().services[0].wants_token {
                    let access = owner.token("linear", Some(&first)).await.unwrap();
                    follower
                        .follow(ServiceFollowParams {
                            service: "linear".into(),
                            owner: Some("vps-1".into()),
                            state: access.state,
                            access: access.access,
                            expires_at: access.expires_at,
                            detail: None,
                        })
                        .await
                        .unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("it never asked");
        })
    };
    let answer = a.call("list_issues").await;
    assert_eq!(text(&answer), "issues on /mcp", "{answer}");
    mac.await.unwrap();
    assert_eq!(w.fake.refreshes(), 1, "the owner's, and only the owner's");
    assert_ne!(w.secret().access, first);
    assert_eq!(f.store.load("linear").unwrap().unwrap().refresh, None);
    let info = f.info();
    assert!(!info.wants_token, "{info:?}");
    assert_eq!(info.state, ServiceState::Connected);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follower_serves_until_its_token_runs_out_then_says_the_owner_is_away() {
    let w = World::new().await;
    w.sign_in().await;
    let f = w.follower();
    f.given_by(&w.services, None).await;
    f.knows_the_tools().await;
    let mut a = Shim::open(&f.services, "a");

    // The owner's away from here on, however long it's been.
    let until = w.secret().expires_at.unwrap();
    w.now.store(until - 1, Ordering::SeqCst);
    assert_eq!(text(&a.call("list_issues").await), "issues on /mcp");
    assert_eq!(w.fake.refreshes(), 0);

    w.now.store(until, Ordering::SeqCst);
    let away = a.call("list_issues").await;
    assert!(is_error(&away), "{away}");
    assert_eq!(
        text(&away),
        "Linear has no current access token here: its login is on vps-1, which can't be reached \
         from here right now. Calls work again once it's back."
    );
    let info = f.info();
    assert_eq!(info.state, ServiceState::OwnerUnreachable);
    assert!(info.wants_token, "{info:?}");
    assert_eq!(w.fake.refreshes(), 0, "a follower can't refresh");
    assert!(
        f.audit
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("outcome=owner_unreachable")),
        "{:?}",
        f.audit.lock().unwrap()
    );
    // Tools still list, and the next call doesn't wait all over again.
    assert!(a.tools().await.contains(&"list_issues".to_owned()));
    let started = std::time::Instant::now();
    assert!(is_error(&a.call("list_issues").await));
    assert!(started.elapsed() < Duration::from_millis(250));

    // It's back: no restart, no sign-in.
    w.services.refresh_due().await;
    assert_eq!(w.fake.refreshes(), 1);
    f.given_by(&w.services, None).await;
    assert_eq!(text(&a.call("list_issues").await), "issues on /mcp");
    assert_eq!(f.info().state, ServiceState::Connected);
    assert!(!f.info().wants_token);
}

#[tokio::test(flavor = "multi_thread")]
async fn many_calls_on_a_follower_never_mean_a_refresh() {
    let w = World::new().await;
    w.sign_in().await;
    let f = w.follower();
    f.given_by(&w.services, None).await;
    let mut a = Shim::open(&f.services, "a");
    a.call("list_issues").await;

    w.fake.expire_access();
    w.fake.reject_all_tokens(true);
    for id in 0..8 {
        a.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": "list_issues"}}))
            .await;
    }
    let mut told = 0;
    while told < 8 {
        let answer = a.recv().await;
        if is_error(&answer) {
            told += 1;
        }
    }
    assert_eq!(w.fake.refreshes(), 0);
    assert_eq!(f.info().state, ServiceState::OwnerUnreachable);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_refreshes_at_half_life_and_followers_get_the_fresh_token() {
    let w = World::new().await;
    w.sign_in().await;
    let f = w.follower();
    let first = f.given_by(&w.services, None).await;
    let login = w.secret();
    let (from, until) = (login.obtained_at.unwrap(), login.expires_at.unwrap());

    w.now.store(from + (until - from) / 2 - 1, Ordering::SeqCst);
    w.services.refresh_due().await;
    assert_eq!(w.fake.refreshes(), 0, "not before half way");

    w.now.store(from + (until - from) / 2, Ordering::SeqCst);
    w.services.refresh_due().await;
    assert_eq!(w.fake.refreshes(), 1);
    w.services.refresh_due().await;
    assert_eq!(w.fake.refreshes(), 1, "and only the once");
    let second = f.given_by(&w.services, None).await;
    assert_ne!(first, second);
    assert!(f.store.load("linear").unwrap().unwrap().expires_at.unwrap() > until);

    // A follower has nothing to refresh, however late it is.
    w.now.store(until * 2, Ordering::SeqCst);
    f.services.refresh_due().await;
    assert_eq!(w.fake.refreshes(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_login_lost_at_the_owner_reaches_its_followers() {
    let w = World::new().await;
    w.sign_in().await;
    let f = w.follower();
    f.given_by(&w.services, None).await;
    let mut a = Shim::open(&f.services, "a");

    w.fake.expire_access();
    w.fake.refuse_refresh("invalid_grant");
    let access = w
        .services
        .token("linear", Some(&w.secret().access))
        .await
        .unwrap();
    assert_eq!(access.state, ServiceState::NeedsReconnect);
    assert_eq!(access.access, None);
    assert!(access.detail.as_deref().unwrap().contains("invalid_grant"));
    f.services
        .follow(ServiceFollowParams {
            service: "linear".into(),
            owner: Some("vps-1".into()),
            state: access.state,
            access: None,
            expires_at: None,
            detail: access.detail,
        })
        .await
        .unwrap();
    assert_eq!(text(&a.call("list_issues").await), RECONNECT);
    assert_eq!(f.info().state, ServiceState::NeedsReconnect);
    assert_eq!(f.store.load("linear").unwrap(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn following_makes_an_owner_forget_its_login_there_and_at_the_service() {
    let w = World::new().await;
    w.sign_in().await;
    let refresh = w.secret().refresh.unwrap();
    w.services
        .follow(ServiceFollowParams {
            service: "linear".into(),
            owner: Some("vps-1".into()),
            state: ServiceState::Connected,
            access: Some("FAKESECRET-from-the-new-owner".into()),
            expires_at: None,
            detail: None,
        })
        .await
        .unwrap();
    assert_eq!(w.info().role, ServiceRole::Follower);
    let held = w.secret();
    assert_eq!(held.refresh, None);
    assert_eq!(held.client_id, None);
    assert_eq!(w.fake.revocations(), [refresh]);

    // And it comes back a follower.
    let again = w.restarted();
    again.load_logins().await;
    assert_eq!(again.list().services[0].role, ServiceRole::Follower);
    w.services.refresh_due().await;
    assert_eq!(w.fake.refreshes(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_owner_gives_out_tokens_and_never_a_refresh_token() {
    let w = World::new().await;
    assert_eq!(
        w.services.token("linear", None).await.unwrap().state,
        ServiceState::Disconnected
    );
    w.sign_in().await;
    let f = w.follower();
    f.given_by(&w.services, None).await;

    let refused = f.services.token("linear", None).await.unwrap_err();
    assert!(
        refused.message.contains("doesn't own"),
        "{}",
        refused.message
    );
    let refused = f.services.disconnect("linear").await.unwrap_err();
    assert!(refused.message.contains("vps-1"), "{}", refused.message);
    assert!(f.store.load("linear").unwrap().is_some());

    let access = w.services.token("linear", None).await.unwrap();
    let on_the_wire = serde_json::to_string(&access).unwrap();
    assert!(!on_the_wire.contains(&w.secret().refresh.unwrap()));
    assert!(on_the_wire.contains(&w.secret().access));
    assert!(!format!("{access:?}").contains(&w.secret().access));
}
