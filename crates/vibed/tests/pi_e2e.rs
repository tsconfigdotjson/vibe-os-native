//! Real Pi, hermetically (#52's test plan): the `pi` binary under `vibe-shim`
//! on a PTY, through the whole of the core, against a mock model on loopback
//! with no provider, no login and no key of anyone's. Its TUI's open questions
//! get their answers here: that a send submits, that Escape interrupts, that
//! it opens straight to its prompt, and that it resumes the same session.
//!
//! It skips without a `pi`: on `PATH`, or in `VIBE_TEST_PI_BIN`. With
//! `VIBE_TEST_PI_MARKER` set, as `scripts/check-full.sh` does, it notes it ran
//! there, so a skip can't pass for a pass. It needs `cargo build --workspace`
//! first, for `vibe-shim`.

mod common;

#[path = "common/mock_model.rs"]
mod mock_model;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use common::{Golden, wait_until, workspace_bin};
use mock_model::{MOCK_KEY, MockModel};
use serde_json::json;
use vibe_core::model::{Activity, AgentKind, AgentState, MessageRole};
use vibe_core::paths::Layout;
use vibe_core::rpc::{
    AgentDeleteParams, AgentParams, AgentStartParams, HostParams, ProfileCreateParams,
    ProjectAddParams,
};
use vibe_env::testing::TestBackend;
use vibe_services::secrets::MemoryStore;
use vibe_services::testing::{FakeService, SECRET_MARK};
use vibed::agents::Backends;
use vibed::core::Core;
use vibed::dispatch::{Session, handle_message};
use vibed::services::ServicesConfig;

fn pi_binary() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("VIBE_TEST_PI_BIN").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(p));
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join("pi"))
        .find(|p| p.is_file())
}

fn mark() {
    if let Some(dir) = std::env::var_os("VIBE_TEST_PI_MARKER") {
        fs::create_dir_all(&dir).unwrap();
        fs::write(Path::new(&dir).join("pi_e2e"), "ran\n").unwrap();
    }
}

