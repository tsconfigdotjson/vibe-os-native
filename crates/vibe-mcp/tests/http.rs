//! The MCP server over real HTTP, on a fake daemon with two hosts: the key
//! gate, the transport, and every tool's arguments, routing and results.

mod client;
mod receiver;

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use client::{Client, post_raw, text};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use vibe_core::model::{
    Activity, AgentState, Certainty, Environment, EventKind, Profile, ProjectSource,
};
use vibe_core::rpc::{
    Agent, AgentDeleteParams, AgentStartParams, AgentStatus, ErrorCode, ErrorObject, Event, Project,
};
use vibe_mcp::webhook::{self, Webhooks};
use vibe_mcp::{
    AgentView, Caller, Check, Daemon, Input, MAX_RESULT_CHARS, Message, Role, Screen, Sent, Target,
};

const KEY: &str = "vk_aaaqeayeaudaocajbifqydiob4ibceqtcqkrmfyydenbwha5dypq";

#[derive(Debug, Default)]
struct Fake {
    log: Arc<Mutex<Vec<String>>>,
    sends: Mutex<Vec<(Target, Input)>>,
    request_ids: Mutex<Vec<String>>,
    /// Sends go out and nothing's heard of them again.
    sends_lost: AtomicBool,
    /// Sends find their host unavailable before they go.
    sends_refused: AtomicBool,
    /// A third host, `vps2`, and `twin`, an agent on it and on `vps`.
    third_host: AtomicBool,
    /// [`Daemon::known_hosts`].
    known: Mutex<HashMap<String, Vec<String>>>,
    /// [`Daemon::known_project_hosts`].
    known_projects: Mutex<HashMap<String, Vec<String>>>,
    starts: Mutex<Vec<AgentStartParams>>,
    deletes: Mutex<Vec<AgentDeleteParams>>,
    /// How many checks each agent stays busy for before it has answered;
    /// absent never answers.
    busy_for: Mutex<HashMap<String, u32>>,
    checks: Mutex<HashMap<String, u32>>,
    states: Mutex<HashMap<String, AgentState>>,
    screen: Mutex<String>,
    reads: Mutex<Vec<(Target, Option<u64>)>>,
    message_reads: Mutex<Vec<usize>>,
    /// What each agent is doing while it hasn't answered.
    activities: Mutex<HashMap<String, Activity>>,
    /// Each check's `background`.
    backgrounds: Mutex<Vec<bool>>,
    /// History given with `after`.
    history: Mutex<Option<Vec<String>>>,
    /// `vps` takes every listing and never answers, as a host stalling would.
    stuck: AtomicBool,
    /// Memory comes just after the deadline, as from a Mac too old to send it with its
    /// agents that the daemon has just given up on.
    late_memory: AtomicBool,
    /// Checks of an agent, by their count, that find its host's link down.
    link_down: Mutex<HashMap<String, std::ops::RangeInclusive<u32>>>,
    /// Its webhooks, as [`Daemon::webhooks`] gives them, made with `hooks_config`.
    hooks: std::sync::OnceLock<Arc<Webhooks>>,
    hooks_config: Mutex<Option<webhook::Config>>,
    /// It offers no events, as a daemon built before them.
    no_events: AtomicBool,
    /// [`KEY`] has been deleted.
    revoked: Arc<AtomicBool>,
    /// The keys, by hash, that hear what's relayed.
    relayed: Arc<Mutex<HashSet<String>>>,
    /// Keys that subscribed, by name, as [`Daemon::events_subscriber`] heard.
    subscribers: Mutex<Vec<String>>,
}

/// What the fake's webhooks ask of it.
#[derive(Debug)]
struct FakeOwners {
    revoked: Arc<AtomicBool>,
    relayed: Arc<Mutex<HashSet<String>>>,
    log: Arc<Mutex<Vec<String>>>,
}

impl webhook::Owners for FakeOwners {
    fn holds(&self, owner: &str) -> bool {
        owner != vibe_core::keys::hash(KEY) || !self.revoked.load(Ordering::SeqCst)
    }

    fn hears_relayed(&self, owner: &str) -> bool {
        self.relayed.lock().unwrap().contains(owner)
    }

    fn log(&self, line: &str) {
        self.log.lock().unwrap().push(line.to_owned());
    }
}

/// A second key, `phone`'s.
fn key2() -> String {
    vibe_core::keys::encode(&[7; 32])
}

fn not_found(m: String) -> ErrorObject {
    ErrorObject::new(ErrorCode::NotFound, m)
}

fn project(host: &str, name: &str) -> Project {
    Project {
        host: host.into(),
        name: name.into(),
        source: ProjectSource::Path,
        repo: None,
        path: format!("/src/{name}"),
        added_at: "2026-10-30T12:00:00Z".into(),
        profiles: vec![Profile::with_defaults(
            "software-eng-linux",
            Environment::Linux,
        )],
        deleted: false,
    }
}

fn agent(host: &str, r: &str, project: &str, state: AgentState) -> Agent {
    Agent {
        host: host.into(),
        agent: r.into(),
        project: project.into(),
        profile: "software-eng-linux".into(),
        kind: Default::default(),
        environment: Environment::Linux,
        state,
        worktree: format!("/w/{project}/{r}"),
        branch: r.into(),
        session_id: "5e55".into(),
        created_at: "2026-10-30T12:00:00Z".into(),
        attach: format!("vibe attach {r}"),
        memory: 4 << 30,
        memory_used: Some(340 << 20),
        status: AgentStatus {
            activity: Activity::Done,
            certainty: Certainty::Firm,
            since: None,
            title: None,
            line: Some("› hello".into()),
            pending_wakeups: vec![],
            stop_reason: None,
            resets_at: None,
            error: None,
        },
    }
}

/// Every agent: `quiet-amber-otter` is on both hosts.
fn all_agents() -> Vec<Agent> {
    vec![
        agent("local", "quiet-amber-otter", "app", AgentState::Idle),
        agent("local", "slow", "app", AgentState::Busy),
        agent("local", "never", "app", AgentState::Busy),
        agent("local", "sleepy", "app", AgentState::Asleep),
        agent("vps", "quiet-amber-otter", "web", AgentState::Question),
        agent("vps", "remote-only", "web", AgentState::Idle),
    ]
}

impl Fake {
    fn find(&self, t: &Target) -> Result<Agent, ErrorObject> {
        if t.agent == "twin" && self.third_host.load(Ordering::SeqCst) && t.host != "local" {
            return Ok(agent(&t.host, "twin", "app", AgentState::Idle));
        }
        all_agents()
            .into_iter()
            .find(|a| a.host == t.host && a.agent == t.agent)
            .ok_or_else(|| not_found(format!("no agent called {} on {}", t.agent, t.host)))
    }
}

impl Daemon for Fake {
    fn log(&self, line: &str) {
        self.log.lock().unwrap().push(line.to_owned());
    }

    fn verify_key(&self, secret: &str) -> Option<String> {
        if secret == key2() {
            return Some("phone".to_owned());
        }
        (secret == KEY && !self.revoked.load(Ordering::SeqCst)).then(|| "laptop".to_owned())
    }

    fn webhooks(&self) -> Option<Arc<Webhooks>> {
        if self.no_events.load(Ordering::SeqCst) {
            return None;
        }
        Some(Arc::clone(self.hooks.get_or_init(|| {
            let config = self
                .hooks_config
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_default();
            let hooks = Webhooks::new(Arc::new(vibe_mcp::events::Hub::default()), config);
            hooks.start(Arc::new(FakeOwners {
                revoked: Arc::clone(&self.revoked),
                relayed: Arc::clone(&self.relayed),
                log: Arc::clone(&self.log),
            }));
            hooks
        })))
    }

    async fn events_subscriber(&self, c: &Caller) {
        self.subscribers.lock().unwrap().push(c.name().to_owned());
    }

    fn known_hosts(&self, _c: &Caller, agent: &str) -> Vec<String> {
        self.known
            .lock()
            .unwrap()
            .get(agent)
            .cloned()
            .unwrap_or_default()
    }

    fn known_project_hosts(&self, _c: &Caller, project: &str) -> Vec<String> {
        self.known_projects
            .lock()
            .unwrap()
            .get(project)
            .cloned()
            .unwrap_or_default()
    }

    async fn hosts(&self, _c: &Caller) -> Result<Vec<String>, ErrorObject> {
        let mut hosts = vec!["local".to_owned(), "vps".to_owned(), "down".to_owned()];
        if self.third_host.load(Ordering::SeqCst) {
            hosts.push("vps2".to_owned());
        }
        Ok(hosts)
    }

    async fn memory(&self, _c: &Caller) -> Vec<(String, vibe_core::rpc::MemoryUse)> {
        if !self.late_memory.load(Ordering::SeqCst) {
            return Vec::new();
        }
        tokio::time::sleep(self.peer_deadline() + Duration::from_millis(50)).await;
        let memory = vibe_core::rpc::MemoryUse {
            total: 8 << 30,
            used: 2 << 30,
            swap_total: 0,
            swap_used: 0,
            pressure: None,
        };
        vec![("local".to_owned(), memory)]
    }

    async fn projects(&self, _c: &Caller, host: &str) -> Result<Vec<Project>, ErrorObject> {
        if host == "vps" && self.stuck.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        match host {
            "local" => Ok(vec![project("local", "app"), project("local", "web")]),
            "vps" => Ok(vec![project("vps", "web")]),
            "vps2" => Ok(vec![]),
            "down" => Err(ErrorObject::new(
                ErrorCode::InternalError,
                "its link is down",
            )),
            other => Err(not_found(format!("no host named {other}"))),
        }
    }

