//! The Done-when of #9, on a Mac with real VMs: a `linux` agent started
//! from the starter profile answers a prompt in `auto` mode on Opus, sleeps
//! (its VM saved to disk), and wakes to carry on the same conversation. It
//! boots the base image `scripts/base-image.sh` built, with Claude Code's
//! managed layer downloaded and checked the way `vibed` does it.
//!
//! ```text
//! VIBE_VM_BIN=build/vibe-vm VIBE_BASE_RELEASES=file://$PWD/build/base-image \
//!   CLAUDE_CODE_OAUTH_TOKEN=… cargo test -p vibed --test linux_vm -- --nocapture
//! ```
//!
//! Without those it skips. `scripts/check-full.sh` runs it with
//! `VIBE_VM_MARKER` set, so a skip can't pass for a pass. It needs
//! `cargo build --workspace` first, for `vibe-shim`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Golden, workspace_bin};
use vibe_core::claude_token::TOKEN_ENV;
use vibe_core::model::Environment;
use vibe_core::paths::Layout;
use vibe_core::rpc::{AgentDeleteParams, AgentStartParams, HostParams, ProjectAddParams};
use vibe_core::shim::{self, Frame, FrameKind, control_hello};
use vibe_env::linux::{LinuxVm, Real, TokenSource};
use vibe_env::{Handle, Sleeper, vm};
use vibed::agents::Backends;
use vibed::core::{Config, Core, DaemonInfo};

fn absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(path)
    }
}

fn mark(test: &str) {
    if let Some(dir) = std::env::var_os("VIBE_VM_MARKER") {
        let dir = absolute(PathBuf::from(dir));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(test), "ran\n").unwrap();
    }
}

/// The assistant's text in `transcript`, every message joined.
fn replies(transcript: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(transcript) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["type"] == "assistant")
        .filter_map(|v| {
            let content = v["message"]["content"].as_array()?.clone();
            Some(
                content
                    .iter()
                    .filter_map(|c| c["text"].as_str())
                    .collect::<Vec<_>>()
                    .join(""),
            )
        })
        .filter(|t| !t.is_empty())
        .collect()
}

