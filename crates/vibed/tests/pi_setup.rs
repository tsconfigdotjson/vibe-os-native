//! Pi's list and its sign-in, for the app, through the core against the stand-in for
//! Pi. The real `pi` does the same in `pi_e2e.rs`, where there is one.

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Once};

use common::{Golden, publish_pi};
use vibe_core::model::{PiLoginMethod, PiLoginState, PiPromptKind, PiTarget};
use vibe_core::paths::Layout;
use vibe_core::rpc::{
    ErrorCode, PiLogin, PiLoginAnswerParams, PiLoginGetParams, PiLoginParams, PiLoginStartParams,
    PiModels, PiModelsParams,
};
use vibe_env::pi_release;
use vibed::core::Core;

static STRAY: Once = Once::new();

struct World {
    _golden: Golden,
    core: Arc<Core>,
    layout: Layout,
    own: PathBuf,
}

async fn world() -> World {
    world_with(|_| {})
}

fn world_with(change: impl FnOnce(&mut vibed::core::Config)) -> World {
    STRAY.call_once(|| {
        // SAFETY: only here, before any test of this binary starts anything of Pi's;
        // nothing else in it reads or writes these.
        unsafe {
            std::env::set_var("OPENROUTER_API_KEY", "stray-openrouter");
            std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-oat01-stray");
        }
    });
    let golden = Golden::new();
    let layout = Layout::new(&golden.home, "macos");
    let mut config = golden.config();
    change(&mut config);
    let own = layout.home().join(".pi/agent");
    let core = Arc::new(Core::open(config).unwrap());
    World {
        _golden: golden,
        core,
        layout,
        own,
    }
}

fn start(provider: &str, method: PiLoginMethod, target: PiTarget) -> PiLoginStartParams {
    PiLoginStartParams {
        host: None,
        target,
        provider: provider.into(),
        method,
    }
}

fn answer(login: &PiLogin, value: &str) -> PiLoginAnswerParams {
    PiLoginAnswerParams {
        host: None,
        id: login.id.clone(),
        prompt: login.prompt.as_ref().expect("a question").id.clone(),
        value: value.into(),
    }
}

impl World {
    async fn models(&self, target: PiTarget) -> PiModels {
        self.core
            .pi_models(PiModelsParams { host: None, target })
            .await
            .unwrap()
    }

    /// What Pi was last run with, by what it left where it was told to keep its things.
    fn last_run(&self, name: &str) -> String {
        fs::read_to_string(self.own.join(name)).unwrap()
    }

    fn own_auth(&self) -> String {
        fs::read_to_string(self.own.join("auth.json")).unwrap_or_default()
    }

    fn store_auth(&self) -> String {
        fs::read_to_string(self.layout.linux_pi_logins_dir().join("auth.json")).unwrap_or_default()
    }

    /// Nothing's left of any run.
    fn no_scratch_left(&self) {
        let root = self.layout.data_dir().join(".pi-setup");
        let left: Vec<_> = fs::read_dir(&root)
            .map(|d| d.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "{left:?}");
    }
}

#[tokio::test]
async fn a_list_says_who_is_signed_in_and_runs_nothing_of_anyones() {
    let w = world().await;
    fs::create_dir_all(&w.own).unwrap();
    fs::write(
        w.own.join("auth.json"),
        r#"{"openai":{"type":"api_key","key":"k"}}"#,
    )
    .unwrap();
    let list = w.models(PiTarget::Native).await;
    assert_eq!(list.error, None);
    assert_eq!(list.target, PiTarget::Native);
    let ids: Vec<&str> = list.providers.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids[0], "openai", "signed in comes first: {ids:?}");
    let openai = &list.providers[0];
    assert!(openai.signed_in && openai.oauth && openai.api_key);
    assert_eq!(openai.oauth_label.as_deref(), Some("Sign in with ChatGPT"));
    let opencode = list.providers.iter().find(|p| p.id == "opencode").unwrap();
    assert!(!opencode.signed_in);
    // What can't be given back to Pi as one argument isn't offered.
    let models: Vec<&str> = opencode.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(models, ["deepseek-v4.1-flash", "gpt-5"]);
    let bedrock = list.providers.iter().find(|p| p.id == "amazon-bedrock");
    assert!(bedrock.unwrap().ambient);

    let args = w.last_run("rpc.args");
    for flag in [
        "--no-extensions",
        "--no-session",
        "--no-approve",
        "--no-mcp",
    ] {
        assert!(args.lines().any(|l| l == flag), "{flag}: {args}");
    }
    let env = w.last_run("rpc.env");
    assert!(!env.contains("stray"), "{env}");
    assert!(env.contains("PI_OFFLINE=1"), "{env}");
    // It wasn't run anywhere a project could speak to it.
    let cwd = w.last_run("rpc.cwd");
    assert!(cwd.contains(".pi-setup"), "{cwd}");
    w.no_scratch_left();
}

