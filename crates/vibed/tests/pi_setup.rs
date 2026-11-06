//! Pi's list, for the app, through the core against the stand-in for Pi. The real `pi`
//! does the same in `pi_e2e.rs`, where there is one. VibeOS signs nobody in to Pi, so
//! these also hold that a login that's there is used and left as it is.

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Once};

use common::{Golden, publish_pi};
use serde_json::{Value, json};
use vibe_core::model::PiTarget;
use vibe_core::paths::Layout;
use vibe_core::rpc::{ErrorCode, PiModels, PiModelsParams};
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

    /// One call through the dispatcher, as a socket client makes it.
    async fn rpc(&self, method: &str, params: Value) -> Value {
        let line =
            json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
        let reply = vibed::dispatch::handle_message(
            &self.core,
            &mut vibed::dispatch::Session::default(),
            line.as_bytes(),
        )
        .await
        .unwrap();
        serde_json::from_str(&reply).unwrap()
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
    assert!(openai.signed_in);
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
}

/// Pi's sign-in was the app's until #158. It's gone from the socket, as if it never was,
/// and what's kept answers.
#[tokio::test]
async fn nothing_on_the_socket_signs_anyone_in_to_pi() {
    let w = world().await;
    for (method, params) in [
        (
            "pi.login.start",
            json!({"target": "native", "provider": "opencode", "method": "api_key"}),
        ),
        ("pi.login.get", json!({"id": "x"})),
        (
            "pi.login.answer",
            json!({"id": "x", "prompt": "p", "value": "sk-made-up"}),
        ),
        ("pi.login.cancel", json!({"id": "x"})),
    ] {
        let said = w.rpc(method, params).await;
        assert_eq!(
            said["error"]["code"],
            ErrorCode::MethodNotFound.code(),
            "{method}: {said}"
        );
    }
    assert!(
        !w.own.join("auth.json").exists(),
        "nothing wrote a login for any of that"
    );
    let list = w.rpc("pi.models", json!({"target": "native"})).await;
    let providers = list["result"]["providers"].as_array().expect("a list");
    assert!(providers.iter().any(|p| p["id"] == "opencode"), "{list}");
    // How a provider signs in is no longer anything the app is told.
    for provider in providers {
        for gone in ["oauth", "oauth_label", "api_key"] {
            assert!(provider.get(gone).is_none(), "{gone}: {provider}");
        }
    }
    let status = w.rpc("pi.status", json!({})).await;
    assert!(status.get("result").is_some(), "{status}");
}

/// What Pi has already is what's used: a list says so with no step of VibeOS's, and
/// looking at it leaves every login, for either kind of agent, as it was, to the byte.
#[tokio::test]
async fn logins_already_there_are_seen_and_left_exactly_as_they_are() {
    let w = world().await;
    let own = r#"{"openai":{"type":"oauth","access":"a","refresh":"r","expires":4102444800000}}"#;
    fs::create_dir_all(&w.own).unwrap();
    fs::write(w.own.join("auth.json"), own).unwrap();
    let stored = r#"{"anthropic":{"type":"api_key","key":"k"}}"#;
    let store = w.layout.linux_pi_logins_dir();
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join("auth.json"), stored).unwrap();

    let signed = |list: &PiModels| -> Vec<String> {
        let signed = list.providers.iter().filter(|p| p.signed_in);
        signed.map(|p| p.id.clone()).collect()
    };
    assert_eq!(signed(&w.models(PiTarget::Native).await), ["openai"]);
    assert_eq!(signed(&w.models(PiTarget::Vm).await), ["anthropic"]);
    let status = w.rpc("pi.status", json!({})).await;
    assert_eq!(status["result"]["logins"], json!(["openai"]), "{status}");
    assert_eq!(
        status["result"]["vms"]["logins"],
        json!(["anthropic"]),
        "{status}"
    );

    assert_eq!(fs::read_to_string(w.own.join("auth.json")).unwrap(), own);
    assert_eq!(fs::read_to_string(store.join("auth.json")).unwrap(), stored);
    w.no_scratch_left();
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
async fn with_no_pi_to_be_had_a_list_says_why() {
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
