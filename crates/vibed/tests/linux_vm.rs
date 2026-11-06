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

#[path = "common/mock_model.rs"]
mod mock_model;

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
use vibe_env::guest::proto::{Ran, Run};
use vibe_env::linux::{LinuxVm, Machine, Real, TokenSource};
use vibe_env::{Handle, Sleeper, vm};
use vibe_services::secrets::MemoryStore;
use vibe_services::testing::FakeService;
use vibed::agents::Backends;
use vibed::core::{Config, Core, DaemonInfo};
use vibed::dispatch::{Session, handle_message};
use vibed::services::ServicesConfig;

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

/// Stops whatever VMs this test's agents still have when it ends, panicking included: a
/// `vibe-vm boot` is detached, and outlives the test and its temporary home otherwise. To be
/// made after the [`Golden`], so it's dropped before that home goes.
struct StopVms {
    layout: Layout,
    vibe_vm: PathBuf,
}

impl Drop for StopVms {
    fn drop(&mut self) {
        let Ok(agents) = fs::read_dir(self.layout.agents_dir()) else {
            return;
        };
        let machine = Real {
            vibe_vm: self.vibe_vm.clone(),
        };
        for agent in agents.flatten() {
            let socket = self.layout.vm_socket(&agent.file_name().to_string_lossy());
            if machine.running(&socket)
                && let Err(e) = machine.stop(&socket)
            {
                eprintln!("can't stop the VM at {}: {e}", socket.display());
            }
        }
    }
}

/// What a guest can say of why something in it went wrong: what its kernel saw, as a crash can
/// leave nothing on the screen, and how its session ended, if it has.
fn guest_says(socket: &Path) -> String {
    let dmesg = match vibe_env::guest::run(socket, &sh("dmesg | tail -n 40")) {
        Ok(ran) => format!("{}{}", how_it_ended(&ran), ran.output),
        Err(e) => format!("(can't ask: {e})"),
    };
    let session = vibe_env::guest::status(socket).map(|s| s.session.and_then(|s| s.exit));
    format!("--- dmesg | tail\n{dmesg}\n--- its session's exit\n{session:?}")
}

fn how_it_ended(ran: &Ran) -> String {
    format!(
        "(code {:?}, signal {:?}, timed out: {}) ",
        ran.code, ran.signal, ran.timed_out
    )
}

/// `command` in a shell in the guest, as long as this test's commands get.
fn sh(command: &str) -> Run {
    Run {
        command: vec!["/bin/sh".into(), "-c".into(), command.into()],
        env: Default::default(),
        cwd: None,
        timeout_ms: 10_000,
    }
}

/// Runs something in the guest that may well exit non-zero. A crash, or running out of time,
/// is never what's being tested, and says so itself rather than as whatever comes of it later.
fn guest_ran(socket: &Path, run: &Run) -> Ran {
    let ran = vibe_env::guest::run(socket, run).unwrap_or_else(|e| {
        panic!(
            "can't run {:?} in the guest: {e}\n{}",
            run.command,
            guest_says(socket)
        )
    });
    if ran.signal.is_some() || ran.timed_out {
        panic!(
            "{:?} crashed in the guest {}: {}\n{}",
            run.command,
            how_it_ended(&ran),
            ran.output,
            guest_says(socket)
        );
    }
    ran
}