    async fn agents(&self, _c: &Caller, host: &str) -> Result<Vec<AgentView>, ErrorObject> {
        if host == "vps" && self.stuck.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        if host == "down" {
            return Err(ErrorObject::new(
                ErrorCode::InternalError,
                "its link is down",
            ));
        }
        let twin = (self.third_host.load(Ordering::SeqCst) && host != "local")
            .then(|| agent(host, "twin", "app", AgentState::Idle));
        Ok(all_agents()
            .into_iter()
            .filter(|a| a.host == host)
            .chain(twin)
            .map(|a| AgentView {
                attach_remote: format!("ssh -t mac {}", a.attach),
                available: true,
                last_seen: None,
                agent: a,
            })
            .collect())
    }

    async fn start_agent(&self, _c: &Caller, p: AgentStartParams) -> Result<Agent, ErrorObject> {
        self.starts.lock().unwrap().push(p.clone());
        let host = p.host.clone().unwrap();
        let name = p.name.clone().unwrap_or("brave-copper-lynx".into());
        Ok(agent(&host, &name, &p.project, AgentState::Busy))
    }

    async fn delete_agent(&self, _c: &Caller, p: AgentDeleteParams) -> Result<Agent, ErrorObject> {
        self.deletes.lock().unwrap().push(p.clone());
        let t = Target {
            host: p.host.clone().unwrap(),
            agent: p.agent.clone(),
        };
        let a = self.find(&t)?;
        if a.agent == "slow" && p.force != Some(true) {
            return Err(ErrorObject::new(
                ErrorCode::Conflict,
                "slow has unsaved work: 1 unpushed commit. Commit and push it, or delete with force",
            ));
        }
        Ok(a)
    }

    async fn send(
        &self,
        _c: &Caller,
        t: &Target,
        input: Input,
        request_id: &str,
    ) -> Result<Sent, ErrorObject> {
        let a = self.find(t)?;
        self.request_ids.lock().unwrap().push(request_id.to_owned());
        if self.sends_lost.load(Ordering::SeqCst) {
            return Err(ErrorObject::new(
                ErrorCode::HostUnavailable,
                "vps is unavailable: its link is down. Outcome unknown: check vibe_read_messages before resending",
            )
            .outcome_unknown());
        }
        if self.sends_refused.load(Ordering::SeqCst) {
            return Err(ErrorObject::new(
                ErrorCode::HostUnavailable,
                "vps is unavailable: its link is down, so nothing was sent",
            ));
        }
        self.sends.lock().unwrap().push((t.clone(), input));
        Ok(Sent {
            woke: a.state == AgentState::Asleep,
            state: AgentState::Busy,
        })
    }

    async fn check(&self, _c: &Caller, t: &Target, background: bool) -> Result<Check, ErrorObject> {
        if t.host == "vps" && self.stuck.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        self.backgrounds.lock().unwrap().push(background);
        let a = self.find(t)?;
        let n = {
            let mut checks = self.checks.lock().unwrap();
            let n = checks.entry(t.agent.clone()).or_default();
            *n += 1;
            *n
        };
        if self
            .link_down
            .lock()
            .unwrap()
            .get(&t.agent)
            .is_some_and(|down| down.contains(&n))
        {
            return Err(ErrorObject::new(
                ErrorCode::HostUnavailable,
                format!("{} is unavailable: its link is down", t.host),
            ));
        }
        let state = self
            .states
            .lock()
            .unwrap()
            .get(&t.agent)
            .copied()
            .unwrap_or(a.state);
        let answered = match self.busy_for.lock().unwrap().get(&t.agent) {
            Some(busy) => n > *busy,
            None => false,
        };
        let activity = self.activities.lock().unwrap().get(&t.agent).copied();
        Ok(Check {
            state: if answered && activity.is_none() {
                AgentState::Idle
            } else {
                state
            },
            activity: match (answered, activity) {
                (_, Some(a)) => a,
                (true, None) => Activity::Done,
                (false, None) => Activity::Thinking,
            },
            line: Some("$ cargo test".into()),
            answered,
        })
    }

    async fn read_output(
        &self,
        _c: &Caller,
        t: &Target,
        after: Option<u64>,
    ) -> Result<Screen, ErrorObject> {
        self.find(t)?;
        self.reads.lock().unwrap().push((t.clone(), after));
        let history = match (after, self.history.lock().unwrap().clone()) {
            (Some(_), Some(h)) => h,
            _ => vec!["old 1".into(), "old 2".into()],
        };
        // The cursor is after the last line, as a shim's is.
        let cursor = if history.len() > 2 {
            history.len() as u64
        } else {
            7
        };
        Ok(Screen {
            screen: self.screen.lock().unwrap().clone(),
            history,
            cursor,
            dropped: if after == Some(0) { 5 } else { 0 },
            more: false,
            live: t.agent != "sleepy",
        })
    }

    async fn read_messages(
        &self,
        _c: &Caller,
        t: &Target,
        last: usize,
    ) -> Result<Vec<Message>, ErrorObject> {
        self.find(t)?;
        self.message_reads.lock().unwrap().push(last);
        Ok(vec![
            Message {
                role: Role::User,
                text: "fix it".into(),
                at: Some("2026-10-30T12:00:00Z".into()),
            },
            Message {
                role: Role::Tool,
                text: "Bash(cargo test)".into(),
                at: None,
            },
            Message {
                role: Role::Assistant,
                text: "Fixed.".into(),
                at: None,
            },
        ])
    }
}

async fn serve(fake: Arc<Fake>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(vibe_mcp::http::serve(listener, fake, true));
    addr
}

async fn client() -> (Arc<Fake>, Client) {
    let fake = Arc::new(Fake::default());
    let addr = serve(Arc::clone(&fake)).await;
    let mut c = Client::new(addr, KEY);
    c.initialize("2025-06-18").await;
    (fake, c)
}

