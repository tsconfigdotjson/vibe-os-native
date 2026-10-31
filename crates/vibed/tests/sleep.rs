//! Sleep, wake and admission (#6), as one scripted scenario run on every
//! backend, with the stand-in for Claude Code:
//!
//! 1. idle → asleep once the idle debounce has passed, not before, with
//!    the Mac let go of; reads don't wake it;
//! 2. a send wakes it, and so does an attach;
//! 3. a start the host has no room for puts an idle agent to sleep first,
//!    on a backend where that frees memory; elsewhere it's refused, unless
//!    it's `max_awake_agents` that's full;
//! 4. a firm `/loop` wakeup fires on time from a sleeping agent.
//!
//! On the test backend, both as one whose sleep keeps memory and as one
//! whose sleep saves to disk, everywhere; on the cgroups backend on Linux
//! with a systemd user manager (`VIBE_TEST_CGROUPS` makes that a must); on
//! the `macos` backend on a Mac; and on real VMs on a Mac with
//! `VIBE_VM_BIN` and `VIBE_BASE_RELEASES` (`scripts/check-full.sh` sets
//! `VIBE_VM_MARKER` too, so a skip can't pass for a pass). It needs
//! `cargo build --workspace` first, for `vibe-shim`.
//!
//! The rest of the policy is here too, on the test backend: the snapshot
//! budget, low disk, and memory pressure on a supervised agent.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::Golden;
use vibe_core::model::{AgentState, Environment, EventKind};
use vibe_core::paths::Layout;
use vibe_core::rpc::{
    Agent, AgentDeleteParams, AgentStartParams, ErrorCode, HostParams, LimitsSetParams,
    ProfileUpdateParams, ProjectAddParams, SessionParams,
};
use vibe_core::shim::{self, control_hello};
use vibe_core::units::{GIB, MIB};
use vibe_env::testing::{TEST_SNAPSHOT, TestBackend};
use vibe_env::{Backend, Handle};
use vibe_mcp::Input;
use vibed::agents::{Backends, HostAdmission};
use vibed::core::{Config, Core};
use vibed::power::RecordedPower;
use vibed::pressure::{
    END_AFTER, END_EVERY, KILL_AFTER, Level, MemoryPressure, PressureWatch, Reading,
};
use vibed::sleep::Timing;

/// How a scenario is timed for a backend: a restore from disk takes
/// seconds, so a VM's `/loop` gets the real lead.
#[derive(Debug, Clone, Copy)]
struct Times {
    debounce: u64,
    wake_before: Duration,
    loop_in: u64,
    /// Slack for a sleep or a wake.
    slack: u64,
}

const QUICK: Times = Times {
    debounce: 3,
    wake_before: Duration::from_secs(4),
    loop_in: 12,
    slack: 3,
};

const SLOW: Times = Times {
    debounce: 3,
    wake_before: vibed::sleep::WAKE_BEFORE_DUE,
    loop_in: 50,
    slack: 8,
};

struct World {
    /// Its home, kept until the world ends.
    _golden: Golden,
    core: Arc<Core>,
    backend: Arc<dyn Backend>,
    layout: Layout,
    power: Arc<RecordedPower>,
    environment: Environment,
    times: Times,
    /// Every agent.state event: when, which agent, what state.
    states: Arc<Mutex<Vec<(SystemTime, String, String)>>>,
    _watch: tokio::task::JoinHandle<()>,
    _events: tokio::task::JoinHandle<()>,
}

fn secs(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH).unwrap().as_secs_f64()
}

/// Seconds since the epoch of an RFC 3339 time as `vibed` writes them,
/// `2026-10-30T12:00:00Z`.
fn parse_time(s: &str) -> f64 {
    let date: Vec<i64> = s[..10].split('-').map(|p| p.parse().unwrap()).collect();
    let time: Vec<i64> = s[11..19].split(':').map(|p| p.parse().unwrap()).collect();
    let (y, m, d) = (date[0], date[1], date[2]);
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    (days * 86_400 + time[0] * 3600 + time[1] * 60 + time[2]) as f64
}