#[tokio::test]
async fn a_list_is_kept_until_what_it_comes_from_changes() {
    let w = world().await;
    let first = w.models(PiTarget::Native).await;
    assert!(w.own.join("rpc.args").is_file());
    fs::remove_file(w.own.join("rpc.args")).unwrap();
    assert_eq!(w.models(PiTarget::Native).await, first);
    assert!(!w.own.join("rpc.args").exists(), "it asked Pi again");
    fs::write(
        w.own.join("auth.json"),
        r#"{"opencode":{"type":"api_key","key":"k"}}"#,
    )
    .unwrap();
    let after = w.models(PiTarget::Native).await;
    assert!(w.own.join("rpc.args").is_file(), "it should ask again");
    let opencode = after.providers.iter().find(|p| p.id == "opencode");
    assert!(opencode.unwrap().signed_in);
}

#[tokio::test]
async fn a_key_in_pi_env_counts_and_a_stray_one_never_does() {
    let w = world().await;
    let path = w.layout.pi_env_file();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "OPENCODE_API_KEY=mine\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let list = w.models(PiTarget::Native).await;
    let opencode = list.providers.iter().find(|p| p.id == "opencode");
    assert!(opencode.unwrap().signed_in);
}

#[tokio::test]
async fn linux_agents_list_goes_by_the_store_on_the_mac() {
    let w = world().await;
    // The owner's own login is nothing to do with them.
    fs::create_dir_all(&w.own).unwrap();
    fs::write(
        w.own.join("auth.json"),
        r#"{"openai":{"type":"api_key","key":"k"}}"#,
    )
    .unwrap();
    let store = w.layout.linux_pi_logins_dir();
    fs::create_dir_all(&store).unwrap();
    fs::write(
        store.join("auth.json"),
        r#"{"anthropic":{"type":"api_key","key":"k"}}"#,
    )
    .unwrap();
    let list = w.models(PiTarget::Vm).await;
    assert_eq!(list.target, PiTarget::Vm);
    let signed: Vec<&str> = list
        .providers
        .iter()
        .filter(|p| p.signed_in)
        .map(|p| p.id.as_str())
        .collect();
    assert_eq!(signed, ["anthropic"]);
    // And it hasn't been run with the owner's own directory.
    assert!(!w.own.join("rpc.args").exists());
    w.no_scratch_left();
}

#[tokio::test]
async fn a_host_with_no_vms_takes_either_target_to_mean_its_own() {
    let golden = Golden::new();
    let mut config = golden.config();
    config.info.os = "linux".into();
    config.layout = Layout::new(&golden.home, "linux");
    config.toolbox = None;
    let core = Core::open(config).unwrap();
    let list = core
        .pi_models(PiModelsParams {
            host: None,
            target: PiTarget::Vm,
        })
        .await
        .unwrap();
    assert_eq!(list.target, PiTarget::Native);
    let login = core
        .pi_login_start(start("opencode", PiLoginMethod::ApiKey, PiTarget::Vm))
        .await
        .unwrap();
    assert_eq!(login.target, PiTarget::Native);
}

#[tokio::test]
async fn a_key_goes_to_pis_own_store_for_native_agents_and_is_never_shown() {
    let w = world().await;
    let mut events = w.core.subscribe().1;
    let login = w
        .core
        .pi_login_start(start("opencode", PiLoginMethod::ApiKey, PiTarget::Native))
        .await
        .unwrap();
    assert_eq!(login.state, PiLoginState::Running);
    let prompt = login.prompt.as_ref().unwrap();
    assert_eq!(prompt.kind, PiPromptKind::Secret);
    let args = w.last_run("rpc.args");
    assert!(args.lines().any(|l| l == "--no-extensions"), "{args}");
    let env = w.last_run("rpc.env");
    assert!(!env.contains("stray"), "{env}");
    assert!(
        !env.contains("PI_OFFLINE"),
        "signing in needs the net: {env}"
    );

    let done = w
        .core
        .pi_login_answer(answer(&login, "sk-the-key"))
        .await
        .unwrap();
    assert_eq!(done.state, PiLoginState::Done);
    assert!(
        !serde_json::to_string(&done).unwrap().contains("sk-the-key"),
        "{done:?}"
    );
    assert!(w.own_auth().contains("sk-the-key"));
    assert!(w.store_auth().is_empty(), "that's not linux agents' login");
    let event = events.try_recv().unwrap();
    assert_eq!(event.kind, vibe_core::model::EventKind::PiChanged);
    w.no_scratch_left();

    // And the next list knows.
    let list = w.models(PiTarget::Native).await;
    let opencode = list.providers.iter().find(|p| p.id == "opencode");
    assert!(opencode.unwrap().signed_in);
}