fn wait_for(what: &str, timeout: Duration, mut ok: impl FnMut() -> bool, why: impl Fn() -> String) {
    let deadline = Instant::now() + timeout;
    while !ok() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}\n{}",
            why()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_linux_agent_answers_on_opus_and_carries_on_after_sleep() {
    let (Some(vibe_vm), Some(base), Ok(token)) = (
        std::env::var_os("VIBE_VM_BIN"),
        std::env::var("VIBE_BASE_RELEASES")
            .ok()
            .filter(|v| !v.is_empty()),
        std::env::var(TOKEN_ENV),
    ) else {
        eprintln!(
            "a_linux_agent_answers_on_opus_and_carries_on_after_sleep: skipped; set VIBE_VM_BIN, \
             VIBE_BASE_RELEASES and {TOKEN_ENV} to run it on a Mac"
        );
        return;
    };
    let vibe_vm = absolute(PathBuf::from(vibe_vm));
    let golden = Golden::new();
    let layout = Layout::new(&golden.home, "macos");

    // The images, as vibed fetches them when it starts.
    let sources = vibed::linux::Sources {
        base_releases: base,
        ..vibed::linux::Sources::from_env()
    };
    let images = vibe_env::images::Images::new(layout.linux_dir());
    vibed::linux::refresh(&images, &layout.agents_dir(), &sources);
    let layers = images.layers().expect("the base image installs");
    let (claude, managed) = layers.managed.clone().expect("Claude's layer downloads");
    eprintln!("base {} with Claude {claude}", layers.base.version);

    let token: TokenSource = Arc::new(move || Ok(token.clone()));
    let backend = Arc::new(LinuxVm::with_machine(
        layout.clone(),
        Arc::new(Real { vibe_vm }),
        token,
    ));
    let mut config = Config::new(DaemonInfo::current(), layout.clone());
    config.gh = golden.gh.clone().into();
    config.shim = workspace_bin("vibe-shim").into_os_string();
    config.backends = Backends::new().with(Environment::Linux, backend.clone());
    let core = Arc::new(Core::open(config).unwrap());
    let project = golden.home.join("src/app");
    core.add_project(ProjectAddParams {
        path: Some(project.to_str().unwrap().into()),
        ..Default::default()
    })
    .await
    .unwrap();

    // The starter profile as it comes: Opus, high, auto, 4 GB.
    let a = core
        .start_agent(AgentStartParams {
            project: "app".into(),
            profile: "software-eng-linux".into(),
            prompt: "Reply with exactly the word PONG and nothing else. Use no tools.".into(),
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("start: {e}"));
    let r = a.agent.clone();
    let agent_dir = layout.agent_dir(&r);
    let logs = || {
        ["console.log", "vibe-vm.log"]
            .iter()
            .map(|f| {
                format!(
                    "--- {f}\n{}",
                    fs::read_to_string(agent_dir.join(f)).unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let screen = || {
        shim::connect(
            &layout.session_socket(&r),
            &control_hello("linux_vm"),
            Duration::from_secs(5),
        )
        .and_then(|mut c| Ok(c.read_output(None)?.screen))
        .unwrap_or_else(|e| format!("(no screen: {e})"))
    };
    let config = vm::VmConfig::load(&agent_dir.join("vm.json")).unwrap();
    assert!(
        config.disks.iter().any(|d| d.path == managed),
        "boots the newest Claude layer"
    );
    let socket = layout.vm_socket(&r);
    let status = vibe_env::guest::status(&socket).unwrap();
    assert!(
        status.problems.is_empty(),
        "{:?}\n{}",
        status.problems,
        logs()
    );
    let command = status.session.expect("Claude runs").command;
    eprintln!("Claude runs as {command:?}");
    let has = |pair: [&str; 2]| command.windows(2).any(|w| w == pair);
    assert!(has(["--model", "opus"]), "{command:?}");
    assert!(has(["--permission-mode", "auto"]), "{command:?}");

    let handle: Handle = {
        let record = vibe_core::store::Store::open(&layout.db())
            .unwrap()
            .agent("local", &r)
            .unwrap();
        serde_json::from_str(&record.compartment).unwrap()
    };
    let transcript = handle
        .claude
        .transcript(Path::new(&a.worktree), &a.session_id);
    wait_for(
        "PONG in the transcript",
        Duration::from_secs(240),
        || {
            replies(&transcript)
                .iter()
                .any(|t| t.to_lowercase().contains("pong"))
        },
        || format!("the screen:\n{}\n{}", screen(), logs()),
    );
    let text = fs::read_to_string(&transcript).unwrap();
    assert!(text.contains("\"model\":\"claude-opus"), "answered on Opus");
    // Nothing about where it's installed or how it updates: the managed
    // layer does that (PRD: Spike results).
    let shown = screen().to_lowercase();
    for warning in [
        "claude install",
        "installmethod",
        "auto-update failed",
        "not in your path",
        "native installation",
    ] {
        assert!(
            !shown.contains(warning),
            "Claude warned ({warning}):\n{shown}"
        );
    }
    let boot_id = vibe_env::guest::status(&socket).unwrap().boot_id;

    // Asleep: the VM saved to disk, its RAM given back.
    let h = handle.clone();
    let b = backend.clone();
    tokio::task::spawn_blocking(move || b.sleep(&h))
        .await
        .unwrap()
        .unwrap_or_else(|e| panic!("sleep: {e}\n{}", logs()));
    assert!(layout.snapshot(&r).is_file());
    assert!(!vm::lock_held(&socket), "vibe-vm exited");

    // A wake restores it on a new shim, and the conversation carries on.
    core.resume_agent(None, &r)
        .await
        .unwrap_or_else(|e| panic!("wake: {e}\n{}", logs()));
    assert!(!layout.snapshot(&r).exists(), "the snapshot is used up");
    // The same boot, restored, not a fresh VM resuming the conversation.
    assert_eq!(
        vibe_env::guest::status(&socket).unwrap().boot_id,
        boot_id,
        "the wake was a fresh boot\n{}",
        logs()
    );
    let mut c = shim::connect(
        &layout.session_socket(&r),
        &control_hello("linux_vm"),
        Duration::from_secs(10),
    )
    .unwrap();
    let ask =
        "What word did you just reply with? Answer with that word in lowercase, nothing else.";
    shim::write_frame(&mut c.stream, &Frame::new(FrameKind::Input, ask)).unwrap();
    std::thread::sleep(Duration::from_millis(800));
    shim::write_frame(&mut c.stream, &Frame::new(FrameKind::Input, "\r")).unwrap();
    c.status().unwrap();
    // The first answer, and a second, after the wake, that remembers it:
    // each says "pong", however it's written.
    let pongs = || {
        replies(&transcript)
            .iter()
            .filter(|t| t.to_lowercase().contains("pong"))
            .count()
    };
    wait_for(
        "pong after the wake",
        Duration::from_secs(240),
        || pongs() >= 2,
        || format!("the screen:\n{}\n{}", screen(), logs()),
    );
    let list = core.agents(HostParams::default()).await.unwrap();
    eprintln!("after the wake: {:?}", list.agents[0].status);

    core.delete_agent(AgentDeleteParams {
        host: None,
        agent: r.clone(),
        force: Some(true),
    })
    .await
    .unwrap();
    assert!(!agent_dir.exists() && !layout.snapshot(&r).exists());
    assert!(!vm::lock_held(&socket));
    mark("linux_agent");
}