fn initialize() -> String {
    json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "t", "version": "1"}}})
    .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_request_needs_a_key() {
    let fake = Arc::new(Fake::default());
    let addr = serve(Arc::clone(&fake)).await;
    let body = initialize();
    for auth in [
        None,
        Some("Bearer vk_wrong".to_owned()),
        Some(format!("Basic {KEY}")),
        Some("Bearer".to_owned()),
        Some(format!("Bearer {}", &KEY[..KEY.len() - 1])),
    ] {
        let headers: Vec<(&str, &str)> =
            auth.iter().map(|a| ("Authorization", a.as_str())).collect();
        let r = post_raw(addr, "/mcp", &headers, &body).await;
        assert_eq!(r.status, 401, "{auth:?}: {}", r.body);
        assert_eq!(
            r.header("www-authenticate"),
            Some("Bearer realm=\"vibeos\"")
        );
        assert!(r.body.contains("API key"), "{}", r.body);
    }
    // Loopback is no exception, and neither is any other path.
    let r = post_raw(
        addr,
        "/",
        &[("Authorization", &format!("Bearer {KEY}"))],
        &body,
    )
    .await;
    assert_eq!(r.status, 404);
    let ok = post_raw(
        addr,
        "/mcp",
        &[("Authorization", &format!("Bearer {KEY}"))],
        &body,
    )
    .await;
    assert_eq!(ok.status, 200, "{}", ok.body);
    // The scheme's case doesn't matter.
    let ok = post_raw(
        addr,
        "/mcp",
        &[("Authorization", &format!("bearer {KEY}"))],
        &body,
    )
    .await;
    assert_eq!(ok.status, 200, "{}", ok.body);
    let info = &ok.response()["result"];
    assert_eq!(info["serverInfo"]["name"], "vibe-os-native");
    assert!(info["capabilities"]["tools"].is_object());
    assert!(info["instructions"].as_str().unwrap().contains("vibe_wait"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browsers_and_rebound_hosts_are_refused() {
    let fake = Arc::new(Fake::default());
    let addr = serve(Arc::clone(&fake)).await;
    let auth = format!("Bearer {KEY}");
    let r = post_raw(
        addr,
        "/mcp",
        &[("Authorization", &auth), ("Origin", "https://evil.example")],
        &initialize(),
    )
    .await;
    assert_eq!(r.status, 403, "{}", r.body);
    let r = post_raw(
        addr,
        "/mcp",
        &[("Authorization", &auth), ("Host", "evil.example")],
        &initialize(),
    )
    .await;
    assert!((400..500).contains(&r.status), "{} {}", r.status, r.body);
    for host in ["localhost:7878", "127.0.0.1"] {
        let r = post_raw(
            addr,
            "/mcp",
            &[("Authorization", &auth), ("Host", host)],
            &initialize(),
        )
        .await;
        assert_eq!(r.status, 200, "{host}: {}", r.body);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eight_tools_for_old_and_new_clients() {
    let fake = Arc::new(Fake::default());
    let addr = serve(Arc::clone(&fake)).await;
    for version in ["2025-03-26", "2025-06-18", "2025-11-25"] {
        let mut c = Client::new(addr, KEY);
        let info = c.initialize(version).await;
        assert_eq!(info["protocolVersion"], version);
        let tools = c.list_tools().await;
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "vibe_list_projects",
                "vibe_start_agent",
                "vibe_list_agents",
                "vibe_send",
                "vibe_wait",
                "vibe_read_messages",
                "vibe_read_output",
                "vibe_delete_agent"
            ],
            "{version}"
        );
        let r = c.call("vibe_list_projects", json!({})).await;
        assert!(
            text(&r).contains("app (on local)"),
            "{version}: {}",
            text(&r)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn projects_on_every_host_with_their_profiles() {
    let (fake, mut c) = client().await;
    let r = c.call("vibe_list_projects", json!({})).await;
    let t = text(&r);
    assert!(t.contains("app (on local) /src/app"), "{t}");
    assert!(t.contains("web (on vps) /src/web"), "{t}");
    assert!(
        t.contains("  software-eng-linux: linux, opus, high reasoning, auto mode, 4G"),
        "{t}"
    );
    assert!(t.contains("down is unavailable: its link is down"), "{t}");
    let projects = r["structuredContent"]["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 3);
    assert_eq!(projects[2]["host"], "vps");
    assert_eq!(projects[0]["profiles"][0]["memory"], 4u64 << 30);
    let r = c.call("vibe_list_projects", json!({"host": "vps"})).await;
    assert_eq!(
        r["structuredContent"]["projects"].as_array().unwrap().len(),
        1
    );
    let e = c.fails("vibe_list_projects", json!({"host": "mars"})).await;
    assert_eq!(e, "no host named mars");
    // Who called what is logged, by key name.
    let log = fake.log.lock().unwrap().join("\n");
    assert!(
        log.contains("mcp: key \"laptop\" called vibe_list_projects"),
        "{log}"
    );
    assert!(!log.contains(KEY), "{log}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_goes_to_the_host_its_project_is_on() {
    let (fake, mut c) = client().await;
    let r = c
        .call(
            "vibe_start_agent",
            json!({"project": "app", "profile": "software-eng-linux", "prompt": "fix it"}),
        )
        .await;
    assert!(
        text(&r).starts_with("started brave-copper-lynx as software-eng-linux in app on local"),
        "{}",
        text(&r)
    );
    assert!(text(&r).contains("call vibe_wait"));
    assert_eq!(r["structuredContent"]["ref"], "brave-copper-lynx");
    // Memory in the names vibe_list_agents gives it: in use, and the limit.
    assert_eq!(r["structuredContent"]["memory"], 340u64 << 20);
    assert_eq!(r["structuredContent"]["memory_limit"], 4u64 << 30);
    assert!(r["structuredContent"].get("memory_used").is_none());
    assert_eq!(
        fake.starts.lock().unwrap()[0].host.as_deref(),
        Some("local")
    );
    // On two hosts, the call says which.
    let e = c
        .fails(
            "vibe_start_agent",
            json!({"project": "web", "profile": "software-eng-linux", "prompt": "x"}),
        )
        .await;
    assert_eq!(
        e,
        "web is a project on local and vps: pass host to say which"
    );
    let r = c
        .call(
            "vibe_start_agent",
            json!({"project": "web", "profile": "software-eng-linux", "prompt": "x",
                   "host": "vps", "from": "main", "name": "my-fix"}),
        )
        .await;
    assert_eq!(r["structuredContent"]["ref"], "vps:my-fix");
    let p = fake.starts.lock().unwrap()[1].clone();
    assert_eq!(
        (
            p.host.as_deref(),
            p.from.as_deref(),
            p.name.as_deref(),
            p.prompt.as_str()
        ),
        (Some("vps"), Some("main"), Some("my-fix"), "x")
    );
    let e = c
        .fails(
            "vibe_start_agent",
            json!({"project": "nope", "profile": "p", "prompt": "x"}),
        )
        .await;
    assert!(
        e.starts_with("no project called nope on local, vps, down"),
        "{e}"
    );
    for missing in ["project", "profile", "prompt"] {
        let mut args = json!({"project": "app", "profile": "p", "prompt": "x"});
        args.as_object_mut().unwrap().remove(missing);
        let e = c.fails("vibe_start_agent", args).await;
        assert_eq!(e, format!("{missing} is required"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_on_every_host_filtered() {
    let (_fake, mut c) = client().await;
    let r = c.call("vibe_list_agents", json!({})).await;
    let t = text(&r);
    assert!(
        t.contains("quiet-amber-otter  idle (done)  software-eng-linux  app · branch quiet-amber-otter  340M of 4G  › hello  attach: ssh -t mac vibe attach quiet-amber-otter"),
        "{t}"
    );
    assert!(t.contains("vps:quiet-amber-otter  question"), "{t}");
    assert!(t.contains("down is unavailable"), "{t}");
    let agents = r["structuredContent"]["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 6);
    assert_eq!(agents[4]["ref"], "vps:quiet-amber-otter");
    assert_eq!(agents[4]["host"], "vps");
    assert_eq!(agents[0]["memory"], 340u64 << 20);
    assert_eq!(agents[0]["attach"], "vibe attach quiet-amber-otter");
    let only = |r: &Value| -> Vec<String> {
        r["structuredContent"]["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["ref"].as_str().unwrap().to_owned())
            .collect()
    };
    let r = c.call("vibe_list_agents", json!({"host": "vps"})).await;
    assert_eq!(only(&r), ["vps:quiet-amber-otter", "vps:remote-only"]);
    let r = c.call("vibe_list_agents", json!({"state": "asleep"})).await;
    assert_eq!(only(&r), ["sleepy"]);
    let r = c
        .call(
            "vibe_list_agents",
            json!({"project": "web", "state": "idle"}),
        )
        .await;
    assert_eq!(only(&r), ["vps:remote-only"]);
    let r = c.call("vibe_list_agents", json!({"project": "none"})).await;
    assert_eq!(text(&r).lines().next(), Some("no agents"));
    let e = c
        .fails("vibe_list_agents", json!({"state": "sleeping"}))
        .await;
    assert!(
        e.starts_with("state must be one of busy, idle, question"),
        "{e}"
    );
}

/// #63: a daemon that's given up on an older Mac right at the deadline still has this
/// host's own memory to say, so the listing isn't to give up on it first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_just_past_the_deadline_isnt_lost() {
    let (fake, mut c) = client().await;
    fake.late_memory.store(true, Ordering::SeqCst);
    let r = c.call("vibe_list_agents", json!({})).await;
    let t = text(&r);
    assert!(t.contains("local: 2G of 8G memory in use"), "{t}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_that_doesnt_answer_holds_up_nothing_of_this_ones() {
    let (fake, mut c) = client().await;
    fake.stuck.store(true, Ordering::SeqCst);

    let started = Instant::now();
    let r = c.call("vibe_list_agents", json!({})).await;
    let t = text(&r);
    assert!(t.contains("quiet-amber-otter  idle"), "{t}");
    assert!(
        t.contains("vps is unavailable: didn't answer in 3 s"),
        "{t}"
    );
    let r = c.call("vibe_list_projects", json!({})).await;
    let t = text(&r);
    assert!(t.contains("app (on local)"), "{t}");
    assert!(
        t.contains("vps is unavailable: didn't answer in 3 s"),
        "{t}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );

    // A bare ref this host has doesn't ask anyone, stuck or not.
    let started = Instant::now();
    c.call("vibe_read_output", json!({"agent": "quiet-amber-otter"}))
        .await;
    assert!(started.elapsed() < Duration::from_secs(2));

    // And a project this host has goes here, with the other only let go.
    let r = c
        .call(
            "vibe_start_agent",
            json!({"project": "web", "profile": "p", "prompt": "go"}),
        )
        .await;
    assert!(text(&r).contains("on local"), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bare_ref_on_two_other_hosts_is_an_error_known_or_asked() {
    let (fake, mut c) = client().await;
    fake.third_host.store(true, Ordering::SeqCst);
    let said = "twin is on vps and vps2: name it with its host, as vps:twin or vps2:twin";

    // Asked.
    let e = c.fails("vibe_read_output", json!({"agent": "twin"})).await;
    assert!(e.contains(said), "{e}");

    // Known from before, on both: the same, and not whichever comes first.
    fake.known
        .lock()
        .unwrap()
        .insert("twin".into(), vec!["vps".into(), "vps2".into()]);
    let e = c
        .fails("vibe_send", json!({"agent": "twin", "text": "hi"}))
        .await;
    assert!(e.contains(said), "{e}");
    assert!(fake.sends.lock().unwrap().is_empty());

    // Naming the host says which.
    c.call("vibe_send", json!({"agent": "vps2:twin", "text": "hi"}))
        .await;

    // Known on one, with the others stuck: it goes there, with no waiting.
    fake.known
        .lock()
        .unwrap()
        .insert("twin".into(), vec!["vps".into()]);
    fake.stuck.store(true, Ordering::SeqCst);
    let begun = Instant::now();
    c.call("vibe_send", json!({"agent": "twin", "text": "hi"}))
        .await;
    assert!(begun.elapsed() < Duration::from_secs(2));

    // This host having it beats any of that.
    fake.known
        .lock()
        .unwrap()
        .insert("slow".into(), vec!["vps".into(), "vps2".into()]);
    c.call("vibe_send", json!({"agent": "slow", "text": "hi"}))
        .await;
    assert_eq!(fake.sends.lock().unwrap().last().unwrap().0.host, "local");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lookups_say_when_a_host_just_didnt_answer() {
    let (fake, mut c) = client().await;
    fake.stuck.store(true, Ordering::SeqCst);

    // Not a plain not-found: it may well be on the host that didn't answer.
    let e = c
        .fails("vibe_read_output", json!({"agent": "nobody"}))
        .await;
    assert!(e.contains("nobody isn't on any host that answered"), "{e}");
    assert!(e.contains("vps didn't"), "{e}");
    let e = c
        .fails(
            "vibe_start_agent",
            json!({"project": "nope", "profile": "p", "prompt": "go"}),
        )
        .await;
    assert!(
        e.contains("no project called nope on any host that answered, and vps didn't"),
        "{e}"
    );

    // A project last seen there is still there, so this is no quicker a way to say
    // which: the same answer it gets from a host that answers in time.
    fake.known_projects
        .lock()
        .unwrap()
        .insert("web".into(), vec!["vps".into()]);
    let e = c
        .fails(
            "vibe_start_agent",
            json!({"project": "web", "profile": "p", "prompt": "go"}),
        )
        .await;
    assert!(
        e.contains("web is a project on local and vps: pass host to say which"),
        "{e}"
    );
    assert!(fake.starts.lock().unwrap().is_empty());

    // And one only there goes there, to fail or not as that host's calls do.
    fake.known_projects
        .lock()
        .unwrap()
        .insert("only-there".into(), vec!["vps".into()]);
    c.call(
        "vibe_start_agent",
        json!({"project": "only-there", "profile": "p", "prompt": "go"}),
    )
    .await;
    let starts = fake.starts.lock().unwrap();
    assert_eq!(starts.last().unwrap().host.as_deref(), Some("vps"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wait_doesnt_stand_behind_a_host_that_doesnt_answer() {
    let (fake, mut c) = client().await;
    fake.busy_for
        .lock()
        .unwrap()
        .insert("quiet-amber-otter".into(), 0);
    fake.stuck.store(true, Ordering::SeqCst);
    let begun = Instant::now();
    let r = c
        .call(
            "vibe_wait",
            json!({"agents": ["vps:remote-only", "local:quiet-amber-otter"], "timeout": 60}),
        )
        .await;
    assert!(
        text(&r).starts_with("quiet-amber-otter has answered"),
        "{}",
        text(&r)
    );
    assert!(
        begun.elapsed() < Duration::from_secs(5),
        "{:?}",
        begun.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_that_may_have_arrived_says_how_to_retry_it_safely() {
    let (fake, mut c) = client().await;
    // Every send carries an id, each its own unless the caller gives one.
    for args in [
        json!({"agent": "slow", "text": "one"}),
        json!({"agent": "slow", "text": "one"}),
        json!({"agent": "slow", "text": "two", "request_id": "mine-1"}),
    ] {
        c.call("vibe_send", args).await;
    }
    {
        let ids = fake.request_ids.lock().unwrap();
        assert_eq!(ids.len(), 3);
        assert_ne!(ids[0], ids[1]);
        assert_eq!(ids[2], "mine-1");
    }

    // A host unavailable names the id to retry with.
    fake.sends_lost.store(true, Ordering::SeqCst);
    let e = c
        .fails("vibe_send", json!({"agent": "slow", "text": "three"}))
        .await;
    let id = fake.request_ids.lock().unwrap().last().unwrap().clone();
    assert!(e.contains("vps is unavailable"), "{e}");
    assert!(e.contains(&format!("request_id \"{id}\"")), "{e}");

    // It says to look first, and that the id delivers it at most once.
    assert!(e.contains("check vibe_read_messages"), "{e}");
    assert!(e.contains("at most once"), "{e}");

    // Yours is the one it names, if you gave one.
    let e = c
        .fails(
            "vibe_send",
            json!({"agent": "slow", "text": "three", "request_id": "mine-2"}),
        )
        .await;
    assert!(e.contains("request_id \"mine-2\""), "{e}");
    fake.sends_lost.store(false, Ordering::SeqCst);

    // One known not to have gone is in no doubt, whatever its host is up to.
    fake.sends_refused.store(true, Ordering::SeqCst);
    let e = c
        .fails("vibe_send", json!({"agent": "slow", "text": "three"}))
        .await;
    assert!(e.contains("nothing was sent"), "{e}");
    assert!(!e.contains("request_id"), "{e}");
    fake.sends_refused.store(false, Ordering::SeqCst);

    // Nor is anything else, and it doesn't mention it.
    let e = c
        .fails("vibe_send", json!({"agent": "nobody", "text": "four"}))
        .await;
    assert!(!e.contains("request_id"), "{e}");

    // A bad id goes nowhere.
    let before = fake.request_ids.lock().unwrap().len();
    for bad in ["", "has space", &"x".repeat(129)] {
        let e = c
            .fails(
                "vibe_send",
                json!({"agent": "slow", "text": "five", "request_id": bad}),
            )
            .await;
        assert!(e.contains("request_id must be"), "{e}");
    }
    assert_eq!(fake.request_ids.lock().unwrap().len(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sends_route_by_ref_and_type_text_or_keys() {
    let (fake, mut c) = client().await;
    let r = c
        .call("vibe_send", json!({"agent": "slow", "text": "go on"}))
        .await;
    assert_eq!(
        text(&r),
        "sent to slow and submitted. Call vibe_wait to know when it has answered"
    );
    let r = c
        .call(
            "vibe_send",
            json!({"agent": "remote-only", "text": "draft", "submit": false}),
        )
        .await;
    assert!(
        text(&r).starts_with("pasted into vps:remote-only, not submitted"),
        "{}",
        text(&r)
    );
    let r = c
        .call(
            "vibe_send",
            json!({"agent": "vps:quiet-amber-otter", "keys": ["2", "Down", "Enter", "Ctrl+C"]}),
        )
        .await;
    assert!(
        text(&r).starts_with("pressed 2 Down Enter Ctrl+C in vps:quiet-amber-otter"),
        "{}",
        text(&r)
    );
    // VibeOS's name for the argument works too.
    let r = c
        .call("vibe_send", json!({"window": "sleepy", "keys": ["y"]}))
        .await;
    assert!(text(&r).contains("(woke it first)"), "{}", text(&r));
    assert_eq!(r["structuredContent"]["woke"], true);
    let sends = fake.sends.lock().unwrap().clone();
    let t = |h: &str, a: &str| Target {
        host: h.into(),
        agent: a.into(),
    };
    assert_eq!(
        sends,
        [
            (
                t("local", "slow"),
                Input::Text {
                    text: "go on".into(),
                    submit: true,
                    force: false
                }
            ),
            (
                t("vps", "remote-only"),
                Input::Text {
                    text: "draft".into(),
                    submit: false,
                    force: false
                }
            ),
            (
                t("vps", "quiet-amber-otter"),
                Input::Keys(vec![
                    "2".into(),
                    "Down".into(),
                    "Enter".into(),
                    "Ctrl+C".into()
                ])
            ),
            (t("local", "sleepy"), Input::Keys(vec!["y".into()])),
        ]
    );
    for (args, expect) in [
        (
            json!({"agent": "slow"}),
            "give text to send, or keys to press",
        ),
        (
            json!({"agent": "slow", "text": "a", "keys": ["b"]}),
            "give text or keys, not both",
        ),
        (
            json!({"agent": "slow", "keys": []}),
            "keys must be a list of keys, like [\"2\"]",
        ),
        (
            json!({"agent": "slow", "keys": ["Enter"], "submit": true}),
            "submit goes with text; press [\"Enter\"] with keys",
        ),
        (
            json!({"text": "a"}),
            "agent is required: a ref, like quiet-amber-otter",
        ),
        (json!({"agent": "slow", "text": 3}), "text must be a string"),
        (
            json!({"agent": "slow", "text": "a", "submit": "yes"}),
            "submit must be true or false",
        ),
    ] {
        assert_eq!(c.fails("vibe_send", args.clone()).await, expect, "{args}");
    }
    let e = c
        .fails("vibe_send", json!({"agent": "slow", "keys": ["F13"]}))
        .await;
    assert!(e.starts_with("unknown key \"F13\""), "{e}");
    // A bare ref on this host and another is this host's; one on none is the
    // core's not-found, with where to look.
    c.call("vibe_read_output", json!({"agent": "quiet-amber-otter"}))
        .await;
    let read = fake.reads.lock().unwrap().last().unwrap().0.clone();
    assert_eq!(read.host, "local");
    let e = c
        .fails("vibe_send", json!({"agent": "ghost", "text": "x"}))
        .await;
    assert_eq!(
        e,
        "no agent called ghost on local. vibe_list_agents lists the agents and their refs."
    );
    assert_eq!(fake.sends.lock().unwrap().len(), 4, "nothing more was sent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wait_reports_progress_until_the_answer() {
    let (fake, mut c) = client().await;
    fake.busy_for.lock().unwrap().insert("slow".into(), 3);
    *fake.screen.lock().unwrap() = "line a\nline b\n❯".into();
    let r = c
        .call_raw("vibe_wait", json!({"agents": "slow", "lines": 2}))
        .await;
    // Progress comes on the same response, as an event stream.
    assert_eq!(
        r.header("content-type"),
        Some("text/event-stream"),
        "{}",
        r.body
    );
    let progress = r.progress();
    assert!(!progress.is_empty(), "{}", r.body);
    assert_eq!(
        progress[0]["params"]["progressToken"],
        json!(format!("p{}", 3))
    );
    assert!(
        progress[0]["params"]["message"]
            .as_str()
            .unwrap()
            .starts_with("slow: busy (thinking), $ cargo test"),
        "{:?}",
        progress[0]
    );
    let result = &r.response()["result"];
    let t = text(result);
    assert!(t.starts_with("slow has answered: it is idle after"), "{t}");
    assert!(t.ends_with("\n\nline b\n❯"), "{t}");
    let s = &result["structuredContent"];
    assert_eq!(
        (s["reached"].clone(), s["state"].clone()),
        (json!(true), json!("idle"))
    );
    assert!(s["waited"].as_f64().unwrap() >= 0.5);

    // Of several, the first to answer ends it.
    fake.busy_for
        .lock()
        .unwrap()
        .insert("remote-only".into(), 0);
    let r = c
        .call(
            "vibe_wait",
            json!({"agents": ["never", "remote-only"], "lines": 0}),
        )
        .await;
    assert_eq!(r["structuredContent"]["ref"], "vps:remote-only");
    assert!(
        text(&r).contains("\nnever: busy (thinking)"),
        "{}",
        text(&r)
    );

    // One asleep won't answer until a send wakes it.
    let r = c.call("vibe_wait", json!({"agents": "sleepy"})).await;
    assert!(
        text(&r).starts_with("sleepy is asleep: vibe_send wakes it"),
        "{}",
        text(&r)
    );
    assert_eq!(r["structuredContent"]["reached"], false);

    // A timeout says it's still at work.
    let r = c
        .call(
            "vibe_wait",
            json!({"agents": "never", "timeout": 1, "lines": 0}),
        )
        .await;
    assert!(
        text(&r).starts_with("still busy after 1s; call vibe_wait again"),
        "{}",
        text(&r)
    );
    for (args, expect) in [
        (
            json!({}),
            "agents is required: one ref, or a list of up to 32",
        ),
        (
            json!({"agents": []}),
            "agents is required: one ref, or a list of up to 32",
        ),
        (
            json!({"agents": "never", "timeout": 241}),
            "timeout must be a whole number from 1 to 240",
        ),
        (
            json!({"agents": "never", "timeout": 0}),
            "timeout must be a whole number from 1 to 240",
        ),
    ] {
        assert_eq!(c.fails("vibe_wait", args.clone()).await, expect, "{args}");
    }
    let e = c.fails("vibe_wait", json!({"agents": "ghost"})).await;
    assert!(e.contains("vibe_list_agents lists"), "{e}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_wait_after_a_turn_needs_no_answer() {
    let (fake, mut c) = client().await;
    fake.states
        .lock()
        .unwrap()
        .insert("never".into(), AgentState::Question);
    let r = c
        .call(
            "vibe_wait",
            json!({"agents": "never", "turn": false, "lines": 0}),
        )
        .await;
    assert_eq!(r["structuredContent"]["reached"], true);
    assert_eq!(r["structuredContent"]["state"], "question");
    assert!(text(&r).contains("It is asking something"), "{}", text(&r));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads_are_cut_to_keep_the_end() {
    let (fake, mut c) = client().await;
    let big: String = (0..5000).map(|i| format!("row {i:05} ......\n")).collect();
    *fake.screen.lock().unwrap() = big.clone() + "the end";
    let r = c
        .call("vibe_read_output", json!({"agent": "slow", "lines": 2000}))
        .await;
    let t = text(&r);
    assert!(t.chars().count() <= MAX_RESULT_CHARS + 200, "{}", t.len());
    assert!(t.contains("earlier characters cut"), "{}", &t[..200]);
    assert!(t.ends_with("the end"));
    let s = r["structuredContent"]["text"].as_str().unwrap();
    assert!(s.chars().count() <= MAX_RESULT_CHARS + 200);
    assert!(s.ends_with("the end"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_from_the_bottom_or_a_cursor() {
    let (fake, mut c) = client().await;
    *fake.screen.lock().unwrap() = "a\nb\nc".into();
    let r = c
        .call("vibe_read_output", json!({"agent": "slow", "lines": 3}))
        .await;
    let t = text(&r);
    assert!(t.starts_with("slow is busy. cursor 7\n\n"), "{t}");
    assert!(t.ends_with("\n\na\nb\nc"), "{t}");
    let r = c.call("vibe_read_output", json!({"agent": "slow"})).await;
    assert!(text(&r).ends_with("old 1\nold 2\na\nb\nc"), "{}", text(&r));
    let r = c
        .call("vibe_read_output", json!({"agent": "slow", "after": 0}))
        .await;
    let t = text(&r);
    assert!(t.contains("cursor 7 (5 lines were no longer held)"), "{t}");
    assert!(
        t.contains("new since 0:\nold 1\nold 2\n\nscreen:\na\nb\nc"),
        "{t}"
    );
    let r = c.call("vibe_read_output", json!({"agent": "sleepy"})).await;
    assert!(
        text(&r).contains("This is the last screen saved"),
        "{}",
        text(&r)
    );
    assert_eq!(r["structuredContent"]["live"], false);
    assert_eq!(r["structuredContent"]["cursor"], 7);
    let reads = fake.reads.lock().unwrap().clone();
    assert_eq!(reads[2].1, Some(0));
    let e = c
        .fails("vibe_read_output", json!({"agent": "slow", "lines": 0}))
        .await;
    assert_eq!(e, "lines must be a whole number from 1 to 2000");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn messages_and_deletes() {
    let (fake, mut c) = client().await;
    let r = c
        .call("vibe_read_messages", json!({"agent": "slow", "last": 3}))
        .await;
    assert_eq!(
        text(&r),
        "user: fix it\n\n→ Bash(cargo test)\n\nassistant: Fixed."
    );
    let m = &r["structuredContent"]["messages"];
    assert_eq!(
        m[2],
        json!({"role": "assistant", "text": "Fixed.", "at": null})
    );
    c.call("vibe_read_messages", json!({"agent": "slow"})).await;
    assert_eq!(*fake.message_reads.lock().unwrap(), [3, 10]);
    let e = c
        .fails("vibe_read_messages", json!({"agent": "slow", "last": 501}))
        .await;
    assert_eq!(e, "last must be a whole number from 1 to 500");

    let e = c.fails("vibe_delete_agent", json!({"agent": "slow"})).await;
    assert!(
        e.contains("Commit and push it, or delete with force"),
        "{e}"
    );
    let r = c
        .call("vibe_delete_agent", json!({"agent": "slow", "force": true}))
        .await;
    assert_eq!(text(&r), "deleted slow; its branch slow is kept");
    let r = c
        .call("vibe_delete_agent", json!({"agent": "vps:remote-only"}))
        .await;
    assert_eq!(r["structuredContent"]["deleted"], "vps:remote-only");
    let d = fake.deletes.lock().unwrap().clone();
    assert_eq!(d[1].force, Some(true));
    assert_eq!(d[2].host.as_deref(), Some("vps"));
    // An unknown tool is the protocol's error, not a tool's.
    let r = c.call_raw("vibe_open_window", json!({})).await;
    assert!(r.response()["error"].is_object(), "{}", r.body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listeners_follow_the_settings() {
    let fake = Arc::new(Fake::default());
    let free = || {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    };
    let port = Arc::new(Mutex::new(free()));
    let p = Arc::clone(&port);
    let settings: Arc<vibe_mcp::http::Settings> =
        Arc::new(move || vibe_mcp::net::listen_on(*p.lock().unwrap(), vec![]));
    let kick = Arc::new(tokio::sync::Notify::new());
    let task = vibe_mcp::http::spawn_listeners(
        Arc::clone(&fake),
        settings,
        Duration::from_secs(3600),
        Arc::clone(&kick),
    );
    let reach = |port: u16| async move {
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    };
    let first = *port.lock().unwrap();
    assert!(reach(first).await);
    let r = post_raw(
        format!("127.0.0.1:{first}").parse().unwrap(),
        "/mcp",
        &[("Authorization", &format!("Bearer {KEY}"))],
        &initialize(),
    )
    .await;
    assert_eq!(r.status, 200);
    // New settings, and a kick: served there at once, and no longer on the
    // old port.
    let second = free();
    *port.lock().unwrap() = second;
    // Checked hourly, so only the kick makes it look again now.
    kick.notify_one();
    assert!(reach(second).await);
    let gone = async {
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(("127.0.0.1", first))
                .await
                .is_err()
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    };
    assert!(gone.await);
    task.abort();
    let _ = task.await;
    let stopped = async {
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(("127.0.0.1", second))
                .await
                .is_err()
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    };
    assert!(stopped.await, "aborting the supervisor stops its listeners");
    let log = fake.log.lock().unwrap().join("\n");
    assert!(
        log.contains(&format!("mcp: serving http://127.0.0.1:{first}/mcp")),
        "{log}"
    );
    assert!(
        log.contains(&format!(
            "mcp: stopped serving http://127.0.0.1:{first}/mcp"
        )),
        "{log}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wait_rides_out_a_link_that_drops_for_a_moment() {
    let (fake, mut c) = client().await;
    fake.busy_for.lock().unwrap().insert("slow".into(), 6);
    // Checks 2 to 4 find the host's link down, as a network blip would.
    fake.link_down.lock().unwrap().insert("slow".into(), 2..=4);
    let r = text(
        &c.call("vibe_wait", json!({"agents": "slow", "lines": 0}))
            .await,
    );
    assert!(r.starts_with("slow has answered"), "{r}");
    // One down the whole time is said to be unavailable when the wait ends.
    fake.checks.lock().unwrap().clear();
    fake.link_down
        .lock()
        .unwrap()
        .insert("slow".into(), 1..=u32::MAX);
    let e = c
        .fails(
            "vibe_wait",
            json!({"agents": "slow", "lines": 0, "timeout": 1}),
        )
        .await;
    assert!(e.contains("unavailable"), "{e}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waits_for_what_until_asks() {
    let (fake, mut c) = client().await;
    // A permission prompt is an answer, said to be maybe a silent command.
    fake.busy_for.lock().unwrap().insert("slow".into(), 0);
    fake.activities
        .lock()
        .unwrap()
        .insert("slow".into(), Activity::AwaitingApproval);
    fake.states
        .lock()
        .unwrap()
        .insert("slow".into(), AgentState::Question);
    let r = c
        .call(
            "vibe_wait",
            json!({"agents": "slow", "lines": 0, "background": true}),
        )
        .await;
    assert_eq!(r["structuredContent"]["reached"], true);
    assert!(
        text(&r).contains("It may be waiting on a permission prompt"),
        "{}",
        text(&r)
    );
    assert!(r["structuredContent"]["waitedMs"].is_number());
    assert!(
        fake.backgrounds.lock().unwrap().iter().all(|b| *b),
        "background reaches the daemon"
    );
    // until: question ends on the question; idle doesn't.
    let r = c
        .call(
            "vibe_wait",
            json!({"agents": "slow", "until": "question", "lines": 0}),
        )
        .await;
    assert_eq!(r["structuredContent"]["reached"], true);
    assert!(
        !fake.backgrounds.lock().unwrap().last().unwrap(),
        "background is off by default"
    );
    let r = c
        .call(
            "vibe_wait",
            json!({"agents": "slow", "until": "idle", "timeout": 1, "lines": 0}),
        )
        .await;
    assert_eq!(r["structuredContent"]["reached"], false, "{}", text(&r));
    // until: change ends on any change of state or activity.
    fake.busy_for.lock().unwrap().insert("never".into(), 2);
    let r = c
        .call(
            "vibe_wait",
            json!({"agents": "never", "until": "change", "turn": false, "lines": 0}),
        )
        .await;
    assert_eq!(r["structuredContent"]["reached"], true);
    assert_eq!(r["structuredContent"]["activity"], "done");
    let e = c
        .fails("vibe_wait", json!({"agents": "never", "until": "soon"}))
        .await;
    assert_eq!(e, "until must be settled, idle, question or change");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reading_on_from_a_cursor_leaves_what_doesnt_fit_for_next_time() {
    let (fake, mut c) = client().await;
    *fake.screen.lock().unwrap() = "$".into();
    // 10,000 new lines, far more than fit.
    let lines: Vec<String> = (0..10_000).map(|i| format!("line {i:05}")).collect();
    *fake.history.lock().unwrap() = Some(lines);
    let r = c
        .call("vibe_read_output", json!({"agent": "slow", "after": 0}))
        .await;
    let t = text(&r);
    assert!(
        t.chars().count() <= MAX_RESULT_CHARS,
        "{}",
        t.chars().count()
    );
    assert!(
        t.contains("new since 0:\nline 00000\nline 00001"),
        "the oldest come first"
    );
    assert!(t.ends_with("screen:\n$"), "and the screen last");
    assert!(t.contains("more history waits"), "{}", &t[..200]);
    let kept = t.lines().filter(|l| l.starts_with("line ")).count() as i64;
    assert!(kept > 1000 && kept < 10_000, "{kept}");
    // The cursor follows the last line given, so nothing is skipped.
    let s = &r["structuredContent"];
    assert_eq!(s["cursor"], kept);
    assert_eq!(s["more"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tailscale_listener_takes_any_host_but_no_browser() {
    let fake = Arc::new(Fake::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(vibe_mcp::http::serve(listener, fake, false));
    let auth = format!("Bearer {KEY}");
    let host = ("Host", "mac-mini.tail1234.ts.net:7878");
    let r = post_raw(
        addr,
        "/mcp",
        &[("Authorization", &auth), host],
        &initialize(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = post_raw(
        addr,
        "/mcp",
        &[
            ("Authorization", &auth),
            host,
            ("Origin", "https://evil.example"),
        ],
        &initialize(),
    )
    .await;
    assert_eq!(r.status, 403);
    let r = post_raw(addr, "/mcp", &[host], &initialize()).await;
    assert_eq!(r.status, 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_2026_client_needs_no_initialize() {
    let fake = Arc::new(Fake::default());
    let addr = serve(Arc::clone(&fake)).await;
    let auth = format!("Bearer {KEY}");
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "t", "version": "1"},
        "io.modelcontextprotocol/clientCapabilities": {},
    });
    let headers = [
        ("Authorization", auth.as_str()),
        ("MCP-Protocol-Version", "2026-07-28"),
        ("Mcp-Method", "tools/call"),
        ("Mcp-Name", "vibe_list_agents"),
    ];
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
        "name": "vibe_list_agents", "arguments": {"host": "vps"}, "_meta": meta}});
    let r = post_raw(addr, "/mcp", &headers, &body.to_string()).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let t = text(&r.response()["result"]);
    assert!(t.starts_with("vps:quiet-amber-otter"), "{t}");
}

/// A request refused before it's served is still read whole before the
/// answer: closing a socket with a request still unread in it resets the
/// connection, and the client may never see the answer. macOS showed it
/// with a small body sent in a second write; a body far larger than one
/// read shows it everywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_request_is_read_before_it_is_answered() {
    let fake = Arc::new(Fake::default());
    let addr = serve(Arc::clone(&fake)).await;
    let pad = "x".repeat(2 << 20);
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"pad": pad}})
        .to_string();
    let good = format!("Bearer {KEY}");
    for (path, auth, status) in [
        ("/mcp", None, 401),
        ("/mcp", Some("Bearer vk_wrong"), 401),
        ("/elsewhere", Some(good.as_str()), 404),
    ] {
        let headers: Vec<(&str, &str)> = auth.iter().map(|a| ("Authorization", *a)).collect();
        let r = post_raw(addr, path, &headers, &body).await;
        assert_eq!(r.status, status, "{path} {auth:?}: {}", r.body);
    }
    // rmcp's own refusals, of a browser and of a rebound host, too.
    for (name, value) in [("Origin", "https://evil.example"), ("Host", "evil.example")] {
        let r = post_raw(
            addr,
            "/mcp",
            &[("Authorization", &good), (name, value)],
            &body,
        )
        .await;
        assert!(
            (400..500).contains(&r.status),
            "{name}: {} {}",
            r.status,
            r.body
        );
    }
    // A small body sent after its head, as macOS split it.
    let r = post_raw(addr, "/mcp", &[], "{}").await;
    assert_eq!(r.status, 401);
    // One past the limit is refused as too large.
    let huge = "y".repeat(5 << 20);
    let r = post_raw(addr, "/mcp", &[("Authorization", &good)], &huge).await;
    assert_eq!(r.status, 413, "{}", r.body);
}

// MCP Events (SEP-3415), by webhook, as ChatGPT has them.

use receiver::{Echo, Receiver, secret, signed_with};

const EVENTS: &str = vibe_mcp::events::EXTENSION;

const WAIT: Duration = Duration::from_secs(10);

/// Webhooks that try again quickly, and let a subscription last a second.
fn quick(path: Option<std::path::PathBuf>) -> webhook::Config {
    webhook::Config {
        path,
        retries: vec![Duration::from_millis(50), Duration::from_millis(100)],
        timeout: Duration::from_secs(2),
        min_ttl: Duration::from_secs(1),
        sweep_every: Duration::from_millis(100),
        ..webhook::Config::default()
    }
}

/// A fake with webhooks made with `config`, served, and a client of it that's
/// initialized as ChatGPT is.
async fn events_fake(config: webhook::Config) -> (Arc<Fake>, Client) {
    let fake = Arc::new(Fake::default());
    *fake.hooks_config.lock().unwrap() = Some(config);
    let addr = serve(Arc::clone(&fake)).await;
    let mut c = Client::new(addr, KEY);
    c.initialize("2025-03-26").await;
    (fake, c)
}

fn hooks(fake: &Fake) -> Arc<Webhooks> {
    fake.webhooks().unwrap()
}

fn core_event(kind: EventKind, host: &str, agent: &str, state: Option<&str>) -> Event {
    Event {
        seq: 0,
        at: "2026-11-25T12:00:00Z".into(),
        host: host.into(),
        kind,
        project: Some("app".into()),
        agent: (!agent.is_empty()).then(|| agent.into()),
        state: state.map(str::to_owned),
        limit: None,
        message: None,
        status: None,
    }
}

fn changed(host: &str, agent: &str, state: &str) -> Event {
    core_event(EventKind::AgentState, host, agent, Some(state))
}

/// A request's result, or its error: one of them.
async fn ask(c: &mut Client, method: &str, params: Value) -> Result<Value, Value> {
    let r = c
        .post(&json!({"jsonrpc": "2.0", "id": 5, "method": method, "params": params}))
        .await;
    assert_eq!(r.status, 200, "{method}: {}", r.body);
    let m = r.response().clone();
    match m.get("error") {
        Some(e) => Err(e.clone()),
        None => Ok(m["result"].clone()),
    }
}

fn subscription(name: &str, args: Value, url: &str, secret: &str) -> Value {
    json!({"name": name, "arguments": args, "cursor": null,
        "delivery": {"mode": "webhook", "url": url, "secret": secret}})
}

async fn subscribe(c: &mut Client, params: Value) -> Value {
    ask(c, "events/subscribe", params)
        .await
        .unwrap_or_else(|e| panic!("subscribe: {e}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_are_advertised_where_chatgpt_looks() {
    let fake = Arc::new(Fake::default());
    let addr = serve(Arc::clone(&fake)).await;
    // `initialize`, as ChatGPT sends it: `capabilities.events`.
    let mut c = Client::new(addr, KEY);
    let info = c.initialize("2025-03-26").await;
    assert_eq!(info["capabilities"]["events"], json!({}), "{info}");
    assert!(info["capabilities"]["tools"].is_object());
    // And on 2026-07-28's `server/discover`, beside the extension.
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "t", "version": "1"},
        "io.modelcontextprotocol/clientCapabilities": {},
    });
    let discover =
        json!({"jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {"_meta": meta}});
    let auth = format!("Bearer {KEY}");
    let headers = [
        ("Authorization", auth.as_str()),
        ("MCP-Protocol-Version", "2026-07-28"),
        ("Mcp-Method", "server/discover"),
    ];
    let r = post_raw(addr, "/mcp", &headers, &discover.to_string()).await;
    let caps = &r.response()["result"]["capabilities"];
    assert_eq!(caps["events"], json!({}), "{}", r.body);
    assert_eq!(caps["extensions"][EVENTS], json!({}), "{}", r.body);
    // events/list in a 2025 session: webhook only.
    let list = ask(&mut c, "events/list", json!({})).await.unwrap();
    let events = list["events"].as_array().unwrap();
    let names: Vec<&str> = events.iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "agent.started",
            "agent.state_changed",
            "agent.turn_ended",
            "agent.deleted"
        ]
    );
    assert!(events.iter().all(|e| e["delivery"] == json!(["webhook"])));
    // A daemon that offers no events says nothing of them.
    fake.no_events.store(true, Ordering::SeqCst);
    let mut old = Client::new(addr, KEY);
    let info = old.initialize("2025-03-26").await;
    assert!(info["capabilities"].get("events").is_none(), "{info}");
    let e = ask(&mut old, "events/list", json!({})).await.unwrap_err();
    assert_eq!(e["code"], -32601);
    fake.no_events.store(false, Ordering::SeqCst);
    // And nothing without a key.
    let r = post_raw(
        addr,
        "/mcp",
        &[("MCP-Protocol-Version", "2025-03-26")],
        &json!({"jsonrpc": "2.0", "id": 1, "method": "events/list"}).to_string(),
    )
    .await;
    assert_eq!(r.status, 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_callback_is_verified_then_gets_each_event_signed() {
    let (fake, mut c) = events_fake(quick(None)).await;
    let hook = Receiver::start().await;
    let url = hook.url("/hooks/chat-1");
    let s1 = secret(1);
    let sub = subscribe(
        &mut c,
        subscription(
            "agent.state_changed",
            json!({"host": "local", "project": "app"}),
            &url,
            &s1,
        ),
    )
    .await;
    let id = sub["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("sub_"), "{sub}");
    assert_eq!(sub["truncated"], false);
    assert!(sub["cursor"].is_string());
    let before = vibe_core::time::parse_rfc3339(sub["refreshBefore"].as_str().unwrap()).unwrap();
    let left = before - vibe_core::time::now();
    assert!(
        (23 * 3600..=24 * 3600).contains(&left),
        "a day by default: {left}"
    );
    assert_eq!(*fake.subscribers.lock().unwrap(), ["laptop"]);
    // Verified first, signed, with a single-use challenge.
    let v = hook.verifications();
    assert_eq!(v.len(), 1);
    assert!(v[0].header("webhook-id").starts_with("msg_verification_"));
    assert_eq!(v[0].header("x-mcp-subscription-id"), id);
    assert!(signed_with(&s1, &v[0]));
    assert!(v[0].json["challenge"].as_str().unwrap().len() >= 32);
    // What matches is posted, signed; what doesn't isn't.
    let hub = Arc::clone(hooks(&fake).hub());
    hub.ingest(&changed("vps", "b", "idle"));
    hub.ingest(&changed("local", "a", "idle"));
    let got = hook.wait_events(1, WAIT).await;
    let p = &got[0];
    assert_eq!(p.json["name"], "agent.state_changed");
    assert_eq!(
        p.json["data"],
        json!({"ref": "a", "host": "local", "agent": "a", "project": "app", "state": "idle", "previous": null})
    );
    assert_eq!(p.json["timestamp"], "2026-11-25T12:00:00Z");
    assert!(p.json["cursor"].is_string());
    assert_eq!(p.header("webhook-id"), p.json["eventId"].as_str().unwrap());
    assert_eq!(p.header("x-mcp-subscription-id"), id);
    assert_eq!(p.header("content-type"), "application/json");
    assert!(signed_with(&s1, p), "{:?}", p.headers);
    assert!(!signed_with(&secret(9), p));
    // Subscribing again, its arguments in another order, is a refresh: the
    // same subscription, no second challenge, and how delivery's going.
    let again = subscribe(
        &mut c,
        subscription(
            "agent.state_changed",
            json!({"project": "app", "host": "local"}),
            &url,
            &s1,
        ),
    )
    .await;
    assert_eq!(again["id"], id.as_str());
    assert_eq!(again["deliveryStatus"]["active"], true);
    assert!(
        again["deliveryStatus"]["lastDeliveryAt"].is_string(),
        "{again}"
    );
    assert_eq!(hook.verifications().len(), 1);
    assert_eq!(hooks(&fake).len(), 1);
    // One more event is one more post.
    hub.ingest(&changed("local", "a", "busy"));
    let got = hook.wait_events(2, WAIT).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(hook.events().len(), 2);
    assert_ne!(got[0].json["eventId"], got[1].json["eventId"]);
    // Nothing of the URL's path reaches the log.
    let log = fake.log.lock().unwrap().join("\n");
    assert!(
        log.contains("subscribed to agent.state_changed by webhook to 127.0.0.1"),
        "{log}"
    );
    assert!(!log.contains("chat-1"), "{log}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_callback_that_fails_verification_says_why_and_gets_nothing() {
    let mut config = quick(None);
    config.timeout = Duration::from_millis(500);
    let (fake, mut c) = events_fake(config).await;
    let hook = Receiver::start().await;
    let url = hook.url("/h");
    let reason = |e: &Value| {
        assert_eq!(e["code"], -32015, "{e}");
        e["data"]["reason"].as_str().unwrap().to_owned()
    };
    for (echo, want) in [
        (Echo::Wrong, "challenge_failed"),
        (Echo::Status(404), "http_4xx"),
        (Echo::Status(503), "http_5xx"),
        (Echo::Status(302), "http_4xx"),
        (Echo::Hang, "timeout"),
    ] {
        hook.echo(echo);
        let e = ask(
            &mut c,
            "events/subscribe",
            subscription("agent.started", json!({}), &url, &secret(1)),
        )
        .await
        .unwrap_err();
        assert_eq!(reason(&e), want, "{echo:?}");
    }
    // Nothing listens there.
    let closed = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    let e = ask(
        &mut c,
        "events/subscribe",
        subscription(
            "agent.started",
            json!({}),
            &format!("http://{closed}/h"),
            &secret(1),
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(reason(&e), "connection_refused");
    assert!(hooks(&fake).is_empty());
    hooks(&fake).hub().ingest(&core_event(
        EventKind::AgentAdded,
        "local",
        "a",
        Some("busy"),
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(hook.events().is_empty());
    // What can't be a subscription is refused before anything's posted.
    let before = hook.posts().len();
    let refused = |url: &str, secret: &str| subscription("agent.started", json!({}), url, secret);
    for (params, code) in [
        (refused("http://receiver.example.com/h", &secret(1)), -32602),
        (refused("https://10.0.0.1/h", &secret(1)), -32602),
        (
            refused("https://169.254.169.254/latest", &secret(1)),
            -32602,
        ),
        (refused(&url, "whsec_c2hvcnQ="), -32602),
        (refused(&url, "not-whsec"), -32602),
        (
            json!({"name": "agent.started", "delivery": {"mode": "email"}}),
            -32026,
        ),
        (
            json!({"name": "agent.exploded", "delivery": {"mode": "webhook", "url": url, "secret": secret(1)}}),
            -32023,
        ),
        (
            json!({"name": "agent.started", "arguments": {"colour": "red"},
                "delivery": {"mode": "webhook", "url": url, "secret": secret(1)}}),
            -32602,
        ),
        (json!({"name": "agent.started"}), -32602),
    ] {
        let e = ask(&mut c, "events/subscribe", params.clone())
            .await
            .unwrap_err();
        assert_eq!(e["code"], code, "{params}: {e}");
    }
    assert_eq!(hook.posts().len(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deliveries_are_tried_again_but_never_after_410_or_413() {
    let (fake, mut c) = events_fake(quick(None)).await;
    let hook = Receiver::start().await;
    let s1 = secret(1);
    subscribe(
        &mut c,
        subscription("agent.state_changed", json!({}), &hook.url("/h"), &s1),
    )
    .await;
    let hub = Arc::clone(hooks(&fake).hub());
    // Twice unavailable, then taken: three tries of the same message.
    hook.answer(&[503, 500]);
    hub.ingest(&changed("local", "a", "idle"));
    let got = hook.wait_events(3, WAIT).await;
    let ids: HashSet<&str> = got.iter().map(|p| p.header("webhook-id")).collect();
    assert_eq!(ids.len(), 1, "one message, tried again");
    assert_eq!(
        got.iter().map(|p| p.status).collect::<Vec<_>>(),
        [503, 500, 200]
    );
    assert!(got.iter().all(|p| signed_with(&s1, p)));
    // Gone, or too large: not tried again.
    for status in [410, 413] {
        let n = hook.events().len();
        hook.answer(&[status]);
        hub.ingest(&changed(
            "local",
            "a",
            if status == 410 { "busy" } else { "idle" },
        ));
        hook.wait_events(n + 1, WAIT).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(hook.events().len(), n + 1, "{status} was tried again");
    }
    // Failing every time, it's given up after the last try.
    let n = hook.events().len();
    hook.answer(&[500, 500, 500, 500]);
    hub.ingest(&changed("local", "a", "busy"));
    hook.wait_events(n + 3, WAIT).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(hook.events().len(), n + 3);
    let log = fake.log.lock().unwrap().join("\n");
    assert!(log.contains("gave up delivering an event"), "{log}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_subscription_lasts_what_was_granted_unless_refreshed() {
    let (fake, mut c) = events_fake(quick(None)).await;
    let hook = Receiver::start().await;
    let url = hook.url("/h");
    let mut params = subscription("agent.state_changed", json!({}), &url, &secret(1));
    let grant = |r: &Value| {
        vibe_core::time::parse_rfc3339(r["refreshBefore"].as_str().unwrap()).unwrap()
            - vibe_core::time::now()
    };
    // At most what was asked, but at least a floor.
    params["ttlMs"] = json!(10);
    let r = subscribe(&mut c, params.clone()).await;
    assert!(grant(&r) <= 1, "{r}");
    // No expiry asked for is the longest there is.
    params["ttlMs"] = Value::Null;
    let r = subscribe(&mut c, params.clone()).await;
    assert!(grant(&r) > 29 * 24 * 3600, "{r}");
    // Refreshed to two seconds, it delivers; left, it ends.
    params["ttlMs"] = json!(2000);
    subscribe(&mut c, params.clone()).await;
    let hub = Arc::clone(hooks(&fake).hub());
    hub.ingest(&changed("local", "a", "idle"));
    hook.wait_events(1, WAIT).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    subscribe(&mut c, params.clone()).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    hub.ingest(&changed("local", "a", "busy"));
    hook.wait_events(2, WAIT).await;
    tokio::time::sleep(Duration::from_millis(2200)).await;
    assert!(hooks(&fake).is_empty(), "it expired");
    hub.ingest(&changed("local", "a", "idle"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(hook.events().len(), 2);
    assert!(
        fake.log
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("expired"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsubscribing_stops_delivery_and_can_be_done_twice() {
    let (fake, mut c) = events_fake(quick(None)).await;
    let hook = Receiver::start().await;
    let url = hook.url("/h");
    let args = json!({"agents": ["a", "vps:b"]});
    subscribe(
        &mut c,
        subscription("agent.deleted", args.clone(), &url, &secret(1)),
    )
    .await;
    // Another key's subscription to the same is its own.
    let mut phone = Client::new(c.addr, &key2());
    phone.initialize("2025-03-26").await;
    subscribe(
        &mut phone,
        subscription("agent.deleted", args.clone(), &url, &secret(2)),
    )
    .await;
    assert_eq!(hooks(&fake).len(), 2);
    let gone = json!({"name": "agent.deleted", "arguments": {"agents": ["a", "vps:b"]},
        "delivery": {"url": url}});
    for _ in 0..2 {
        let r = ask(&mut c, "events/unsubscribe", gone.clone())
            .await
            .unwrap();
        assert_eq!(r, json!({"resultType": "complete"}));
    }
    assert_eq!(hooks(&fake).len(), 1, "the phone's is left");
    hooks(&fake)
        .hub()
        .ingest(&core_event(EventKind::AgentDeleted, "local", "a", None));
    let got = hook.wait_events(1, WAIT).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(hook.events().len(), 1);
    assert!(signed_with(&secret(2), &got[0]), "the phone's");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deleted_key_ends_its_subscriptions() {
    let (fake, mut c) = events_fake(quick(None)).await;
    let hook = Receiver::start().await;
    subscribe(
        &mut c,
        subscription("agent.started", json!({}), &hook.url("/h"), &secret(1)),
    )
    .await;
    let hub = Arc::clone(hooks(&fake).hub());
    fake.revoked.store(true, Ordering::SeqCst);
    hub.ingest(&core_event(EventKind::KeysChanged, "local", "", None));
    let end = Instant::now() + WAIT;
    while !hooks(&fake).is_empty() {
        assert!(Instant::now() < end, "the subscription outlived its key");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    hub.ingest(&core_event(
        EventKind::AgentAdded,
        "local",
        "a",
        Some("busy"),
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(hook.events().is_empty());
    let log = fake.log.lock().unwrap().join("\n");
    assert!(
        log.contains("key \"laptop\" is gone: ended its 1 webhook subscription"),
        "{log}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscriptions_outlast_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp-webhooks.json");
    let hook = Receiver::start().await;
    let s1 = secret(1);
    let (fake, mut c) = events_fake(quick(Some(path.clone()))).await;
    let sub = subscribe(
        &mut c,
        subscription("agent.started", json!({}), &hook.url("/h"), &s1),
    )
    .await;
    // Kept privately, with no key in it.
    let kept = std::fs::read_to_string(&path).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        !kept.contains(KEY) && kept.contains(&vibe_core::keys::hash(KEY)),
        "{kept}"
    );
    hooks(&fake).stop();
    // Another daemon, as after a restart: it delivers with no subscribe.
    let (fake2, mut c2) = events_fake(quick(Some(path.clone()))).await;
    assert_eq!(hooks(&fake2).len(), 1);
    hooks(&fake2).hub().ingest(&core_event(
        EventKind::AgentAdded,
        "local",
        "a",
        Some("busy"),
    ));
    let got = hook.wait_events(1, WAIT).await;
    assert!(signed_with(&s1, &got[0]));
    assert_eq!(
        got[0].header("x-mcp-subscription-id"),
        sub["id"].as_str().unwrap()
    );
    // Its refresh needs no verifying again, and says what it may have missed.
    let mut params = subscription("agent.started", json!({}), &hook.url("/h"), &s1);
    params["cursor"] = sub["cursor"].clone();
    let again = subscribe(&mut c2, params).await;
    assert_eq!(again["id"], sub["id"]);
    assert_eq!(again["truncated"], true);
    assert_eq!(hook.verifications().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_secret_signs_beside_the_old_for_a_while() {
    let (fake, mut c) = events_fake(quick(None)).await;
    let hook = Receiver::start().await;
    let (s1, s2) = (secret(1), secret(2));
    subscribe(
        &mut c,
        subscription("agent.started", json!({}), &hook.url("/h"), &s1),
    )
    .await;
    subscribe(
        &mut c,
        subscription("agent.started", json!({}), &hook.url("/h"), &s2),
    )
    .await;
    hooks(&fake).hub().ingest(&core_event(
        EventKind::AgentAdded,
        "local",
        "a",
        Some("busy"),
    ));
    let got = hook.wait_events(1, WAIT).await;
    assert_eq!(got[0].header("webhook-signature").split(' ').count(), 2);
    assert!(signed_with(&s1, &got[0]) && signed_with(&s2, &got[0]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscriptions_are_limited_per_key_and_all_told() {
    let mut config = quick(None);
    config.max_per_owner = 2;
    config.max_total = 3;
    let (_fake, mut c) = events_fake(config).await;
    let hook = Receiver::start().await;
    let sub = |host: &str| {
        subscription(
            "agent.started",
            json!({"host": host}),
            &hook.url("/h"),
            &secret(1),
        )
    };
    subscribe(&mut c, sub("a")).await;
    subscribe(&mut c, sub("b")).await;
    // A refresh is no more.
    subscribe(&mut c, sub("b")).await;
    let e = ask(&mut c, "events/subscribe", sub("c")).await.unwrap_err();
    assert_eq!(e["code"], -32025, "{e}");
    assert_eq!(e["data"]["limit"], "subscriptionsPerKey");
    let mut phone = Client::new(c.addr, &key2());
    phone.initialize("2025-03-26").await;
    subscribe(&mut phone, sub("a")).await;
    let e = ask(&mut phone, "events/subscribe", sub("b"))
        .await
        .unwrap_err();
    assert_eq!(e["data"]["limit"], "subscriptions", "{e}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cursor_picks_up_where_a_lapsed_subscription_left_off() {
    let (fake, mut c) = events_fake(quick(None)).await;
    let hook = Receiver::start().await;
    let url = hook.url("/h");
    let params = subscription("agent.state_changed", json!({}), &url, &secret(1));
    let sub = subscribe(&mut c, params.clone()).await;
    let hub = Arc::clone(hooks(&fake).hub());
    hub.ingest(&changed("local", "a", "idle"));
    let got = hook.wait_events(1, WAIT).await;
    let cursor = got[0].json["cursor"].clone();
    assert_ne!(cursor, sub["cursor"], "it moved on");
    let gone = json!({"name": "agent.state_changed", "arguments": {}, "delivery": {"url": url}});
    ask(&mut c, "events/unsubscribe", gone).await.unwrap();
    // Missed while it was gone: delivered from the cursor.
    hub.ingest(&changed("local", "b", "idle"));
    hub.ingest(&changed("local", "c", "question"));
    let mut again = params.clone();
    again["cursor"] = cursor;
    let r = subscribe(&mut c, again).await;
    assert_eq!(r["truncated"], false, "{r}");
    let got = hook.wait_events(3, WAIT).await;
    let refs: Vec<&str> = got[1..]
        .iter()
        .map(|p| p.json["data"]["ref"].as_str().unwrap())
        .collect();
    let refs: HashSet<&str> = refs.into_iter().collect();
    assert_eq!(refs, HashSet::from(["b", "c"]));
    // A link that came back since a cursor is what it may have missed.
    let cursor = subscribe(&mut c, params.clone()).await["cursor"].clone();
    hub.ingest(&core_event(EventKind::HostLink, "vps", "", Some("down")));
    hub.ingest(&core_event(
        EventKind::HostLink,
        "vps",
        "",
        Some("connected"),
    ));
    let mut refresh = params.clone();
    refresh["cursor"] = cursor;
    assert_eq!(subscribe(&mut c, refresh).await["truncated"], true);
    // One from another vibed, or none this one gave.
    let mut stale = params.clone();
    stale["cursor"] = json!("feedfacecafe-3");
    assert_eq!(subscribe(&mut c, stale).await["truncated"], true);
    let mut bad = params;
    bad["cursor"] = json!("nonsense");
    assert_eq!(
        ask(&mut c, "events/subscribe", bad).await.unwrap_err()["code"],
        -32602
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_is_relayed_reaches_only_keys_its_daemon_takes() {
    let (fake, mut c) = events_fake(quick(None)).await;
    let hook = Receiver::start().await;
    let mut phone = Client::new(c.addr, &key2());
    phone.initialize("2025-03-26").await;
    fake.relayed
        .lock()
        .unwrap()
        .insert(vibe_core::keys::hash(&key2()));
    subscribe(
        &mut c,
        subscription("agent.started", json!({}), &hook.url("/laptop"), &secret(1)),
    )
    .await;
    subscribe(
        &mut phone,
        subscription("agent.started", json!({}), &hook.url("/phone"), &secret(2)),
    )
    .await;
    let hub = Arc::clone(hooks(&fake).hub());
    hub.ingest_relayed(&core_event(EventKind::AgentAdded, "mac", "m", Some("busy")));
    hub.ingest(&core_event(
        EventKind::AgentAdded,
        "local",
        "a",
        Some("busy"),
    ));
    hook.wait_events(3, WAIT).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut heard: Vec<(String, String)> = hook
        .events()
        .iter()
        .map(|p| {
            let who = if signed_with(&secret(2), p) {
                "phone"
            } else {
                "laptop"
            };
            (
                who.to_owned(),
                p.json["data"]["ref"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    heard.sort();
    assert_eq!(
        heard,
        [
            ("laptop".to_owned(), "a".to_owned()),
            ("phone".to_owned(), "a".to_owned()),
            ("phone".to_owned(), "mac:m".to_owned()),
        ]
    );
}