#[tokio::test]
async fn a_key_for_linux_agents_lands_in_the_store_and_goes_through_its_rules() {
    let w = world().await;
    let login = w
        .core
        .pi_login_start(start("opencode", PiLoginMethod::ApiKey, PiTarget::Vm))
        .await
        .unwrap();
    assert_eq!(login.target, PiTarget::Vm);
    let done = w
        .core
        .pi_login_answer(answer(&login, "sk-for-vms"))
        .await
        .unwrap();
    assert_eq!(done.state, PiLoginState::Done, "{:?}", done.error);
    assert!(w.store_auth().contains("sk-for-vms"), "{}", w.store_auth());
    assert!(w.own_auth().is_empty(), "that's not the owner's own login");
    w.no_scratch_left();
    // It's the owner's own: no agent's offer to be told of.
    let status = w
        .core
        .pi_status(vibe_core::rpc::HostParams { host: None })
        .await
        .unwrap();
    let vms = status.vms.unwrap();
    assert_eq!(vms.logins, ["opencode"]);
    assert!(vms.offered.is_empty(), "{:?}", vms.offered);
    let list = w.models(PiTarget::Vm).await;
    let opencode = list.providers.iter().find(|p| p.id == "opencode");
    assert!(opencode.unwrap().signed_in);

    // A key that would run something on a VM goes nowhere, and says so.
    let login = w
        .core
        .pi_login_start(start("deepseek", PiLoginMethod::ApiKey, PiTarget::Vm))
        .await
        .unwrap();
    let refused = w
        .core
        .pi_login_answer(answer(&login, "!cat ~/.secret-key"))
        .await
        .unwrap();
    assert_eq!(refused.state, PiLoginState::Failed);
    let why = refused.error.unwrap();
    assert!(why.contains("run a command"), "{why}");
    assert!(!w.store_auth().contains("deepseek"), "{}", w.store_auth());
    w.no_scratch_left();
}

#[tokio::test]
async fn a_browser_login_shows_its_page_and_only_a_good_answer_ends_it() {
    let w = world().await;
    let login = w
        .core
        .pi_login_start(start("anthropic", PiLoginMethod::Oauth, PiTarget::Native))
        .await
        .unwrap();
    let prompt = login.prompt.as_ref().unwrap();
    assert_eq!(prompt.kind, PiPromptKind::Select);
    assert_eq!(prompt.options.len(), 2);
    // It has to be one of the choices, and it stays asked.
    let e = w
        .core
        .pi_login_answer(answer(&login, "anything"))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams.code());
    let page = w
        .core
        .pi_login_answer(answer(&login, "browser"))
        .await
        .unwrap();
    assert!(page.url.as_deref().unwrap().starts_with("https://"));
    assert_eq!(
        page.instructions.as_deref(),
        Some("Complete sign-in in your browser.")
    );
    let asked = page.prompt.as_ref().unwrap();
    assert_eq!(asked.kind, PiPromptKind::ManualCode);
    // The question that's over can't be answered again.
    let e = w
        .core
        .pi_login_answer(answer(&login, "browser"))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict.code());
    // Nor is it asked twice over.
    let again = w
        .core
        .pi_login_get(PiLoginGetParams {
            host: None,
            id: page.id.clone(),
            seen: Some(0),
        })
        .await
        .unwrap();
    assert_eq!(again, page);
    let done = w
        .core
        .pi_login_answer(answer(&page, "http://localhost/callback?code=good"))
        .await
        .unwrap();
    assert_eq!(done.state, PiLoginState::Done);
    assert_eq!(done.url, None);
    assert_eq!(done.prompt, None);
    assert!(w.own_auth().contains("code=good"));
}

#[tokio::test]
async fn one_that_goes_wrong_says_how_and_leaves_nothing() {
    let w = world().await;
    let failed = w
        .core
        .pi_login_start(start("no-such", PiLoginMethod::Oauth, PiTarget::Native))
        .await
        .unwrap();
    assert_eq!(failed.state, PiLoginState::Failed);
    assert_eq!(failed.error.as_deref(), Some("Unknown provider: no-such"));
    let gone = w
        .core
        .pi_login_start(start("goes-away", PiLoginMethod::Oauth, PiTarget::Native))
        .await
        .unwrap();
    assert_eq!(gone.state, PiLoginState::Failed);
    assert!(gone.error.unwrap().contains("Pi stopped"));
    w.no_scratch_left();
    // A name that can't be a provider's never gets as far as Pi.
    for bad in ["", "a b", "x\ny", "--flag;rm"] {
        let e = w
            .core
            .pi_login_start(start(bad, PiLoginMethod::ApiKey, PiTarget::Native))
            .await
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidParams.code(), "{bad}");
    }
    let e = w
        .core
        .pi_login_get(PiLoginGetParams {
            host: None,
            id: "nope".into(),
            seen: None,
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound.code());
}