/// The golden world, on the backend `make` makes for its home, for
/// `environment`, with the real clock, a host with room for two 1G agents,
/// and the sleep watch looking every 200 ms.
async fn world(
    make: impl FnOnce(&Golden) -> Arc<dyn Backend>,
    environment: Environment,
    times: Times,
    change: impl FnOnce(&mut Config),
) -> World {
    let golden = Golden::new();
    let backend = make(&golden);
    let power = Arc::new(RecordedPower::default());
    let mut config = golden.config();
    config.clock = Arc::new(vibe_core::time::now);
    config.backends = Backends::new().with(environment, Arc::clone(&backend));
    config.admission = Arc::new(HostAdmission {
        total_memory: 2 * GIB,
    });
    config.power = power.clone();
    config.sleep_timing = Timing {
        every: Duration::from_millis(200),
        wake_before: times.wake_before,
    };
    change(&mut config);
    let core = Arc::new(Core::open(config).unwrap());
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
    core.set_limits(LimitsSetParams {
        reserve_memory: Some(0),
        idle_debounce: Some(times.debounce),
        ..Default::default()
    })
    .unwrap();
    let states = Arc::new(Mutex::new(Vec::new()));
    let (_, mut rx) = core.subscribe();
    let events = {
        let states = Arc::clone(&states);
        tokio::spawn(async move {
            while let Ok(e) = rx.recv().await {
                if e.kind == EventKind::AgentState
                    && let (Some(a), Some(s)) = (e.agent, e.state)
                {
                    states.lock().unwrap().push((SystemTime::now(), a, s));
                }
            }
        })
    };
    let watch = core.spawn_sleep_watch();
    World {
        layout: Layout::new(&golden.home, "macos"),
        _golden: golden,
        core,
        backend,
        power,
        environment,
        times,
        states,
        _watch: watch,
        _events: events,
    }
}

impl World {
    fn start(&self, prompt: &str) -> AgentStartParams {
        AgentStartParams {
            project: "app".into(),
            profile: match self.environment {
                Environment::Linux => "software-eng-linux",
                Environment::Macos => "software-eng-mac",
            }
            .into(),
            prompt: prompt.into(),
            ..Default::default()
        }
    }

    async fn agent(&self, r: &str) -> Agent {
        self.core
            .agents(HostParams::default())
            .await
            .unwrap()
            .agents
            .into_iter()
            .find(|a| a.agent == r)
            .unwrap_or_else(|| panic!("no agent {r}"))
    }

    async fn state(&self, r: &str) -> AgentState {
        self.agent(r).await.state
    }