struct World {
    _golden: Golden,
    core: Arc<Core>,
    layout: Layout,
    mock: MockModel,
    /// The service behind the real broker.
    fake: FakeService,
    /// What the broker holds for it, to know its Bearer by.
    store: Arc<MemoryStore>,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

static STRAY: Once = Once::new();

async fn world(pi: PathBuf) -> World {
    // What Pi would silently use, were it given them.
    STRAY.call_once(|| {
        // SAFETY: only here, before either of this binary's tests starts anything
        // of Pi's; nothing else in it reads or writes these.
        unsafe {
            std::env::set_var("OPENROUTER_API_KEY", "stray-openrouter");
            std::env::set_var("ANTHROPIC_API_KEY", "stray-anthropic");
            std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-oat01-stray");
        }
    });
    let golden = Golden::new();
    let layout = Layout::new(&golden.home, "macos");
    let agent_dir = layout.linux_pi_dir();
    fs::create_dir_all(&agent_dir).unwrap();
    let mock = MockModel::start("127.0.0.1");
    // And a provider with no key at all, to see how Pi says it can't start a turn.
    let mut models: serde_json::Value =
        serde_json::from_str(&mock.models_json("127.0.0.1")).unwrap();
    let mut nokey = models["providers"]["mock"].clone();
    nokey.as_object_mut().unwrap().remove("apiKey");
    models["providers"]["nokey"] = nokey;
    fs::write(agent_dir.join("models.json"), models.to_string()).unwrap();

    let mut config = golden.config();
    config.backends = Backends::all(Arc::new(
        TestBackend::new(&golden.claude).with_pi(pi, &agent_dir),
    ));
    // The real broker and the real `vibe mcp-proxy`, to a stand-in for the service.
    let fake = FakeService::start().await;
    let store = Arc::new(MemoryStore::default());
    config.services = Some(ServicesConfig {
        services: vec![fake.service()],
        store: store.clone(),
        place: vibe_services::Place::Mac,
        token_wait: Duration::from_millis(500),
    });
    config.vibe_cli = workspace_bin("vibe").into_os_string();
    let core = Arc::new(Core::open(config).unwrap());
    core.add_project(ProjectAddParams {
        path: Some(golden.home.join("src/app").to_str().unwrap().into()),
        ..Default::default()
    })
    .await
    .unwrap();
    core.create_profile(ProfileCreateParams {
        project: "app".into(),
        name: "pi-linux".into(),
        agent: Some(AgentKind::Pi),
        pi_model: Some("mock/mock-model".into()),
        ..Default::default()
    })
    .unwrap();
    let tasks = core.spawn_services();
    let begun = rpc(&core, "services.connect", json!({"service": "linear"})).await;
    let back = fake
        .sign_in(begun["result"]["url"].as_str().expect("a page"))
        .await;
    let done = rpc(
        &core,
        "services.complete",
        json!({"service": "linear", "callback_url": back}),
    )
    .await;
    assert_eq!(done["result"]["state"], "connected", "{done}");
    World {
        _golden: golden,
        core,
        layout,
        mock,
        fake,
        store,
        _tasks: tasks,
    }
}

/// Real Pi's own list and sign-in, with no network and a key that's made up. What
/// `vibe-setup.ts` asks of it isn't a documented API, so this is what says the pin
/// still has it. Pi's own files are in a home of their own.
async fn setup_world(pi: PathBuf) -> (Golden, Arc<Core>) {
    let golden = Golden::new();
    let mut config = golden.config();
    config.pi_driver = Some(pi);
    let core = Arc::new(Core::open(config).unwrap());
    (golden, core)
}

#[tokio::test]
async fn real_pi_gives_its_list_and_signs_in_with_a_key_and_nothing_elses_runs() {
    let Some(pi) = pi_binary() else {
        eprintln!("skipped: no pi");
        return;
    };
    mark();
    let (golden, core) = setup_world(pi).await;
    let own = golden.home.join(".pi/agent");
    // An extension of the owner's, which must not run for this.
    let marker = golden.home.join("an-extension-ran");
    fs::create_dir_all(own.join("extensions")).unwrap();
    fs::write(
        own.join("extensions/theirs.ts"),
        format!(
            "import {{ writeFileSync }} from \"node:fs\";\n\
             export default function () {{ writeFileSync({marker:?}, \"ran\"); }}\n"
        ),
    )
    .unwrap();

    let list = rpc(&core, "pi.models", json!({"target": "native"})).await;
    let providers = list["result"]["providers"].as_array().expect("a list");
    let opencode = providers
        .iter()
        .find(|p| p["id"] == "opencode")
        .unwrap_or_else(|| panic!("{list}"));
    assert_eq!(opencode["signed_in"], false);
    assert_eq!(opencode["api_key"], true);
    assert!(!opencode["models"].as_array().unwrap().is_empty());
    let anthropic = providers.iter().find(|p| p["id"] == "anthropic").unwrap();
    assert_eq!(anthropic["oauth"], true);

    let begun = rpc(
        &core,
        "pi.login.start",
        json!({"target": "native", "provider": "opencode", "method": "api_key"}),
    )
    .await;
    let login = &begun["result"];
    assert_eq!(login["prompt"]["kind"], "secret", "{begun}");
    let done = rpc(
        &core,
        "pi.login.answer",
        json!({"id": login["id"], "prompt": login["prompt"]["id"], "value": "sk-made-up"}),
    )
    .await;
    assert_eq!(done["result"]["state"], "done", "{done}");
    assert!(!done.to_string().contains("sk-made-up"));
    let auth = fs::read_to_string(own.join("auth.json")).unwrap();
    assert!(auth.contains("sk-made-up"), "Pi's own store has it");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(own.join("auth.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let list = rpc(&core, "pi.models", json!({"target": "native"})).await;
    let opencode = list["result"]["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "opencode")
        .unwrap()
        .clone();
    assert_eq!(opencode["signed_in"], true, "{list}");
    assert!(!marker.exists(), "the owner's extension ran");
}

#[tokio::test]
async fn real_pi_asks_how_shows_a_page_and_checks_what_comes_back() {
    let Some(pi) = pi_binary() else {
        eprintln!("skipped: no pi");
        return;
    };
    mark();
    let (_golden, core) = setup_world(pi).await;
    let begun = rpc(
        &core,
        "pi.login.start",
        json!({"target": "vm", "provider": "anthropic", "method": "oauth"}),
    )
    .await;
    let login = &begun["result"];
    assert_eq!(login["prompt"]["kind"], "select", "{begun}");
    let options = login["prompt"]["options"].as_array().unwrap();
    assert!(options.iter().any(|o| o["id"] == "browser"), "{begun}");
    let page = rpc(
        &core,
        "pi.login.answer",
        json!({"id": login["id"], "prompt": login["prompt"]["id"], "value": "browser"}),
    )
    .await;
    let now = &page["result"];
    let url = now["url"].as_str().unwrap_or_else(|| panic!("{page}"));
    assert!(url.starts_with("https://"), "{page}");
    assert!(
        vibe_env::pi_setup::redirect_of(url).is_some(),
        "it means to come back to loopback, for a Mac to catch: {url}"
    );
    assert_eq!(now["prompt"]["kind"], "manual_code", "{page}");
    // Somewhere it never sent anyone. Pi refuses that before it asks anyone anything.
    let wrong = rpc(
        &core,
        "pi.login.answer",
        json!({
            "id": now["id"], "prompt": now["prompt"]["id"],
            "value": "http://localhost:53692/callback?code=nope&state=not-its-state",
        }),
    )
    .await;
    assert_eq!(wrong["result"]["state"], "failed", "{wrong}");

    // Given up on, it's over at once.
    let begun = rpc(
        &core,
        "pi.login.start",
        json!({"target": "native", "provider": "anthropic", "method": "oauth"}),
    )
    .await;
    let over = rpc(
        &core,
        "pi.login.cancel",
        json!({"id": begun["result"]["id"]}),
    )
    .await;
    assert_eq!(over["result"]["state"], "cancelled", "{over}");
}

/// One JSON-RPC call through the daemon's dispatcher, as a socket's client.
async fn rpc(core: &Core, method: &str, params: serde_json::Value) -> serde_json::Value {
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    let answer = handle_message(core, &mut Session::default(), line.as_bytes())
        .await
        .expect("an answer");
    serde_json::from_str(&answer).unwrap()
}

impl World {
    async fn activity(&self, r: &str) -> (AgentState, Activity) {
        let list = self.core.agents(HostParams { host: None }).await.unwrap();
        let a = list.agents.iter().find(|a| a.agent == r).unwrap();
        (a.state, a.status.activity)
    }

    async fn wait_for(&self, r: &str, want: Activity) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let (_, now) = self.activity(r).await;
            if now == want {
                return;
            }
            if Instant::now() >= deadline {
                let screen = self.screen(r).await;
                panic!("{r} is {now}, not {want}. Its screen:\n{screen}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn screen(&self, r: &str) -> String {
        self.core
            .agent_output(None, r, None)
            .await
            .map(|s| s.screen)
            .unwrap_or_default()
    }

    async fn wait_screen(&self, r: &str, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let screen = self.screen(r).await;
            if screen.contains(text) {
                return;
            }
            assert!(Instant::now() < deadline, "no {text:?} on:\n{screen}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn send(&self, r: &str, text: &str) {
        self.core
            .send_input(
                None,
                r,
                vibe_mcp::Input::Text {
                    text: text.into(),
                    submit: true,
                    force: false,
                },
            )
            .await
            .unwrap();
    }

    async fn wait_answered(&self, r: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let c = self.core.check_agent(None, r, false).await.unwrap();
            if c.answered {
                return;
            }
            assert!(Instant::now() < deadline, "{r} never answered: {c:?}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn sessions(&self, r: &str) -> Vec<String> {
        fs::read_dir(self.layout.agent_dir(r).join(vibe_env::pi::VM_SESSIONS))
            .map(|d| {
                d.map(|e| e.unwrap().file_name().into_string().unwrap())
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_real_pi_lives_a_whole_life_on_a_mock_model() {
    let Some(pi) = pi_binary() else {
        eprintln!("a_real_pi_…: skipped: no `pi` on PATH, nor in VIBE_TEST_PI_BIN");
        return;
    };
    let w = world(pi).await;

    // It opens straight to its prompt, with no first-run dialog, and calls the
    // shared service's tool: `vibe-services.ts` registering it from what a real
    // launch offers, Pi starting the real shim, the broker adding the login.
    let a = w
        .core
        .start_agent(AgentStartParams {
            project: "app".into(),
            profile: "pi-linux".into(),
            prompt: "call linear".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let r = a.agent.clone();
    w.wait_for(&r, Activity::Done).await;
    w.wait_answered(&r).await;
    let calls = w.fake.calls();
    let call = calls.last().expect("the tool was called");
    assert_eq!(call.tool.as_deref(), Some("list_issues"));
    let login = vibe_services::SecretStore::load(&*w.store, "linear")
        .unwrap()
        .expect("a login");
    assert_eq!(call.bearer.as_deref(), Some(login.access.as_str()));
    let agent_dir = w.layout.linux_pi_dir();
    assert!(!agent_dir.join("mcp.json").exists(), "nothing written");
    let messages = w.core.agent_messages(None, &r, 10).await.unwrap();
    assert_eq!(messages.first().unwrap().role, MessageRole::User);
    assert_eq!(messages.last().unwrap().text, "The tool said issues.");
    w.wait_screen(&r, "The tool said issues.").await;
    assert!(
        !messages.iter().any(|m| m.text.contains(SECRET_MARK)),
        "{messages:?}"
    );

    // Its one session, by the id it was started with.
    let files = w.sessions(&r);
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(files[0].ends_with(&format!("_{}.jsonl", a.session_id)));

    // Only the mock's key was used: no stray one, and no Claude token.
    let keys = w.mock.keys.lock().unwrap().clone();
    assert!(!keys.is_empty());
    for key in &keys {
        assert!(key.contains(MOCK_KEY), "{key}");
    }

    // A send submits, and a wait waits for its answer.
    w.send(&r, "and again").await;
    w.wait_answered(&r).await;
    let messages = w.core.agent_messages(None, &r, 10).await.unwrap();
    assert!(
        messages.iter().any(|m| m.text == "and again"),
        "{messages:?}"
    );
    assert_eq!(messages.last().unwrap().text, "Answered.");

    // Escape interrupts a turn, as the menu's Interrupt presses it.
    w.send(&r, "slow").await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    w.core
        .interrupt_agent(AgentParams {
            host: None,
            agent: r.clone(),
        })
        .await
        .unwrap();
    w.wait_for(&r, Activity::Interrupted).await;

    // Asleep and awake: exactly where it was.
    w.core.sleep_agent(None, &r).await.unwrap();
    assert_eq!(w.activity(&r).await.0, AgentState::Asleep);
    w.core.resume_agent(None, &r).await.unwrap();
    w.send(&r, "after a sleep").await;
    w.wait_answered(&r).await;

    // Its processes gone, as after a reboot: the same session id goes on in
    // the same file, with the conversation it had.
    let socket = w.layout.session_socket(&r);
    let vibed::sessions::Probe::Running(session) = vibed::sessions::probe(&socket).await else {
        panic!("its session runs");
    };
    vibe_env::tree::SessionTree::new(session.pid).end(Duration::from_secs(5));
    wait_until("it to exit", || !socket.exists());
    w.core.resume_agent(None, &r).await.unwrap();
    w.send(&r, "after a reboot").await;
    w.wait_answered(&r).await;
    assert_eq!(w.sessions(&r), files, "the same file");
    let messages = w.core.agent_messages(None, &r, 50).await.unwrap();
    for said in [
        "call linear",
        "and again",
        "after a sleep",
        "after a reboot",
    ] {
        assert!(
            messages.iter().any(|m| m.text == said),
            "{said}: {messages:?}"
        );
    }

    w.core
        .delete_agent(AgentDeleteParams {
            host: None,
            agent: r.clone(),
            force: Some(true),
        })
        .await
        .unwrap();
    assert!(
        agent_dir.join("models.json").is_file(),
        "the shared dir stays"
    );

    // Pi's real words for a turn that can't start, which `pi_error_on_screen` has
    // only from its source: read off its real screen, with its real editor below.
    w.core
        .create_profile(ProfileCreateParams {
            project: "app".into(),
            name: "pi-nokey".into(),
            agent: Some(AgentKind::Pi),
            pi_model: Some("nokey/mock-model".into()),
            ..Default::default()
        })
        .unwrap();
    let a = w
        .core
        .start_agent(AgentStartParams {
            project: "app".into(),
            profile: "pi-nokey".into(),
            prompt: "hello".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let r = a.agent.clone();
    let deadline = Instant::now() + Duration::from_secs(60);
    let list = loop {
        let list = w.core.agents(HostParams { host: None }).await.unwrap();
        let errored = list
            .agents
            .iter()
            .any(|x| x.agent == r && x.status.activity == Activity::Errored);
        if errored {
            break list;
        }
        assert!(
            Instant::now() < deadline,
            "it never read as errored; its screen:\n{}",
            w.screen(&r).await
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let error = list
        .agents
        .iter()
        .find(|x| x.agent == r)
        .and_then(|x| x.status.error.clone())
        .unwrap_or_default();
    assert!(error.contains("No API key found for nokey"), "{error}");
    w.core
        .delete_agent(AgentDeleteParams {
            host: None,
            agent: r,
            force: Some(true),
        })
        .await
        .unwrap();

    // `/` and `!` as its real editor sorts them. One that's answered and has since
    // lost its key reads as done, so only the screen can say a later turn failed. Its
    // own `/session` owes no answer, but a path or a lone `!` goes to the model.
    let models_file = w.layout.linux_pi_dir().join("models.json");
    let models = fs::read_to_string(&models_file).unwrap();
    for prompt in ["/tmp/notes.txt is empty", "!"] {
        fs::write(&models_file, &models).unwrap();
        let a = w
            .core
            .start_agent(AgentStartParams {
                project: "app".into(),
                profile: "pi-linux".into(),
                prompt: "hello".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let r = a.agent.clone();
        let activity = async || {
            let list = w.core.agents(HostParams { host: None }).await.unwrap();
            let a = list.agents.iter().find(|x| x.agent == r).unwrap();
            (a.status.activity, a.status.error.clone())
        };
        let until = async |wanted: Activity| {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                let (now, _) = activity().await;
                if now == wanted {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "{prompt:?}: it's {now}, not {wanted}; its screen:\n{}",
                    w.screen(&r).await
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        };
        let send = async |text: &str| {
            w.core
                .send_input(
                    None,
                    &r,
                    vibe_mcp::Input::Text {
                        text: text.into(),
                        submit: true,
                        force: false,
                    },
                )
                .await
                .unwrap();
        };
        until(Activity::Done).await;
        let mut no_key: serde_json::Value = serde_json::from_str(&models).unwrap();
        no_key["providers"]["mock"]
            .as_object_mut()
            .unwrap()
            .remove("apiKey");
        fs::write(&models_file, no_key.to_string()).unwrap();
        let socket = w.layout.session_socket(&r);
        let vibed::sessions::Probe::Running(session) = vibed::sessions::probe(&socket).await else {
            panic!("its session runs");
        };
        vibe_env::tree::SessionTree::new(session.pid).end(Duration::from_secs(5));
        wait_until("it to exit", || !socket.exists());
        w.core.resume_agent(None, &r).await.unwrap();
        send("/session").await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        let (now, _) = activity().await;
        assert_eq!(now, Activity::Done, "{}", w.screen(&r).await);
        send(prompt).await;
        until(Activity::Errored).await;
        let (_, error) = activity().await;
        let error = error.unwrap_or_default();
        assert!(error.contains("No API key found for mock"), "{error}");
        w.core
            .delete_agent(AgentDeleteParams {
                host: None,
                agent: r,
                force: Some(true),
            })
            .await
            .unwrap();
    }
    mark();
}

/// A profile's Model field lets through what's safe to pass on, and the real Pi has
/// the last word: it resolves a bare id only it can, and says of one it won't take,
/// and that's what the app says.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_pi_decides_what_a_model_is() {
    let Some(pi) = pi_binary() else {
        eprintln!("a_real_pi_decides_…: skipped: no `pi` on PATH, nor in VIBE_TEST_PI_BIN");
        return;
    };
    let w = world(pi).await;
    for (name, model) in [("pi-bare", "mock-model"), ("pi-nosuch", "no-such-model")] {
        w.core
            .create_profile(ProfileCreateParams {
                project: "app".into(),
                name: name.into(),
                agent: Some(AgentKind::Pi),
                pi_model: Some(model.into()),
                ..Default::default()
            })
            .unwrap();
    }
    let start = async |profile: &str| {
        w.core
            .start_agent(AgentStartParams {
                project: "app".into(),
                profile: profile.into(),
                prompt: "hello".into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .agent
    };

    // A bare id, as `pi auth check` can't resolve and Pi itself can.
    let bare = start("pi-bare").await;
    w.wait_for(&bare, Activity::Done).await;

    // One it won't take: gone before it draws anything, saying why.
    let nosuch = start("pi-nosuch").await;
    let deadline = Instant::now() + Duration::from_secs(60);
    let said = loop {
        let list = w.core.agents(HostParams { host: None }).await.unwrap();
        let this = list.agents.iter().find(|x| x.agent == nosuch).unwrap();
        if this.status.activity == Activity::Errored {
            assert_eq!(this.state, AgentState::Exited);
            break this.status.error.clone().unwrap_or_default();
        }
        assert!(
            Instant::now() < deadline,
            "it never read as errored: {:?}",
            this.status
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert!(said.contains("Model \"no-such-model\" not found"), "{said}");
    assert!(!said.contains("--list-models"), "{said}");

    for agent in [bare, nosuch] {
        w.core
            .delete_agent(AgentDeleteParams {
                host: None,
                agent,
                force: Some(true),
            })
            .await
            .unwrap();
    }
}