/// Runs something in the guest that this test relies on having worked.
fn guest_ok(socket: &Path, run: &Run) -> Ran {
    let ran = guest_ran(socket, run);
    assert_eq!(
        ran.code,
        Some(0),
        "{:?} failed in the guest {}: {}\n{}",
        run.command,
        how_it_ended(&ran),
        ran.output,
        guest_says(socket)
    );
    ran
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
    let _stop_vms = StopVms {
        layout: layout.clone(),
        vibe_vm: vibe_vm.clone(),
    };

    // The images, as vibed fetches them when it starts.
    let sources = vibed::linux::Sources {
        base_releases: base,
        ..vibed::linux::Sources::from_env()
    };
    let images = vibe_env::images::Images::new(layout.linux_dir());
    vibed::linux::refresh(&images, &layout.agents_dir(), &sources, false);
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
        .claude()
        .unwrap()
        .transcript(Path::new(&a.worktree), &a.session_id);
    wait_for(
        "PONG in the transcript",
        Duration::from_secs(240),
        || {
            replies(&transcript)
                .iter()
                .any(|t| t.to_lowercase().contains("pong"))
        },
        || {
            format!(
                "the screen:\n{}\n{}\n{}",
                screen(),
                logs(),
                guest_says(&socket)
            )
        },
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
        || {
            format!(
                "the screen:\n{}\n{}\n{}",
                screen(),
                logs(),
                guest_says(&socket)
            )
        },
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

/// A Pi agent in a real VM: the checked Pi layer, on a mock model the guest
/// reaches through its NAT gateway, with no provider and no login. It opens
/// straight to its prompt, writes its session where the host reads it, sleeps
/// and wakes on the same conversation, resumes the same session in a fresh boot,
/// and shares its one agent dir with another Pi VM. The Claude token isn't in
/// its environment. Takes `VIBE_VM_BIN` and `VIBE_BASE_RELEASES`, as the test
/// above does, and needs no Claude token: none is given.
#[tokio::test(flavor = "multi_thread")]
async fn a_pi_agent_answers_in_a_vm_and_shares_the_one_login() {
    let (Some(vibe_vm), Some(base)) = (
        std::env::var_os("VIBE_VM_BIN"),
        std::env::var("VIBE_BASE_RELEASES")
            .ok()
            .filter(|v| !v.is_empty()),
    ) else {
        eprintln!(
            "a_pi_agent_answers_in_a_vm…: skipped; set VIBE_VM_BIN and VIBE_BASE_RELEASES to run it \
             on a Mac"
        );
        return;
    };
    let vibe_vm = absolute(PathBuf::from(vibe_vm));
    let golden = Golden::new();
    let layout = Layout::new(&golden.home, "macos");
    let _stop_vms = StopVms {
        layout: layout.clone(),
        vibe_vm: vibe_vm.clone(),
    };
    let sources = vibed::linux::Sources {
        base_releases: base,
        ..vibed::linux::Sources::from_env()
    };
    let images = vibe_env::images::Images::new(layout.linux_dir());
    vibed::linux::refresh(&images, &layout.agents_dir(), &sources, true);
    let (pi, layer) = images.pi().expect("Pi's layer downloads");
    eprintln!("Pi {pi}");

    // The mock, on the address the VM's NAT reaches this Mac at.
    let gateway =
        std::env::var("VIBE_TEST_VM_GATEWAY").unwrap_or_else(|_| "192.168.64.1".to_owned());
    let mock = mock_model::MockModel::start("0.0.0.0");
    let shared = layout.linux_pi_dir();
    fs::create_dir_all(&shared).unwrap();
    fs::write(shared.join("models.json"), mock.models_json(&gateway)).unwrap();
    fs::write(shared.join("settings.json"), "{}\n").unwrap();
    // Loaded through its link into the read-only share, as the owner's would be.
    fs::create_dir_all(shared.join("extensions")).unwrap();
    fs::write(
        shared.join("extensions/vibe-link-check.ts"),
        "import { writeFileSync } from \"node:fs\";\n\
         export default function () { writeFileSync(\"/tmp/vibe-link-check\", \"loaded\"); }\n",
    )
    .unwrap();

    // No token to be had: a Pi agent mustn't ask for one.
    let token: TokenSource = Arc::new(|| panic!("a Pi agent asked for the Claude token"));
    let backend = Arc::new(LinuxVm::with_machine(
        layout.clone(),
        Arc::new(Real { vibe_vm }),
        token,
    ));
    let mut config = Config::new(DaemonInfo::current(), layout.clone());
    config.gh = golden.gh.clone().into();
    config.shim = workspace_bin("vibe-shim").into_os_string();
    config.backends = Backends::new().with(Environment::Linux, backend.clone());
    // The real broker, on this Mac, to a stand-in for the service: Pi in the VM
    // reaches it through the guest's own shim and the relay the Mac dials.
    let fake = FakeService::start().await;
    config.services = Some(ServicesConfig {
        services: vec![fake.service()],
        store: Arc::new(MemoryStore::default()),
        place: vibe_services::Place::Mac,
        token_wait: Duration::from_secs(1),
    });
    let core = Arc::new(Core::open(config).unwrap());
    let project = golden.home.join("src/app");
    core.add_project(ProjectAddParams {
        path: Some(project.to_str().unwrap().into()),
        ..Default::default()
    })
    .await
    .unwrap();
    core.create_profile(vibe_core::rpc::ProfileCreateParams {
        project: "app".into(),
        name: "pi-linux".into(),
        agent: Some(vibe_core::model::AgentKind::Pi),
        pi_model: Some("mock/mock-model".into()),
        ..Default::default()
    })
    .unwrap();
    let _services = core.spawn_services();
    let rpc = |method: &'static str, params: serde_json::Value| {
        let core = Arc::clone(&core);
        async move {
            let line = serde_json::json!(
                {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}
            )
            .to_string();
            let answer = handle_message(&core, &mut Session::default(), line.as_bytes())
                .await
                .expect("an answer");
            serde_json::from_str::<serde_json::Value>(&answer).unwrap()
        }
    };
    let begun = rpc("services.connect", serde_json::json!({"service": "linear"})).await;
    let back = fake
        .sign_in(begun["result"]["url"].as_str().expect("a page"))
        .await;
    let done = rpc(
        "services.complete",
        serde_json::json!({"service": "linear", "callback_url": back}),
    )
    .await;
    assert_eq!(done["result"]["state"], "connected", "{done}");
    let start = |prompt: &'static str| {
        let core = Arc::clone(&core);
        async move {
            core.start_agent(AgentStartParams {
                project: "app".into(),
                profile: "pi-linux".into(),
                prompt: prompt.into(),
                ..Default::default()
            })
            .await
            .unwrap_or_else(|e| panic!("start: {e}"))
        }
    };
    let a = start("hello").await;
    let r = a.agent.clone();
    let agent_dir = layout.agent_dir(&r);
    let config = vm::VmConfig::load(&agent_dir.join("vm.json")).unwrap();
    assert!(config.disks.iter().any(|d| d.path == layer), "the Pi layer");
    let socket = layout.vm_socket(&r);
    let status = vibe_env::guest::status(&socket).unwrap();
    assert!(status.problems.is_empty(), "{:?}", status.problems);
    let session = status.session.expect("Pi runs");
    assert!(session.command[0].ends_with("pi"), "{:?}", session.command);
    assert!(
        session
            .command
            .iter()
            .any(|w| w == vibe_env::linux::GUEST_PI_EXTENSION),
        "the layer's extension: {:?}",
        session.command
    );
    // The share mounted at all, with its guest's mount and no error.
    assert!(
        status
            .shares
            .iter()
            .any(|m| m.tag == vibe_env::pi::CONFIG_TAG && m.path == vibe_env::pi::GUEST_CONFIG_DIR),
        "{:?}",
        status.shares
    );

    // What the owner keeps can't be changed from a guest, at the share itself,
    // by any name: on a Mac's case-insensitive disk `SETTINGS.JSON` is
    // `settings.json`. Nor by a mount of its own. Its own agent dir it can write,
    // and that goes no further.
    let on_the_mac = || {
        ["settings.json", "models.json"].map(|name| fs::read_to_string(shared.join(name)).unwrap())
    };
    let before = on_the_mac();
    let script = "cd /home/vibe/.pi/config\n\
        touch made-here 2>/dev/null && echo 'WROTE made-here'\n\
        mkdir extensions 2>/dev/null && echo 'MADE extensions'\n\
        for p in settings.json models.json SETTINGS.JSON MODELS.JSON Settings.Json; do\n\
          echo x >> \"$p\" 2>/dev/null && echo \"WROTE $p\"\n\
        done\n\
        rm settings.json 2>/dev/null && echo 'REMOVED settings.json'\n\
        mv settings.json gone 2>/dev/null && echo 'MOVED settings.json'\n\
        mount -o remount,rw . 2>/dev/null && echo 'REMOUNTED'\n\
        unshare -Urm sh -c 'mount -o remount,rw . && touch made-here' 2>/dev/null && echo 'WROTE in a namespace'\n\
        cd /home/vibe/.pi/agent\n\
        for p in settings.json; do echo x >> \"$p\" 2>/dev/null && echo \"WROTE-LINK $p\"; done\n\
        test -L models.json && echo models-is-a-link\n\
        grep -q providers models.json && echo models-readable\n\
        touch auth.json && echo login-writable\n";
    let ran = guest_ok(
        &socket,
        &Run {
            command: vec!["/bin/sh".into(), "-c".into(), script.into()],
            env: Default::default(),
            cwd: Some("/home/vibe".into()),
            timeout_ms: 30_000,
        },
    );
    let said = ran.output;
    for good in ["login-writable", "models-is-a-link", "models-readable"] {
        assert!(said.contains(good), "{good}: {said}");
    }
    for bad in [
        "WROTE ",
        "WROTE-LINK",
        "MADE",
        "REMOVED",
        "MOVED",
        "REMOUNTED",
    ] {
        assert!(!said.contains(bad), "{bad}: {said}");
    }
    assert_eq!(on_the_mac(), before, "nothing changed on the Mac");
    assert!(!shared.join("made-here").exists());

    let sessions = agent_dir.join(vibe_env::pi::VM_SESSIONS);
    let answers = |n: usize| {
        let sessions = sessions.clone();
        move || {
            fs::read_dir(&sessions)
                .into_iter()
                .flatten()
                .flatten()
                .any(|e| {
                    fs::read_to_string(e.path())
                        .unwrap_or_default()
                        .matches("\"Answered.\"")
                        .count()
                        >= n
                })
        }
    };
    let screen = |r: &str| {
        shim::connect(
            &layout.session_socket(r),
            &control_hello("linux_vm"),
            Duration::from_secs(5),
        )
        .and_then(|mut c| Ok(c.read_output(None)?.screen))
        .unwrap_or_else(|e| format!("(no screen: {e})"))
    };
    wait_for(
        "Pi's answer in its session, on the host",
        Duration::from_secs(120),
        answers(1),
        || format!("the screen:\n{}\n{}", screen(&r), guest_says(&socket)),
    );
    assert!(!mock.keys.lock().unwrap().is_empty(), "it asked the mock");
    // The shared service, as a Pi agent in a VM gets it.
    core.send_input(
        None,
        &r,
        vibe_mcp::Input::Text {
            text: "call linear".into(),
            submit: true,
            force: false,
        },
    )
    .await
    .unwrap();
    wait_for(
        "Pi's call to the shared service, by way of the Mac's broker",
        Duration::from_secs(120),
        || {
            fake.calls()
                .iter()
                .any(|c| c.tool.as_deref() == Some("list_issues"))
        },
        || format!("the screen:\n{}\n{}", screen(&r), guest_says(&socket)),
    );
    let linked = guest_ok(
        &socket,
        &Run {
            command: vec!["/bin/cat".into(), "/tmp/vibe-link-check".into()],
            env: Default::default(),
            cwd: None,
            timeout_ms: 10_000,
        },
    );
    assert!(
        linked.output.contains("loaded"),
        "the owner's extension through its link: {}",
        linked.output
    );

    // Asleep, its VM saved to disk; awake, the same boot and conversation.
    let boot_id = vibe_env::guest::status(&socket).unwrap().boot_id;
    core.sleep_agent(None, &r)
        .await
        .unwrap_or_else(|e| panic!("sleep: {e}"));
    assert!(layout.snapshot(&r).is_file());
    core.resume_agent(None, &r)
        .await
        .unwrap_or_else(|e| panic!("wake: {e}"));
    assert_eq!(vibe_env::guest::status(&socket).unwrap().boot_id, boot_id);
    core.send_input(
        None,
        &r,
        vibe_mcp::Input::Text {
            text: "after a sleep".into(),
            submit: true,
            force: false,
        },
    )
    .await
    .unwrap();
    wait_for(
        "its second answer",
        Duration::from_secs(120),
        answers(2),
        || format!("the screen:\n{}\n{}", screen(&r), guest_says(&socket)),
    );
    let before: Vec<_> = fs::read_dir(&sessions)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(before.len(), 1, "{before:?}");

    // A second VM has an agent dir of its own: what the first writes there, it
    // doesn't see. It sees what the owner keeps, and what `vibed` passes on.
    let b = start("hello").await;
    let shared_run = |socket: &Path, command: &str| guest_ok(socket, &sh(command));
    let other = layout.vm_socket(&b.agent);
    shared_run(&socket, "echo from-a > /home/vibe/.pi/agent/planted");
    let looked = shared_run(&other, "cat /home/vibe/.pi/agent/planted 2>&1; echo done");
    assert!(!looked.output.contains("from-a"), "{}", looked.output);
    assert!(
        shared_run(&other, "cat /home/vibe/.pi/agent/models.json")
            .output
            .contains("providers"),
        "the owner's reach it"
    );

    // A login signed in to in one VM reaches the other through the store, seen
    // through virtio-fs after the rename, and a command never does.
    let login = r#"{"xai":{"type":"api_key","key":"from-a"}}"#;
    shared_run(
        &socket,
        &format!("printf '%s' '{login}' > /home/vibe/.pi/agent/auth.json"),
    );
    let mut seen = None;
    core.pi_watch(&mut seen).await;
    core.pi_watch(&mut seen).await;
    wait_for(
        "the other VM to be given the login",
        Duration::from_secs(10),
        || {
            // It may not be there yet.
            guest_ran(&other, &sh("cat /home/vibe/.pi/agent/auth.json"))
                .output
                .contains("from-a")
        },
        || {
            format!(
                "the VM it was signed in to:\n{}\nthe other:\n{}",
                guest_says(&socket),
                guest_says(&other)
            )
        },
    );
    shared_run(
        &socket,
        "printf '%s' '{\"openai\":{\"type\":\"api_key\",\"key\":\"!touch /tmp/owned\"}}' \
         > /home/vibe/.pi/agent/auth.json",
    );
    core.pi_watch(&mut seen).await;
    core.pi_watch(&mut seen).await;
    let status = core.pi_status(HostParams { host: None }).await.unwrap();
    let vms = status.vms.unwrap();
    // Two things for the owner to hear, and each said once: the first sign-in to
    // xai, a login every agent now uses, and the one that's refused.
    assert_eq!(vms.alerts.len(), 2, "{:?}", vms.alerts);
    let (signed_in, refused) = (&vms.alerts[1], &vms.alerts[0]);
    assert!(signed_in.contains("signed in to xai"), "{:?}", vms.alerts);
    assert!(
        refused.contains("login for openai would run a command"),
        "{:?}",
        vms.alerts
    );
    assert!(!vms.alerts.concat().contains("touch"), "{:?}", vms.alerts);
    assert!(!vms.logins.contains(&"openai".to_owned()));
    assert!(
        !shared_run(&other, "cat /home/vibe/.pi/agent/auth.json")
            .output
            .contains("touch")
    );

    // Delete both: each one's sessions go, the shared directory stays.
    for agent in [&r, &b.agent] {
        core.delete_agent(AgentDeleteParams {
            host: None,
            agent: agent.clone(),
            force: Some(true),
        })
        .await
        .unwrap_or_else(|e| panic!("delete: {e}"));
    }
    assert!(shared.join("models.json").is_file());
    mark("linux_pi_agent");
}