    /// Waits up to `within` for `r` to be in `state`.
    async fn wait_state(&self, r: &str, state: AgentState, within: Duration) {
        let deadline = Instant::now() + within;
        loop {
            let now = self.state(r).await;
            if now == state {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{r} is {now}, never {state}; its screen:\n{}",
                self.screen(r)
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// When `r` last went into `state`, by its events.
    fn went(&self, r: &str, state: &str) -> Option<SystemTime> {
        self.states
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(_, a, s)| a == r && s == state)
            .map(|(t, _, _)| *t)
    }

    fn screen(&self, r: &str) -> String {
        shim::connect(
            &self.layout.session_socket(r),
            &control_hello("sleep test"),
            Duration::from_secs(5),
        )
        .and_then(|mut c| Ok(c.read_output(None)?.screen))
        .unwrap_or_else(|e| format!("(no screen: {e})"))
    }

    async fn wait_screen(&self, r: &str, text: &str, within: Duration) {
        let deadline = Instant::now() + within;
        loop {
            let screen = self.screen(r);
            if screen.contains(text) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{text:?} never on {r}'s screen:\n{screen}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn handle(&self, r: &str) -> Handle {
        let record = vibe_core::store::Store::open(&self.layout.db())
            .unwrap()
            .agent("local", r)
            .unwrap();
        serde_json::from_str(&record.compartment).unwrap()
    }

    fn set(&self, change: LimitsSetParams) {
        self.core.set_limits(change).unwrap();
    }

    fn profile_memory(&self, memory: u64) {
        for profile in ["software-eng-linux", "software-eng-mac"] {
            self.core
                .update_profile(ProfileUpdateParams {
                    project: "app".into(),
                    profile: profile.into(),
                    memory: Some(memory),
                    ..Default::default()
                })
                .unwrap();
        }
    }

    /// Waits up to 5 s for the Mac to be held awake, or let go.
    async fn wait_power(&self, held: bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.power.held() != held {
            assert!(
                Instant::now() < deadline,
                "the Mac is {}held awake",
                if held { "not " } else { "still " }
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
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

fn text(t: &str) -> Input {
    Input::Text {
        text: t.into(),
        submit: true,
        force: false,
    }
}

/// The number after `prefix` on `screen`.
fn number_after(screen: &str, prefix: &str) -> f64 {
    let rest = &screen[screen
        .find(prefix)
        .unwrap_or_else(|| panic!("no {prefix:?} in {screen}"))
        + prefix.len()..];
    rest.split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

/// The Done-when of #6, on the world's backend.
async fn scenario(w: &World) {
    let t = w.times;
    let caps = w.backend.capabilities();
    let wait = |s: u64| Duration::from_secs(s);

    // 1. At rest with nothing attached, it sleeps once the debounce has
    // passed, and not before.
    let a = w.core.start_agent(w.start("talk")).await.unwrap();
    let r = a.agent.clone();
    w.wait_screen(&r, "Ready.", wait(60)).await;
    w.wait_state(&r, AgentState::Idle, wait(30)).await;
    w.wait_power(true).await;
    let rested = parse_time(
        &w.agent(&r)
            .await
            .status
            .since
            .expect("when it came to rest"),
    );
    w.wait_state(&r, AgentState::Asleep, wait(t.debounce + t.slack + 10))
        .await;
    let slept = secs(w.went(&r, "asleep").unwrap());
    assert!(
        slept - rested >= t.debounce as f64 - 1.0
            && slept - rested <= (t.debounce + t.slack) as f64 + 1.0,
        "asleep {:.1}s after it came to rest, with a debounce of {}s",
        slept - rested,
        t.debounce
    );
    // Nothing of it runs; saved to disk, its snapshot is there.
    let h = w.handle(&r);
    let procs = w.backend.processes(&h).unwrap();
    assert!(procs.iter().all(|p| p.stopped), "{procs:?}");
    if caps.sleep_frees_memory {
        assert!(w.backend.saved(&h).is_some(), "no snapshot");
    }
    // No agent awake: the Mac may sleep.
    w.wait_power(false).await;
    // Reads never wake it.
    w.core.agent_output(None, &r, None).await.unwrap();
    w.core.agent_messages(None, &r, 5).await.unwrap();
    w.core.check_agent(None, &r, false).await.unwrap();
    assert_eq!(w.state(&r).await, AgentState::Asleep);

    // 2. A send wakes it, where it was.
    let sent = w.core.send_input(None, &r, text("hello")).await.unwrap();
    assert!(sent.woke, "{sent:?}");
    w.wait_screen(&r, "> hello", wait(30)).await;
    assert_ne!(w.state(&r).await, AgentState::Asleep);
    assert!(
        !w.screen(&r).contains("Resumed"),
        "woken exactly, not resumed"
    );
    if caps.sleep_frees_memory {
        assert!(
            w.backend.saved(&w.handle(&r)).is_none(),
            "its snapshot is used up"
        );
    }
    w.wait_power(true).await;
    // Idle again, it sleeps again, and an attach wakes it too.
    w.wait_state(
        &r,
        AgentState::Asleep,
        wait(t.debounce + t.slack + 10) + vibed::sleep::AFTER_WAKE,
    )
    .await;
    let attach = w
        .core
        .attach_session(SessionParams {
            host: None,
            agent: r.clone(),
        })
        .await
        .unwrap();
    assert!(Path::new(&attach.socket).exists());
    assert_ne!(w.state(&r).await, AgentState::Asleep);
    w.wait_screen(&r, "> hello", wait(10)).await;

    // 3. A start the host has no room for: 1.5G more doesn't fit beside
    // its 1G of 2G. It's still inside its debounce now.
    w.set(LimitsSetParams {
        idle_debounce: Some(600),
        ..Default::default()
    });
    tokio::time::sleep(vibed::sleep::AFTER_WAKE + Duration::from_secs(1)).await;
    w.wait_state(&r, AgentState::Idle, wait(10)).await;
    w.profile_memory(1536 * MIB);
    let started = w.core.start_agent(w.start("talk")).await;
    if caps.sleep_frees_memory {
        // Its sleep gives its memory back, so it sleeps early, first.
        let b = started.unwrap_or_else(|e| panic!("{}", e.message));
        assert_eq!(w.state(&r).await, AgentState::Asleep);
        assert!(w.backend.saved(&w.handle(&r)).is_some());
        w.wait_screen(&b.agent, "Ready.", wait(60)).await;
        w.delete(&b.agent).await;
    } else {
        // Its sleep would free nothing, so it's left awake, and the start
        // refused with the reason.
        let err = started.unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict.code());
        assert!(
            err.message.contains("no room for an agent with 1.5G")
                && err
                    .message
                    .contains("No idle agent would free memory by going to sleep"),
            "{}",
            err.message
        );
        assert_ne!(w.state(&r).await, AgentState::Asleep);
        // When it's max_awake_agents that's full, any idle agent's sleep
        // makes room.
        w.profile_memory(GIB);
        w.set(LimitsSetParams {
            max_awake_agents: Some(1),
            ..Default::default()
        });
        let b = w.core.start_agent(w.start("talk")).await.unwrap();
        assert_eq!(w.state(&r).await, AgentState::Asleep);
        w.wait_screen(&b.agent, "Ready.", wait(60)).await;
        w.delete(&b.agent).await;
        w.set(LimitsSetParams {
            max_awake_agents: Some(16),
            ..Default::default()
        });
    }
    w.profile_memory(GIB);

    // 4. A firm /loop wakeup fires on time from a sleeping agent: it
    // sleeps, is woken ahead of it, and the loop fires when it's due.
    w.set(LimitsSetParams {
        idle_debounce: Some(t.debounce),
        ..Default::default()
    });
    let c = w
        .core
        .start_agent(w.start(&format!("loop {}", t.loop_in)))
        .await
        .unwrap();
    let rc = c.agent.clone();
    w.wait_screen(&rc, "Loop due at ", wait(60)).await;
    let due = number_after(&w.screen(&rc), "Loop due at ");
    let lead = t.wake_before.as_secs_f64();
    w.wait_state(&rc, AgentState::Asleep, wait(t.debounce + t.slack + 10))
        .await;
    let slept = secs(w.went(&rc, "asleep").unwrap());
    assert!(slept < due - lead, "asleep at {slept}, due at {due}");
    w.wait_screen(&rc, "Loop fired at ", wait(t.loop_in + t.slack + 10))
        .await;
    let fired = number_after(&w.screen(&rc), "Loop fired at ");
    let woke = w
        .states
        .lock()
        .unwrap()
        .iter()
        .filter(|(at, a, s)| *a == rc && s != "asleep" && secs(*at) > slept)
        .map(|(at, _, _)| secs(*at))
        .reduce(f64::min)
        .expect("a wake");
    assert!(
        woke >= due - lead - 1.0 && woke < due,
        "woken at {woke}, for a loop due at {due} with a lead of {lead}s"
    );
    assert!(
        fired >= due && fired <= due + 3.0,
        "fired at {fired}, due at {due}"
    );
    assert!(!w.screen(&rc).contains("Resumed"), "woken exactly");

    w.delete(&r).await;
    w.delete(&rc).await;
}

fn stand_in() -> (tempfile::TempDir, PathBuf) {
    let bin = common::short_tempdir();
    let claude = common::stand_in(bin.path());
    (bin, claude)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_the_test_backend_whose_sleep_keeps_memory() {
    let (_bin, claude) = stand_in();
    let make = |_: &Golden| -> Arc<dyn Backend> { Arc::new(TestBackend::new(&claude)) };
    let w = world(make, Environment::Linux, QUICK, |_| {}).await;
    scenario(&w).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_the_test_backend_whose_sleep_saves_to_disk() {
    let (_bin, claude) = stand_in();
    let make = |_: &Golden| -> Arc<dyn Backend> {
        Arc::new(TestBackend::new(&claude).sleeping_to_disk(64 * MIB))
    };
    let w = world(make, Environment::Linux, QUICK, |_| {}).await;
    scenario(&w).await;
}

/// Stops a test's slice, and whatever a failed test left in it.
struct StopSlice(String);

impl Drop for StopSlice {
    fn drop(&mut self) {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "stop", &self.0])
            .status();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_a_linux_hosts_cgroups() {
    let must = std::env::var_os("VIBE_TEST_CGROUPS").is_some_and(|v| !v.is_empty());
    let slice = format!("vibesleep{}.slice", std::process::id());
    let _stop = StopSlice(slice.clone());
    let probe = vibe_env::cgroups::Cgroups::new("/").with_slice(slice.clone());
    if !cfg!(target_os = "linux") || probe.check().is_err() {
        assert!(!must, "VIBE_TEST_CGROUPS is set, but: {:?}", probe.check());
        eprintln!("on_a_linux_hosts_cgroups: skipped; no cgroups here");
        return;
    }
    let (_bin, claude) = stand_in();
    let make = |g: &Golden| -> Arc<dyn Backend> {
        Arc::new(
            vibe_env::cgroups::Cgroups::new(&g.home)
                .with_claude(&claude)
                .with_path(std::env::var("PATH").unwrap())
                .with_slice(slice.clone()),
        )
    };
    let w = world(make, Environment::Linux, QUICK, |_| {}).await;
    scenario(&w).await;
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_the_macos_backend() {
    let (_bin, claude) = stand_in();
    let make = |g: &Golden| -> Arc<dyn Backend> {
        // The owner's Claude state, as a Mac has it before any agent.
        fs::create_dir_all(g.home.join(".claude/projects")).unwrap();
        fs::write(g.home.join(".claude.json"), "{}").unwrap();
        Arc::new(
            vibe_env::macos::Macos::new(&g.home)
                .with_claude(&claude)
                .with_path(std::env::var("PATH").unwrap()),
        )
    };
    let w = world(make, Environment::Macos, QUICK, |_| {}).await;
    scenario(&w).await;
}

fn absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(path)
    }
}

/// On real VMs: the base image `scripts/base-image.sh` built, with the
/// stand-in run as Claude from the share the project's `.git` is on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_real_vms() {
    let (Some(vibe_vm), Some(base)) = (
        std::env::var_os("VIBE_VM_BIN"),
        std::env::var("VIBE_BASE_RELEASES")
            .ok()
            .filter(|v| !v.is_empty()),
    ) else {
        eprintln!(
            "on_real_vms: skipped; set VIBE_VM_BIN and VIBE_BASE_RELEASES to run it on a Mac"
        );
        return;
    };
    let vibe_vm = absolute(PathBuf::from(vibe_vm));
    let make = |g: &Golden| -> Arc<dyn Backend> {
        let layout = Layout::new(&g.home, "macos");
        let images = vibe_env::images::Images::new(layout.linux_dir());
        images
            .install_base(
                &vibe_env::images::base_url(&base, vibe_env::images::BASE_RELEASE),
                vibe_env::images::BASE_RELEASE,
            )
            .unwrap();
        // The stand-in, where every VM sees it at the same path.
        let tools = g.home.join("src/app/.git/vibe-tools");
        fs::create_dir_all(&tools).unwrap();
        common::write_script(&tools.join("claude"), vibe_env::testing::STAND_IN);
        images
            .add_managed("0.0.1", &tools.join("claude"), "stand-in")
            .unwrap();
        // Small VMs, quick to save.
        fs::create_dir_all(layout.config_dir()).unwrap();
        fs::write(layout.host_config(), "vm_disk = \"2G\"\ncpu_cores = 2\n").unwrap();
        let token: vibe_env::linux::TokenSource =
            Arc::new(|| Ok("sk-ant-oat01-unused-by-the-stand-in".into()));
        Arc::new(
            vibe_env::linux::LinuxVm::with_machine(
                layout,
                Arc::new(vibe_env::linux::Real {
                    vibe_vm: vibe_vm.clone(),
                }),
                token,
            )
            .with_claude(tools.join("claude").to_string_lossy().into_owned()),
        )
    };
    let w = world(make, Environment::Linux, SLOW, |c| {
        // Its snapshots on this Mac's real disk.
        c.free_disk = Arc::new(vibed::sleep::free_space);
    })
    .await;
    scenario(&w).await;
    if let Some(dir) = std::env::var_os("VIBE_VM_MARKER") {
        let dir = absolute(PathBuf::from(dir));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("sleep_scenario"), "ran\n").unwrap();
    }
}

/// Memory pressure as a test sets it.
#[derive(Debug, Default)]
struct Pressure(Mutex<Option<Reading>>);

impl Pressure {
    fn set(&self, r: Option<Reading>) {
        *self.0.lock().unwrap() = r;
    }
}

impl MemoryPressure for Pressure {
    fn read(&self) -> Option<Reading> {
        *self.0.lock().unwrap()
    }
}

/// Three agents at rest, on a backend whose sleep saves to disk.
async fn three_on_disk(claude: &Path, change: impl FnOnce(&mut Config)) -> (World, Vec<Agent>) {
    let make = |_: &Golden| -> Arc<dyn Backend> {
        Arc::new(TestBackend::new(claude).sleeping_to_disk(64 * MIB))
    };
    let w = world(make, Environment::Linux, QUICK, |c| {
        c.admission = Arc::new(HostAdmission {
            total_memory: 8 * GIB,
        });
        change(c);
    })
    .await;
    w.set(LimitsSetParams {
        idle_debounce: Some(600),
        ..Default::default()
    });
    let mut agents = Vec::new();
    for _ in 0..3 {
        let a = w.core.start_agent(w.start("talk")).await.unwrap();
        w.wait_screen(&a.agent, "Ready.", Duration::from_secs(20))
            .await;
        agents.push(a);
    }
    (w, agents)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshots_share_a_budget_and_the_oldest_goes_first() {
    let (_bin, claude) = stand_in();
    let (w, agents) = three_on_disk(&claude, |_| {}).await;
    let [a, b, c] = [&agents[0], &agents[1], &agents[2]];
    // Room for two 64M snapshots, not three.
    w.set(LimitsSetParams {
        snapshot_disk: Some(150 * MIB),
        ..Default::default()
    });
    for x in [a, b] {
        w.core.sleep_agent(None, &x.agent).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let snapshot = |x: &Agent| w.layout.agent_dir(&x.agent).join(TEST_SNAPSHOT);
    assert!(snapshot(a).is_file() && snapshot(b).is_file());
    // The third's needs room: the oldest is dropped, and goes with it.
    w.core.sleep_agent(None, &c.agent).await.unwrap();
    assert!(!snapshot(a).exists(), "the oldest snapshot is dropped");
    assert!(snapshot(b).is_file() && snapshot(c).is_file());
    assert_eq!(w.state(&a.agent).await, AgentState::Asleep);
    // Its snapshot gone, a send wakes it with claude --resume.
    let sent = w.core.send_input(None, &a.agent, text("hi")).await.unwrap();
    assert!(sent.woke);
    w.wait_screen(
        &a.agent,
        &format!("Resumed {}.", a.session_id),
        Duration::from_secs(20),
    )
    .await;
    // A snapshot bigger than the whole budget can't be made: it stays
    // awake, and says why.
    w.set(LimitsSetParams {
        snapshot_disk: Some(32 * MIB),
        ..Default::default()
    });
    let err = w.core.sleep_agent(None, &a.agent).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict.code());
    assert!(
        err.message.contains("stays awake") && err.message.contains("snapshot_disk is 32M"),
        "{}",
        err.message
    );
    assert_ne!(w.state(&a.agent).await, AgentState::Asleep);
    for x in &agents {
        w.delete(&x.agent).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn low_disk_refuses_starts_and_keeps_an_agent_awake() {
    let (_bin, claude) = stand_in();
    let free = Arc::new(std::sync::atomic::AtomicU64::new(100 * GIB));
    let (w, agents) = {
        let free = Arc::clone(&free);
        three_on_disk(&claude, move |c| {
            c.free_disk =
                Arc::new(move |_: &Path| Some(free.load(std::sync::atomic::Ordering::SeqCst)));
        })
        .await
    };
    let set_free = |n: u64| free.store(n, std::sync::atomic::Ordering::SeqCst);
    // Under 10G free, nothing new starts.
    set_free(5 * GIB);
    let err = w.core.start_agent(w.start("talk")).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict.code());
    assert!(
        err.message.contains("only 5G free") && err.message.contains("need 10G free"),
        "{}",
        err.message
    );
    // A snapshot the disk can't take, with room to spare: it stays awake,
    // whether asked or idle.
    set_free(512 * MIB);
    let a = &agents[0];
    let err = w.core.sleep_agent(None, &a.agent).await.unwrap_err();
    assert!(
        err.message.contains("stays awake") && err.message.contains("the disk has only 512M free"),
        "{}",
        err.message
    );
    w.set(LimitsSetParams {
        idle_debounce: Some(1),
        ..Default::default()
    });
    tokio::time::sleep(Duration::from_secs(4)).await;
    for x in &agents {
        assert_ne!(w.state(&x.agent).await, AgentState::Asleep, "{}", x.agent);
        assert!(!w.layout.agent_dir(&x.agent).join(TEST_SNAPSHOT).exists());
    }
    for x in &agents {
        w.delete(&x.agent).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn critical_pressure_sleeps_idle_agents_then_pauses_and_ends_the_hog() {
    let (_bin, claude) = stand_in();
    let pressure = Arc::new(Pressure::default());
    // Supervised, as macos agents are; each reported at 900M of its 1G.
    let make = |_: &Golden| -> Arc<dyn Backend> {
        Arc::new(TestBackend::new(&claude).reporting_memory(900 * MIB))
    };
    let w = {
        let pressure = Arc::clone(&pressure);
        world(make, Environment::Macos, QUICK, move |c| {
            c.pressure = pressure
        })
        .await
    };
    w.set(LimitsSetParams {
        idle_debounce: Some(600),
        ..Default::default()
    });
    let idle = w.core.start_agent(w.start("talk")).await.unwrap();
    let hog = w.core.start_agent(w.start("hello")).await.unwrap();
    w.wait_state(&idle.agent, AgentState::Idle, Duration::from_secs(20))
        .await;
    w.wait_screen(&hog.agent, "Working on: hello", Duration::from_secs(20))
        .await;
    let (_, mut events) = w.core.subscribe();

    pressure.set(Some(Level::Critical.into()));
    // Nothing new starts, and idle agents sleep early, at once.
    let err = w.core.start_agent(w.start("more")).await.unwrap_err();
    assert!(
        err.message.contains("short of memory right now")
            && err
                .message
                .contains("macOS says memory pressure is critical"),
        "{}",
        err.message
    );
    let mut watch = PressureWatch::default();
    let t0 = Instant::now();
    assert_eq!(w.core.check_pressure(&mut watch, t0).await, None);
    assert_eq!(w.state(&idle.agent).await, AgentState::Asleep);
    assert_ne!(w.state(&hog.agent).await, AgentState::Paused);
    // Once it lasts, the agent furthest over 80% of its memory is paused,
    // and says so.
    assert_eq!(
        w.core.check_pressure(&mut watch, t0 + END_AFTER).await,
        None
    );
    assert_eq!(w.state(&hog.agent).await, AgentState::Paused);
    let h = w.handle(&hog.agent);
    assert!(w.backend.processes(&h).unwrap().iter().all(|p| p.stopped));
    let limit = loop {
        let e = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("an agent.limit event")
            .unwrap();
        if e.kind == EventKind::AgentLimit {
            break e;
        }
    };
    assert_eq!(limit.agent.as_deref(), Some(hog.agent.as_str()));
    let message = limit.message.unwrap();
    assert!(
        message.contains("at 88% of its 1G") && message.contains("ended in 5 min"),
        "{message}"
    );
    // The limit watch leaves it paused; nothing more happens until it has
    // been paused a while.
    w.core.check_limits(&mut Default::default()).await;
    assert_eq!(w.state(&hog.agent).await, AgentState::Paused);
    assert_eq!(
        w.core
            .check_pressure(&mut watch, t0 + END_AFTER + END_EVERY)
            .await,
        None
    );
    // Paused 5 min with the pressure on, it's ended, resumable.
    assert_eq!(
        w.core
            .check_pressure(&mut watch, t0 + END_AFTER + KILL_AFTER)
            .await,
        Some(hog.agent.clone())
    );
    assert_eq!(w.state(&hog.agent).await, AgentState::Asleep);
    assert!(
        w.backend
            .processes(&w.handle(&hog.agent))
            .unwrap()
            .is_empty()
    );
    pressure.set(None);
    let sent = w
        .core
        .send_input(None, &hog.agent, text("back"))
        .await
        .unwrap();
    assert!(sent.woke);
    w.wait_screen(
        &hog.agent,
        &format!("Resumed {}.", hog.session_id),
        Duration::from_secs(20),
    )
    .await;

    // Paused again, it runs again once the pressure eases.
    pressure.set(Some(Level::Critical.into()));
    let mut watch = PressureWatch::default();
    let t1 = Instant::now();
    w.core.check_pressure(&mut watch, t1).await;
    w.core.check_pressure(&mut watch, t1 + END_AFTER).await;
    assert_eq!(w.state(&hog.agent).await, AgentState::Paused);
    pressure.set(None);
    assert_eq!(
        w.core.check_pressure(&mut watch, t1 + END_AFTER * 2).await,
        None
    );
    assert_ne!(w.state(&hog.agent).await, AgentState::Paused);
    let procs = w.backend.processes(&w.handle(&hog.agent)).unwrap();
    assert!(procs.iter().all(|p| !p.stopped), "{procs:?}");
    w.delete(&idle.agent).await;
    w.delete(&hog.agent).await;
}