#[tokio::test]
async fn giving_up_ends_pi_and_a_new_one_ends_the_last() {
    let w = world().await;
    let login = w
        .core
        .pi_login_start(start("opencode", PiLoginMethod::ApiKey, PiTarget::Vm))
        .await
        .unwrap();
    let over = w
        .core
        .pi_login_cancel(PiLoginParams {
            host: None,
            id: login.id.clone(),
        })
        .await
        .unwrap();
    assert_eq!(over.state, PiLoginState::Cancelled);
    assert!(w.store_auth().is_empty());
    w.no_scratch_left();
    // Over already, it stays as it ended.
    let still = w
        .core
        .pi_login_cancel(PiLoginParams {
            host: None,
            id: login.id,
        })
        .await
        .unwrap();
    assert_eq!(still.state, PiLoginState::Cancelled);

    let first = w
        .core
        .pi_login_start(start("opencode", PiLoginMethod::ApiKey, PiTarget::Native))
        .await
        .unwrap();
    let second = w
        .core
        .pi_login_start(start("deepseek", PiLoginMethod::ApiKey, PiTarget::Native))
        .await
        .unwrap();
    assert_ne!(first.id, second.id);
    let e = w
        .core
        .pi_login_answer(answer(&first, "sk-first"))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound.code());
    let done = w
        .core
        .pi_login_answer(answer(&second, "sk-second"))
        .await
        .unwrap();
    assert_eq!(done.state, PiLoginState::Done);
    assert!(!w.own_auth().contains("sk-first"));
}

#[tokio::test]
async fn an_answer_too_long_is_refused() {
    let w = world().await;
    let login = w
        .core
        .pi_login_start(start("opencode", PiLoginMethod::ApiKey, PiTarget::Native))
        .await
        .unwrap();
    let e = w
        .core
        .pi_login_answer(answer(&login, &"k".repeat(70_000)))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams.code());
}

#[tokio::test]
async fn with_no_pi_at_all_the_pinned_one_is_got_and_linked_nowhere() {
    let releases = tempfile::tempdir().unwrap();
    let platform = pi_release::platform_here().unwrap();
    publish_pi(releases.path(), pi_release::PINNED, platform);
    let w = world_with(|config| {
        config.pi_driver = None;
        config.pi_releases = format!("file://{}", releases.path().display());
        config.checks = Arc::new(vibed::doctor::FixedChecks {
            pi: Err("no pi".into()),
            ..common::golden_checks()
        });
    });
    // Its `pi` is only a script that says its version, so it gives no list, in words.
    let list = w.models(PiTarget::Vm).await;
    assert!(list.providers.is_empty());
    assert!(list.error.unwrap().contains("Pi"));
    let kept = w
        .layout
        .data_dir()
        .join(format!("pi-{}/pi", pi_release::PINNED));
    assert!(kept.is_file());
    assert!(
        !w.layout.home().join(".local/bin/pi").exists(),
        "it's for VibeOS, and nobody asked for a `pi` on their PATH"
    );
}

#[tokio::test]
async fn with_no_pi_to_be_had_a_list_says_why_and_a_sign_in_fails() {
    let w = world_with(|config| {
        config.pi_driver = None;
        config.pi_releases = "file:///nowhere".to_owned();
        config.checks = Arc::new(vibed::doctor::FixedChecks {
            pi: Err("no pi".into()),
            ..common::golden_checks()
        });
    });
    let list = w.models(PiTarget::Native).await;
    assert!(list.error.unwrap().contains("can't get Pi"));
    let e = w
        .core
        .pi_login_start(start("opencode", PiLoginMethod::ApiKey, PiTarget::Native))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::CommandFailed.code());
}

#[tokio::test]
async fn a_pi_of_some_other_version_isnt_asked() {
    let w = world_with(|config| {
        config.pi_driver = None;
        config.pi_releases = "file:///nowhere".to_owned();
        config.checks = Arc::new(vibed::doctor::FixedChecks {
            pi: Ok(("/bin/false".into(), "0.1.0".into())),
            ..common::golden_checks()
        });
    });
    let list = w.models(PiTarget::Native).await;
    assert!(list.error.unwrap().contains("can't get Pi"));
}
