//! MCP on the real core, over real HTTP: the round VibeOS's orchestrator
//! drives, from start to delete, against the test backend with the
//! stand-in for Claude Code (PRD: MCP interface; #12's "Done when").

mod common;

#[path = "../../vibe-mcp/tests/client/mod.rs"]
mod client;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use client::{Client, post_raw, text};
use common::Golden;
use serde_json::{Value, json};
use vibe_core::paths::Layout;
use vibe_core::rpc::{KeyCreateParams, ProfileUpdateParams, ProjectAddParams};
use vibe_core::shim::{self, control_hello};
use vibe_core::units::GIB;
use vibe_env::tree::SessionTree;
use vibed::core::Core;

struct World {
    _golden: Golden,
    core: Arc<Core>,
    layout: Layout,
    addr: SocketAddr,
    key: String,
}

/// The golden world with `app` added, MCP served on a free port, and a
/// key for it.
async fn world() -> World {
    let golden = Golden::new();
    let core = Arc::new(Core::open(golden.config()).unwrap());
    let project = golden.home.join("src/app");
    core.add_project(ProjectAddParams {
        path: Some(project.to_str().unwrap().into()),
        ..Default::default()
    })
    .await
    .unwrap();
    for profile in ["software-eng-linux", "software-eng-mac"] {
        core.update_profile(ProfileUpdateParams {
            project: "app".into(),
            profile: profile.into(),
            memory: Some(GIB),
            ..Default::default()
        })
        .unwrap();
    }
    let key = core
        .create_key(KeyCreateParams {
            name: "orchestrator".into(),
        })
        .unwrap()
        .secret;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(vibe_mcp::http::serve(listener, Arc::clone(&core), true));
    World {
        layout: Layout::new(&golden.home, "macos"),
        _golden: golden,
        core,
        addr,
        key,
    }
}

impl World {
    async fn client(&self) -> Client {
        let mut c = Client::new(self.addr, &self.key);
        c.initialize("2025-06-18").await;
        c
    }

    fn session(&self, r: &str) -> SessionTree {
        let status = shim::connect(
            &self.layout.session_socket(r),
            &control_hello("test"),
            Duration::from_secs(10),
        )
        .unwrap()
        .status()
        .unwrap();
        SessionTree::new(status.pid)
    }
}

