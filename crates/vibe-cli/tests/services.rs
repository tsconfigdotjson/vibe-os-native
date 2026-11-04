//! Shared MCP services with the real Claude Code: `claude -p --mcp-config`, as
//! a launch offers them, through the real `vibe mcp-proxy` and this host's
//! broker, to a stand-in for the service. It spends a little of the owner's
//! model usage, so it runs only when asked:
//!
//! ```text
//! VIBE_TEST_REAL_CLAUDE=1 CLAUDE_CODE_OAUTH_TOKEN=… cargo test -p vibe-cli --test services -- --nocapture
//! ```
//!
//! scripts/check-full.sh does, where there's a `claude`. Without either, it
//! says so and passes without having checked anything. The real Linear is never
//! involved.

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use vibe_core::model::ServiceState;
use vibe_services::secrets::MemoryStore;
use vibe_services::testing::{FakeService, SECRET_MARK};
use vibe_services::{Deps, Place, Services, relay};

const VIBE: &str = env!("CARGO_BIN_EXE_vibe");

#[tokio::test(flavor = "multi_thread")]
async fn real_claude_calls_a_shared_service_through_the_shim() {
    if std::env::var_os("VIBE_TEST_REAL_CLAUDE").is_none() {
        eprintln!("skipped: set VIBE_TEST_REAL_CLAUDE=1 to spend a model call on this");
        return;
    }
    let claude = std::env::var("VIBE_CLAUDE").unwrap_or_else(|_| "claude".to_owned());
    if Command::new(&claude)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("skipped: no {claude} here");
        return;
    }

    let fake = FakeService::start().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::default());
    let services = Services::new(
        vec![fake.service()],
        store.clone(),
        dir.path().join("services.json"),
        Deps {
            clock: Arc::new(vibe_core::time::now),
            random: Arc::new(vibe_core::keys::random_bytes),
            changed: Arc::new(|_| {}),
            audit: Arc::new(|line| eprintln!("{line}")),
            place: Place::LinuxHost,
            token_wait: Duration::from_secs(1),
        },
    );
    let auth = services.connect("linear").await.unwrap();
    let back = fake.sign_in(&auth.url).await;
    let info = services.complete("linear", &back).await.unwrap();
    let login = vibe_services::SecretStore::load(&*store, "linear")
        .unwrap()
        .expect("a login");
    assert_eq!(info.state, ServiceState::Connected);

    let socket = dir.path().join("services.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let broker = services.clone();
    tokio::spawn(async move {
        while let Ok((conn, _)) = listener.accept().await {
            let admit: relay::Admit = Arc::new(|_, _| Ok(()));
            tokio::spawn(relay::serve_local(broker.clone(), conn, admit));
        }
    });

    // As a launch offers it, with the one allow rule a test needs to go without
    // a prompt. Signed in by token, in a home of its own: never the owner's login,
    // which on a Mac is in the Keychain.
    let token = std::env::var("CLAUDE_CODE_OAUTH_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .expect("CLAUDE_CODE_OAUTH_TOKEN: this signs in with a token, never the owner's login");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let config = serde_json::json!({"mcpServers": {"vibe-linear": {
        "type": "stdio",
        "command": VIBE,
        "args": ["mcp-proxy", "linear"],
    }}})
    .to_string();
    let cwd = dir.path().to_owned();
    let cache = dir.path().join("cache");
    let started = Instant::now();
    let out = tokio::task::spawn_blocking(move || {
        Command::new(&claude)
            .args(["-p", "Call the tool mcp__vibe-linear__list_issues once, with no arguments, then repeat exactly the text it returned."])
            .args(["--model", "haiku", "--output-format", "text"])
            .args(["--mcp-config", &config])
            .args(["--allowedTools", "mcp__vibe-linear__*"])
            .current_dir(&cwd)
            .env("HOME", &home)
            .env("CLAUDE_CODE_OAUTH_TOKEN", &token)
            .env("VIBE_SERVICES_SOCKET", cwd.join("services.sock"))
            .env("VIBE_SERVICES_CACHE", &cache)
            .env("VIBE_AGENT", "real-claude")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    eprintln!(
        "claude took {:?}: {said}\n{}",
        started.elapsed(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{out:?}");

    // The service was really called, with the Bearer that only the broker has.
    let deadline = Instant::now() + Duration::from_secs(5);
    while fake.calls().is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let call = fake.calls().pop().expect("the tool was called");
    assert_eq!(call.tool.as_deref(), Some("list_issues"));
    assert_eq!(call.bearer.as_deref(), Some(login.access.as_str()));
    assert!(said.contains("issues on /mcp"), "{said}");
    assert!(!said.contains(SECRET_MARK), "{said}");
}