fn messages(r: &Value) -> Vec<(String, String)> {
    r["structuredContent"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap().to_owned(),
                m["text"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_orchestrator_round_from_start_to_delete() {
    let w = world().await;
    let mut c = w.client().await;

    // Find a project and its profiles.
    let r = c.call("vibe_list_projects", json!({})).await;
    assert!(text(&r).contains("app (on local)"), "{}", text(&r));
    assert!(
        text(&r).contains("software-eng-linux: linux, opus"),
        "{}",
        text(&r)
    );

    // Start an agent on a prompt that asks a question.
    let r = c
        .call(
            "vibe_start_agent",
            json!({"project": "app", "profile": "software-eng-linux", "prompt": "pick a question"}),
        )
        .await;
    let agent = r["structuredContent"]["ref"].as_str().unwrap().to_owned();
    assert_eq!(r["structuredContent"]["branch"], agent.as_str());

    // Wait: it answers its prompt with a question.
    let r = c
        .call("vibe_wait", json!({"agents": agent, "timeout": 30}))
        .await;
    assert_eq!(r["structuredContent"]["reached"], true, "{}", text(&r));
    assert_eq!(r["structuredContent"]["state"], "question");
    assert!(text(&r).contains("It is asking something"), "{}", text(&r));
    assert!(text(&r).contains("Red or blue?"), "{}", text(&r));

    // Read it, as the transcript and the screen have it.
    let r = c.call("vibe_read_messages", json!({"agent": agent})).await;
    let m = messages(&r);
    assert_eq!(m[0], ("user".into(), "pick a question".into()));
    assert!(
        m[1].0 == "tool" && m[1].1.starts_with("AskUserQuestion("),
        "{m:?}"
    );
    let r = c.call("vibe_read_output", json!({"agent": agent})).await;
    assert!(
        text(&r).starts_with(&format!("{agent} is question. cursor")),
        "{}",
        text(&r)
    );
    assert!(text(&r).contains("Red or blue?"));

    // Text is refused on a question; keys answer it.
    let e = c
        .fails("vibe_send", json!({"agent": agent, "text": "blue"}))
        .await;
    assert!(
        e.contains("is waiting on a question, so nothing was pasted"),
        "{e}"
    );
    let r = c
        .call(
            "vibe_send",
            json!({"agent": agent, "keys": ["B", "l", "u", "e", "Enter"]}),
        )
        .await;
    assert!(
        text(&r).starts_with(&format!("pressed B l u e Enter in {agent}")),
        "{}",
        text(&r)
    );
    let r = c
        .call("vibe_wait", json!({"agents": [agent], "timeout": 30}))
        .await;
    assert_eq!(r["structuredContent"]["reached"], true, "{}", text(&r));
    assert_eq!(r["structuredContent"]["state"], "idle");
    let r = c
        .call("vibe_read_messages", json!({"agent": agent, "last": 1}))
        .await;
    assert_eq!(
        messages(&r),
        [("assistant".into(), "You chose Blue.".into())]
    );

    // The next turn: text, submitted, then a wait for its answer and not
    // the last one's.
    let r = c
        .call(
            "vibe_send",
            json!({"agent": agent, "text": "and now green"}),
        )
        .await;
    assert!(
        text(&r).starts_with(&format!("sent to {agent} and submitted")),
        "{}",
        text(&r)
    );
    let r = c
        .call(
            "vibe_wait",
            json!({"agents": agent, "timeout": 30, "lines": 5}),
        )
        .await;
    assert_eq!(r["structuredContent"]["reached"], true, "{}", text(&r));
    assert!(text(&r).contains("> and now green"), "{}", text(&r));
    let r = c
        .call("vibe_read_messages", json!({"agent": agent, "last": 2}))
        .await;
    assert_eq!(
        messages(&r),
        [
            ("user".into(), "and now green".into()),
            ("assistant".into(), "> and now green".into())
        ]
    );
    let r = c.call("vibe_list_agents", json!({"state": "idle"})).await;
    let a = &r["structuredContent"]["agents"][0];
    assert_eq!(a["ref"], agent.as_str());
    assert_eq!(a["host"], "local");
    assert_eq!(a["status"]["activity"], "done");
    assert_eq!(a["attach"], format!("vibe attach {agent}"));
    assert!(a["attach_remote"].as_str().unwrap().starts_with("ssh -t "));
    assert!(a["memory"].as_u64().unwrap() > 0, "{a}");
    assert_eq!(a["memory_limit"], GIB);

    // From a cursor, only what's new.
    let r = c.call("vibe_read_output", json!({"agent": agent})).await;
    let cursor = r["structuredContent"]["cursor"].as_u64().unwrap();
    let r = c
        .call("vibe_read_output", json!({"agent": agent, "after": cursor}))
        .await;
    assert!(text(&r).contains("\n\nscreen:\n"), "{}", text(&r));
    assert!(
        !text(&r).contains("new since"),
        "nothing scrolled: {}",
        text(&r)
    );

    // Delete: nothing unsaved, so it goes, and its branch stays.
    let r = c.call("vibe_delete_agent", json!({"agent": agent})).await;
    assert_eq!(
        text(&r),
        format!("deleted {agent}; its branch {agent} is kept")
    );
    // No agents, and the host's own memory, with the golden Mac's
    // warning: a report, never a limit (#47).
    let r = c.call("vibe_list_agents", json!({})).await;
    assert_eq!(
        text(&r),
        "no agents\nlocal: 58G of 64G memory in use, 3G of 4G swap. Short of memory: macOS warns \
         of memory pressure. Agents keep running, and it swaps"
    );
    assert_eq!(
        r["structuredContent"]["hosts"],
        json!([{"host": "local", "memory": 58u64 << 30, "memory_total": 64u64 << 30,
                "swap": 3u64 << 30, "swap_total": 4u64 << 30,
                "pressure": "macOS warns of memory pressure"}])
    );
    let e = c.fails("vibe_read_messages", json!({"agent": agent})).await;
    assert_eq!(
        e,
        format!(
            "no agent called {agent} on local. vibe_list_agents lists the agents and their refs."
        )
    );
    // The key says when it was last used.
    assert!(w.core.keys().unwrap().keys[0].last_used_at.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_resumes_an_exited_agent_and_reads_never_wake_it() {
    let w = world().await;
    let mut c = w.client().await;
    let r = c
        .call(
            "vibe_start_agent",
            json!({"project": "app", "profile": "software-eng-mac", "prompt": "question"}),
        )
        .await;
    let agent = r["structuredContent"]["ref"].as_str().unwrap().to_owned();
    c.call("vibe_wait", json!({"agents": agent, "timeout": 30}))
        .await;
    w.session(&agent).end(Duration::from_secs(5));

    // It answered its prompt before it exited, and a wait says both.
    let r = c
        .call("vibe_wait", json!({"agents": agent, "timeout": 30}))
        .await;
    assert!(
        text(&r).starts_with(&format!("{agent} has answered: it is exited")),
        "{}",
        text(&r)
    );
    assert!(
        text(&r).contains("It has exited since: vibe_send resumes it."),
        "{}",
        text(&r)
    );
    let r = c.call("vibe_read_output", json!({"agent": agent})).await;
    assert_eq!(r["structuredContent"]["live"], false);
    assert!(
        text(&r).contains("Red or blue?"),
        "the last screen saved: {}",
        text(&r)
    );
    let r = c.call("vibe_read_messages", json!({"agent": agent})).await;
    assert_eq!(messages(&r)[0].1, "question");
    let r = c.call("vibe_list_agents", json!({})).await;
    assert_eq!(r["structuredContent"]["agents"][0]["state"], "exited");

    // A send resumes its session, and types once it's up.
    let r = c
        .call("vibe_send", json!({"agent": agent, "text": "back again"}))
        .await;
    assert_eq!(r["structuredContent"]["woke"], true, "{}", text(&r));
    assert!(text(&r).contains("(woke it first)"));
    let session_id = w.core.agents(Default::default()).await.unwrap().agents[0]
        .session_id
        .clone();
    let mut screen = String::new();
    for _ in 0..200 {
        let r = c.call("vibe_read_output", json!({"agent": agent})).await;
        screen = text(&r);
        if screen.contains("> back again") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        screen.contains(&format!("Resumed {session_id}.")),
        "{screen}"
    );
    assert!(screen.contains("> back again"), "{screen}");
    // Resumed, it has written nothing in answer; ended again, it never will,
    // and a wait says so at once instead of running out its time.
    w.session(&agent).end(Duration::from_secs(5));
    let begun = std::time::Instant::now();
    let r = c
        .call("vibe_wait", json!({"agents": agent, "timeout": 30}))
        .await;
    assert!(
        text(&r).starts_with(&format!("{agent} has exited: vibe_send resumes it")),
        "{}",
        text(&r)
    );
    assert_eq!(r["structuredContent"]["reached"], false);
    assert!(begun.elapsed() < Duration::from_secs(10));
    c.call("vibe_delete_agent", json!({"agent": agent, "force": true}))
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoked_key_stops_working_at_once() {
    let w = world().await;
    let mut c = w.client().await;
    c.call("vibe_list_agents", json!({})).await;
    w.core
        .revoke_key(vibe_core::rpc::KeyParams {
            name: "orchestrator".into(),
        })
        .unwrap();
    let r = post_raw(
        w.addr,
        "/mcp",
        &[("Authorization", &format!("Bearer {}", w.key))],
        &json!({"jsonrpc": "2.0", "id": 9, "method": "tools/list"}).to_string(),
    )
    .await;
    assert_eq!(r.status, 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_wakes_an_asleep_agent_and_still_guards_its_question() {
    let w = world().await;
    let mut c = w.client().await;
    let r = c
        .call(
            "vibe_start_agent",
            json!({"project": "app", "profile": "software-eng-mac", "prompt": "question"}),
        )
        .await;
    let agent = r["structuredContent"]["ref"].as_str().unwrap().to_owned();
    c.call("vibe_wait", json!({"agents": agent, "timeout": 30}))
        .await;
    // Asleep as vibed records it (#6 puts it there), its session frozen
    // but still there.
    {
        let mut store = vibe_core::store::Store::open(&w.layout.db()).unwrap();
        store
            .set_agent_state("local", &agent, vibe_core::model::AgentState::Asleep)
            .unwrap();
    }
    let r = c.call("vibe_read_output", json!({"agent": agent})).await;
    assert!(
        text(&r).starts_with(&format!("{agent} is asleep")),
        "{}",
        text(&r)
    );
    let r = c.call("vibe_list_agents", json!({})).await;
    assert_eq!(
        r["structuredContent"]["agents"][0]["state"], "asleep",
        "reads don't wake it"
    );
    // Text is still refused once it's awake on its question.
    let e = c
        .fails("vibe_send", json!({"agent": agent, "text": "blue"}))
        .await;
    assert!(
        e.contains("is waiting on a question, so nothing was pasted. It was woken first."),
        "{e}"
    );
    let r = c.call("vibe_list_agents", json!({})).await;
    assert_eq!(r["structuredContent"]["agents"][0]["state"], "question");
    let r = c
        .call(
            "vibe_send",
            json!({"agent": agent, "keys": ["R", "e", "d", "Enter"]}),
        )
        .await;
    assert_eq!(r["structuredContent"]["woke"], false);
    let r = c
        .call("vibe_wait", json!({"agents": agent, "timeout": 30}))
        .await;
    assert_eq!(r["structuredContent"]["state"], "idle", "{}", text(&r));
    c.call("vibe_delete_agent", json!({"agent": agent, "force": true}))
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_name_used_again_is_waited_on_afresh() {
    let w = world().await;
    let mut c = w.client().await;
    let start = json!({"project": "app", "profile": "software-eng-linux", "prompt": "question", "name": "worker"});
    c.call("vibe_start_agent", start.clone()).await;
    c.call("vibe_wait", json!({"agents": "worker", "timeout": 30}))
        .await;
    c.call(
        "vibe_send",
        json!({"agent": "worker", "keys": ["R", "e", "d", "Enter"]}),
    )
    .await;
    let r = c
        .call("vibe_wait", json!({"agents": "worker", "timeout": 30}))
        .await;
    assert_eq!(r["structuredContent"]["state"], "idle");
    c.call(
        "vibe_delete_agent",
        json!({"agent": "worker", "force": true}),
    )
    .await;
    // Its branch is kept, which holds the name until it goes too.
    let project = w.layout.home().join("src/app");
    common::git(&["-C", project.to_str().unwrap(), "branch", "-D", "worker"]);
    // The new worker's prompt is what's waited on, not the old one's send.
    c.call("vibe_start_agent", start).await;
    let begun = std::time::Instant::now();
    let r = c
        .call("vibe_wait", json!({"agents": "worker", "timeout": 30}))
        .await;
    assert_eq!(r["structuredContent"]["reached"], true, "{}", text(&r));
    assert_eq!(r["structuredContent"]["state"], "question");
    assert!(begun.elapsed() < Duration::from_secs(15));
    c.call(
        "vibe_delete_agent",
        json!({"agent": "worker", "force": true}),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_turn_tailscale_on_and_off_and_it_applies_at_once() {
    use vibe_core::model::EventKind;
    use vibe_core::rpc::{HostParams, McpSetParams};
    let golden = Golden::new();
    let core = Arc::new(Core::open(golden.config()).unwrap());
    let host_toml = golden.home.join(".config/vibeos/host.toml");
    let s = core.mcp(HostParams::default()).unwrap();
    assert_eq!(
        (s.port, s.tailscale, s.url.as_str()),
        (7878, false, "http://127.0.0.1:7878/mcp")
    );
    assert!(s.tailscale_urls.is_empty());

    // A port of its own, and the listeners on it, checked only hourly.
    let free = || {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    };
    let first = free();
    std::fs::create_dir_all(host_toml.parent().unwrap()).unwrap();
    std::fs::write(
        &host_toml,
        format!("# mine\nmcp_port = {first}  # the port\n"),
    )
    .unwrap();
    // Not served yet, so hosts.list says MCP is off.
    let mcp_on = |core: &Core| core.hosts().unwrap().hosts[0].mcp;
    assert!(!mcp_on(&core));
    let (_, mut served) = core.subscribe();
    let task = core.spawn_mcp(Duration::from_secs(3600));
    let serves = |port: u16| async move {
        for _ in 0..200 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    };
    assert!(serves(first).await);
    // Served on loopback: hosts.list says so, and it's announced.
    let e = tokio::time::timeout(Duration::from_secs(5), served.recv())
        .await
        .expect("an mcp.changed")
        .unwrap();
    assert_eq!(e.kind, EventKind::McpChanged);
    assert_eq!(e.message.as_deref(), Some("MCP is served"));
    assert!(mcp_on(&core));

    // On: written into host.toml, keeping the rest; announced; and the
    // Tailscale URL is there for the setup command.
    let (_, mut events) = core.subscribe();
    let s = core
        .set_mcp(McpSetParams {
            host: None,
            tailscale: Some(true),
        })
        .unwrap();
    assert!(s.tailscale);
    assert_eq!(s.url, format!("http://127.0.0.1:{first}/mcp"));
    assert_eq!(
        s.tailscale_urls,
        [format!("http://{}:{first}/mcp", common::GOLDEN_TAILSCALE)]
    );
    assert_eq!(events.try_recv().unwrap().kind, EventKind::McpChanged);
    let text = std::fs::read_to_string(&host_toml).unwrap();
    assert!(text.starts_with("# mine\n"), "{text}");
    assert!(
        text.contains(&format!("mcp_port = {first}  # the port")),
        "{text}"
    );
    assert!(text.contains("mcp_tailscale = true"), "{text}");
    assert_eq!(core.mcp(HostParams::default()).unwrap(), s);

    // A set applies at once, not at the next hourly check: with the port
    // edited by hand meanwhile, a set moves the listener there now.
    let second = free();
    let edited = text.replace(
        &format!("mcp_port = {first}"),
        &format!("mcp_port = {second}"),
    );
    std::fs::write(&host_toml, edited).unwrap();
    let s = core
        .set_mcp(McpSetParams {
            host: Some("local".into()),
            tailscale: Some(false),
        })
        .unwrap();
    assert!(!s.tailscale && s.tailscale_urls.is_empty());
    assert!(serves(second).await, "the set was applied at once");
    let mut gone = false;
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(("127.0.0.1", first))
            .await
            .is_err()
        {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(gone, "the old port stopped");

    // Nothing to change, another host, or a broken host.toml: refused, and
    // the file is left alone.
    let e = core.set_mcp(McpSetParams::default()).unwrap_err();
    assert_eq!(
        e.message,
        "Invalid params: give at least one setting to change"
    );
    let e = core
        .set_mcp(McpSetParams {
            host: Some("vps".into()),
            tailscale: Some(true),
        })
        .unwrap_err();
    assert_eq!(e.message, "no host named vps");
    std::fs::write(&host_toml, "cpu_core = 4\n").unwrap();
    let e = core.mcp(HostParams::default()).unwrap_err();
    assert_eq!(e.code, vibe_core::rpc::ErrorCode::HostConfigInvalid.code());
    assert!(
        core.set_mcp(McpSetParams {
            host: None,
            tailscale: Some(true),
        })
        .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(&host_toml).unwrap(),
        "cpu_core = 4\n"
    );
    // While it's broken, MCP stays where it was.
    assert!(serves(second).await);
    assert!(mcp_on(&core));
    // Once the listeners stop, hosts.list says MCP is off again.
    task.abort();
    for _ in 0..200 {
        if !mcp_on(&core) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!mcp_on(&core), "MCP is off once it isn't served");
}
