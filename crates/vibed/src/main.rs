//! `vibed`: serves the JSON-RPC socket until SIGTERM or SIGINT.

use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use vibe_core::keychain;
use vibe_core::model::Environment;
use vibe_core::paths::{self, Layout};
use vibe_core::rpc::MAX_MESSAGE_BYTES;
use vibe_env::cgroups::Cgroups;
use vibe_env::linux::{self, LinuxVm};
use vibe_env::macos::Macos;
use vibe_env::testing::{TestBackend, TestToolbox};
use vibe_env::toolbox::Toolbox;
use vibe_services::secrets::{FileStore, Keychain};
use vibe_services::{Place, SecretStore, Service};
use vibed::agents::Backends;
use vibed::core::{Config, Core, DaemonInfo, StateLock};
use vibed::services::ServicesConfig;
use vibed::{logging, server};

const USAGE: &str = "\
usage: vibed [--socket PATH]

Serves the VibeOS JSON-RPC API on a Unix socket, and MCP on
http://127.0.0.1:7878/mcp (mcp_port and mcp_tailscale in host.toml), until
SIGTERM or SIGINT.
State lives under $HOME: on macOS in ~/Library/Application Support/VibeOS
(logs in ~/Library/Logs/VibeOS), elsewhere in ~/.local/share/vibeos. Host
limits are in ~/.config/vibeos/host.toml.

options:
  --socket PATH   the socket to serve (default: $VIBE_SOCKET, or run/vibed.sock
                  in the state directory)
  -V, --version   print the version
  -h, --help      print this help

environment:
  VIBE_GH         the gh to run (default: gh on PATH)
  VIBE_SHIM       the vibe-shim to start sessions with (default: the one
                  beside vibed, or on PATH)
  VIBE_VM         on a Mac, the vibe-vm that runs linux agents' VMs
                  (default: the one beside vibed)
  VIBE_BASE_RELEASES, VIBE_CLAUDE_RELEASES
                  where the Linux base image and Claude Code for linux agents
                  come from (default: their published releases; a file://
                  directory works too)
  VIBE_CLAUDE     the claude native agents run: macos agents on a Mac, linux
                  agents on a Linux host (default: the one on the login
                  shell's PATH on a Mac, on vibed's PATH or in ~/.local/bin
                  on a Linux host)
  VIBE_SSH        the ssh that reaches Linux hosts (default: ssh on PATH)
  VIBE_HOST_BUNDLE
                  the Linux builds of vibed, vibe and vibe-shim that Add Linux
                  host installs, as <dir>/<x86_64|aarch64>/<binary> (default:
                  linux/ beside vibed, or in the app's Resources)
  VIBE_CLI        the vibe agents run the shared services' shim with (default:
                  the one beside vibed, or on PATH)
  VIBE_TEST_SERVICES_DIR, VIBE_TEST_SERVICES_UPSTREAM
                  for tests only, in debug builds: keep shared services' logins
                  in this directory whatever the OS, and find Linear's stand-in
                  at this URL
  VIBE_TEST_HOST_BROKER
                  for tests only, in debug builds: 1 has this vibed's broker
                  behave as a Linux host's, whatever the OS
  VIBE_LOGIN_KEYCHAIN
                  never keeps this vibed out of the login Keychain; allow lets
                  one outside an app bundle in, which is otherwise kept out so
                  that no test reaches it; the agents it starts aren't given allow
  VIBE_TEST_MAC_PEER
                  for tests only, with VIBE_TEST_BACKEND: 1 lets a Mac's tunnel end
                  here whatever the OS, as a stand-in Linux host on a Mac
  VIBE_TEST_BACKEND
                  for tests only: run every environment's agents natively,
                  unlimited but for a supervisor, with this program as
                  Claude Code";

/// How often `gh auth status` runs (PRD: Auth, security and trust).
const GH_CHECK_EVERY: Duration = Duration::from_secs(60 * 60);

/// How often what VibeOS sets up itself is looked at again.
const REPAIR_EVERY: Duration = Duration::from_secs(60 * 60);

/// How often an open toolbox's shell is checked, to save it once it exits.
const TOOLBOX_POLL: Duration = Duration::from_secs(1);

/// How often projects whose agents are all at rest get `git maintenance`.
const MAINTENANCE_EVERY: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, PartialEq, Eq)]
enum Args {
    Run { socket: Option<PathBuf> },
    Version,
    Help,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut socket = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-V" | "--version" => return Ok(Args::Version),
            "-h" | "--help" => return Ok(Args::Help),
            "--socket" => {
                let path = iter.next().ok_or("--socket needs a path")?;
                socket = Some(PathBuf::from(path));
            }
            other => match other.strip_prefix("--socket=") {
                Some(path) => socket = Some(PathBuf::from(path)),
                None => return Err(format!("unexpected argument: {other}")),
            },
        }
    }
    Ok(Args::Run { socket })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let socket = match parse_args(&args) {
        Ok(Args::Run { socket }) => socket,
        Ok(Args::Version) => {
            println!("vibed {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Ok(Args::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            vibed::log!("vibed: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let layout = match Layout::current() {
        Ok(l) => l,
        Err(e) => {
            vibed::log!("vibed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let path = match paths::socket_path(socket) {
        Ok(p) => p,
        Err(e) => {
            vibed::log!("vibed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let logs = layout.logs_dir();
    let logged = DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&logs)
        .and_then(|()| logging::to_file(&logs.join("vibed.log")));
    if let Err(e) = logged {
        vibed::log!("vibed: warning: can't log to {}: {e}", logs.display());
    }
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            vibed::log!("vibed: can't start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let done = runtime.block_on(run(path, layout));
    // A download on a blocking thread isn't waited for: `run` has done all that
    // matters, and systemd or launchd is waiting for this to end.
    runtime.shutdown_timeout(Duration::from_secs(1));
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            vibed::log!("vibed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The descriptors `vibed` asks for: the status engine watches a
/// transcript or two per agent, and on macOS each watch holds one open,
/// where launchd starts jobs with a soft limit of 256.
const OPEN_FILES: u64 = 8192;

/// Raises the soft limit on open files towards [`OPEN_FILES`], as far as
/// the hard limit allows.
fn raise_open_files() {
    // SAFETY: getrlimit and setrlimit only read and write the struct.
    unsafe {
        let mut lim = std::mem::zeroed::<libc::rlimit>();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return;
        }
        let want = (OPEN_FILES as libc::rlim_t).min(lim.rlim_max);
        if lim.rlim_cur < want {
            lim.rlim_cur = want;
            if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) != 0 {
                vibed::log!(
                    "vibed: warning: can't raise the open-file limit: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
    }
}

async fn run(path: PathBuf, layout: Layout) -> Result<(), Box<dyn std::error::Error>> {
    raise_open_files();
    // Install the handlers before the socket exists, so a client that sees
    // the socket can always stop the daemon cleanly.
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    // Another vibed on this socket is the likelier story, and takes no waiting.
    // A lock that can't be looked at isn't one: `bind` says what's wrong.
    if let Ok(true) = vibe_core::socket::lock_state(&path) {
        return Err(server::BindError::AlreadyRunning { path }.into());
    }
    // The state first: a vibed refused for another's state must bind nothing, as
    // anything that connects takes a socket to mean a vibed that's running.
    let state_lock = StateLock::take(&layout)?;
    let (listener, guard) = server::bind(&path)?;
    let listener = UnixListener::from_std(listener)?;
    let mut config = Config::new(DaemonInfo::current(), layout);
    // For tests only, in debug builds: a host running behind this build.
    if cfg!(debug_assertions)
        && let Some(version) = env_text("VIBE_TEST_VERSION")
    {
        config.info.version = version;
    }
    config.state_lock = Some(state_lock);
    if let Some(gh) = std::env::var_os("VIBE_GH").filter(|v| !v.is_empty()) {
        config.gh = gh;
    }
    if let Some(shim) = std::env::var_os("VIBE_SHIM").filter(|v| !v.is_empty()) {
        config.shim = shim;
    }
    if let Some(ssh) = std::env::var_os("VIBE_SSH").filter(|v| !v.is_empty()) {
        config.remote = Arc::new(vibed::remote::Ssh {
            program: ssh,
            bundle: vibed::remote::default_bundle(),
            shared: Some(config.layout.run_dir()),
        });
    }
    if let Some(vibe) = std::env::var_os("VIBE_CLI").filter(|v| !v.is_empty()) {
        config.vibe_cli = vibe;
    }
    config.services = Some(services_config(&config.layout));
    config.serves_mac_peer = serves_mac_peer(
        &config.info.os,
        env_text("VIBE_TEST_BACKEND").is_some(),
        env_text("VIBE_TEST_MAC_PEER").as_deref(),
    );
    if let Some(claude) = std::env::var_os("VIBE_TEST_BACKEND").filter(|v| !v.is_empty()) {
        vibed::log!(
            "vibed: VIBE_TEST_BACKEND is set: agents run under the test backend, with {} as \
             Claude Code",
            PathBuf::from(&claude).display()
        );
        let mut backend = TestBackend::new(PathBuf::from(claude));
        if let Some(pi) = std::env::var_os("VIBE_TEST_PI").filter(|v| !v.is_empty()) {
            let dir = config.layout.data_dir().join("test-pi");
            backend = backend.with_pi(PathBuf::from(pi), dir);
        }
        config.backends = Backends::all(Arc::new(backend));
        // Tests' agents start whatever the machine running them is doing,
        // its memory and its disk, and never hold it awake.
        config.pressure = Arc::new(vibed::pressure::NoPressure);
        config.free_disk = Arc::new(|_: &std::path::Path| None);
        config.power = Arc::new(vibed::power::NoPower);
        // Its toolbox shell is a plain shell, and keeps empty versions.
        config.toolbox = Some(Arc::new(TestToolbox::new(
            config.layout.clone(),
            vec!["/bin/sh".into()],
        )));
    }
    // linux agents on a Mac run in VMs; a Linux host's run natively, each in
    // a cgroup of its own.
    let mut images = None;
    if config.backends.get(Environment::Linux).is_none() && cfg!(target_os = "macos") {
        match vibe_vm() {
            Some(vibe_vm) => {
                vibed::log!("vibed: linux agents run in VMs, with {}", vibe_vm.display());
                let machine = Arc::new(linux::Real {
                    vibe_vm: vibe_vm.clone(),
                });
                config.toolbox = Some(Arc::new(Toolbox::new(
                    config.layout.clone(),
                    machine,
                    linux::keychain_token(),
                )));
                let backend = LinuxVm::new(config.layout.clone(), vibe_vm);
                config.backends = config.backends.with(Environment::Linux, Arc::new(backend));
                let watch =
                    vibed::linux::Watch::new(&config.layout, vibed::linux::Sources::from_env());
                config.linux_images = Some(Arc::clone(&watch));
                images = Some(vibed::linux::spawn(watch, vibed::linux::CHECK_EVERY));
            }
            None => vibed::log!(
                "vibed: no vibe-vm beside vibed, nor in VIBE_VM, so linux agents can't start"
            ),
        }
    }
    let mut cgroups = None;
    if config.backends.get(Environment::Linux).is_none() && cfg!(target_os = "linux") {
        let mut backend = Cgroups::new(config.layout.home());
        if let Some(claude) = std::env::var_os("VIBE_CLAUDE").filter(|v| !v.is_empty()) {
            backend = backend.with_claude(PathBuf::from(claude));
        }
        if let Some(pi) = std::env::var_os("VIBE_PI").filter(|v| !v.is_empty()) {
            backend = backend.with_pi(PathBuf::from(pi));
        }
        match backend.check() {
            Ok((systemd, warnings)) => {
                vibed::log!(
                    "vibed: linux agents run natively, each in a cgroup of its own under systemd {}'s {}",
                    systemd.version,
                    backend.slice()
                );
                for w in warnings {
                    vibed::log!("vibed: warning: {w}");
                }
            }
            Err(e) => vibed::log!("vibed: linux agents can't start here: {e}"),
        }
        let backend = Arc::new(backend);
        cgroups = Some(Arc::clone(&backend));
        config.backends = config.backends.with(Environment::Linux, backend);
    }
    // macos agents run natively, under the governor.
    let mut macos = None;
    if config.backends.get(Environment::Macos).is_none() && cfg!(target_os = "macos") {
        let mut backend = Macos::new(config.layout.home());
        if let Some(claude) = std::env::var_os("VIBE_CLAUDE").filter(|v| !v.is_empty()) {
            backend = backend.with_claude(PathBuf::from(claude));
        }
        if let Some(pi) = std::env::var_os("VIBE_PI").filter(|v| !v.is_empty()) {
            backend = backend.with_pi(PathBuf::from(pi));
        }
        vibed::log!("vibed: macos agents run natively, under the governor");
        let backend = Arc::new(backend);
        macos = Some(Arc::clone(&backend));
        config.backends = config.backends.with(Environment::Macos, backend);
    }
    let core = Arc::new(Core::open(config)?);
    vibed::log!("vibed: state in {}", core.layout().data_dir().display());
    core.warn_about_limits();
    // In the background, so a shim that's slow to answer doesn't hold up
    // the socket.
    let adopting = {
        let core = Arc::clone(&core);
        tokio::spawn(async move {
            let adopted = core.adopt_sessions().await;
            vibed::log!(
                "vibed: {adopted} running session{} found",
                if adopted == 1 { "" } else { "s" }
            );
        })
    };
    let gh_checks = core.spawn_gh_checks(GH_CHECK_EVERY);
    let repairs = core.spawn_repairs(REPAIR_EVERY);
    // Every agent's transcript, read whole first: the states from before a
    // restart, rebuilt.
    let status = core.spawn_status();
    let maintenance = core.spawn_maintenance(MAINTENANCE_EVERY);
    // Pauses and warnings from agents' limits, as events and states.
    let limit_watch = core.spawn_limit_watch(vibed::limit_watch::LIMITS_EVERY);
    // The toolbox's Pi login moved to where agents find it; logins as they change.
    let pi_watch = core.spawn_pi_watch(vibed::pi::PI_WATCH_EVERY);
    // Saves the toolbox when its shell exits, and runs the weekly update.
    let toolbox = core.spawn_toolbox(TOOLBOX_POLL, vibed::toolbox::UPDATE_CHECK_EVERY);
    // MCP on loopback, and on the Tailscale address when host.toml says.
    let mcp = core.spawn_mcp(vibed::mcp::LISTEN_CHECK_EVERY);
    // Memory pressure: refuse starts, sleep idle agents early, then pause
    // or end agents.
    let pressure = core.spawn_pressure_watch(vibed::pressure::PRESSURE_EVERY);
    // Idle agents sleep; a due /loop wakes one; the Mac stays up meanwhile.
    let sleep = core.spawn_sleep_watch();
    // The links to Linux hosts, each kept up on its own.
    core.spawn_links();
    // The tunnels that let a Linux host's MCP reach this Mac; and on a Linux host, a look at
    // its end of one. Each call through one has its API key checked here.
    core.spawn_tunnels();
    if let Some(note) = core.retired_external_note() {
        vibed::log!("vibed: {note}");
    }
    // Shared MCP services: this host's broker, and on a Mac, the relays it
    // dials to its VMs.
    let services = core.spawn_services();
    vibed::log!(
        "vibed {} listening on {}",
        env!("CARGO_PKG_VERSION"),
        path.display()
    );
    let shutdown = async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    };
    server::serve(listener, Arc::clone(&core), MAX_MESSAGE_BYTES, shutdown).await;
    core.stop_links();
    core.stop_tunnels();
    for task in services {
        task.abort();
    }
    mcp.abort();
    gh_checks.abort();
    repairs.abort();
    status.abort();
    maintenance.abort();
    limit_watch.abort();
    pi_watch.abort();
    pressure.abort();
    sleep.abort();
    adopting.abort();
    if let Some(toolbox) = toolbox {
        toolbox.abort();
    }
    if let Some(images) = images {
        images.abort();
    }
    // Governors' threads end with the process, mid duty cycle or not:
    // their agents run on as themselves.
    if let Some(macos) = macos {
        macos.release();
    }
    // The kernel holds cgroups to their limits without vibed.
    if let Some(cgroups) = cgroups {
        cgroups.release();
    }
    drop(guard);
    vibed::log!("vibed: stopped");
    Ok(())
}

/// Whether a Mac's tunnel can end here: on a Linux host, never a Mac; except, in tests, a
/// stand-in Linux host on a Mac, which runs the test backend and asks with
/// `VIBE_TEST_MAC_PEER=1`.
fn serves_mac_peer(os: &str, test_backend: bool, stand_in: Option<&str>) -> bool {
    os == "linux" || (test_backend && stand_in == Some("1"))
}

/// Which services this host's broker offers, and where it keeps what it holds:
/// the Keychain on a Mac, and on a Linux host a private file in its state
/// directory. Only a debug build listens to the test variables, so a release
/// can't be talked into keeping a login elsewhere or sending one to a stand-in.
/// And a debug build kept out of the login Keychain (`vibe_core::keychain`) keeps
/// them as a Linux host does, in its own state directory, where a test can look.
/// A release kept out reports the Keychain's refusal: one can't be talked into
/// keeping a login in a file.
fn services_config(layout: &Layout) -> ServicesConfig {
    let stand_in_host =
        cfg!(debug_assertions) && env_text("VIBE_TEST_HOST_BROKER").as_deref() == Some("1");
    let place = if cfg!(target_os = "macos") && !stand_in_host {
        Place::Mac
    } else {
        Place::LinuxHost
    };
    let services = match env_text("VIBE_TEST_SERVICES_UPSTREAM").filter(|_| cfg!(debug_assertions))
    {
        Some(base) => {
            vibed::log!("vibed: VIBE_TEST_SERVICES_UPSTREAM is set: Linear is at {base}");
            vec![Service::linear_at(&base)]
        }
        None => vibe_services::supported(),
    };
    let test_dir = env_text("VIBE_TEST_SERVICES_DIR").filter(|_| cfg!(debug_assertions));
    let store: Arc<dyn SecretStore> = match test_dir {
        Some(dir) => {
            vibed::log!("vibed: VIBE_TEST_SERVICES_DIR is set: logins are kept in {dir}");
            Arc::new(FileStore(PathBuf::from(dir)))
        }
        None => {
            let dir = layout.data_dir().join("services");
            if place == Place::Mac {
                mac_store(dir, cfg!(debug_assertions), keychain::login_open())
            } else {
                Arc::new(FileStore(dir))
            }
        }
    };
    ServicesConfig {
        services,
        store,
        place,
        token_wait: vibe_services::broker::TOKEN_WAIT,
    }
}

/// A Mac's logins, for a `debug` build or a release, with the login Keychain
/// `open` to it or not. Logins a debug build once kept in `dir` go once the
/// Keychain's in use, as nothing would read or renew them and they're live.
fn mac_store(dir: PathBuf, debug: bool, open: bool) -> Arc<dyn SecretStore> {
    let file = FileStore(dir);
    if debug && !open {
        vibed::log!(
            "vibed: this debug build stays out of the login Keychain: logins are kept in \
             {} ({}=allow lets it in)",
            file.0.display(),
            keychain::GUARD_ENV
        );
        return Arc::new(file);
    }
    if open {
        match file.remove_all() {
            Ok(0) => {}
            Ok(n) => vibed::log!(
                "vibed: removed {n} login(s) a debug build left in {}: logins are in the \
                 Keychain now, so connect again if one's missing",
                file.0.display()
            ),
            Err(e) => vibed::log!(
                "vibed: can't remove the logins a debug build left in {}: {e}",
                file.0.display()
            ),
        }
    }
    Arc::new(Keychain::default())
}

/// An environment variable's value, if it's set to text.
fn env_text(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The `vibe-vm` to run VMs with: `VIBE_VM`, or the one beside `vibed`.
fn vibe_vm() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("VIBE_VM").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let beside = std::env::current_exe().ok()?.parent()?.join("vibe-vm");
    beside.is_file().then_some(beside)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn a_mac_keeps_logins_in_the_keychain_and_clears_up_after_a_debug_build() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("services");
        std::fs::create_dir(&dir).unwrap();
        let login = dir.join("linear.login");
        std::fs::write(&login, "live").unwrap();
        let kind = |store: Arc<dyn SecretStore>| {
            let kind = format!("{store:?}");
            kind[..kind.find(['(', ' ']).unwrap()].to_owned()
        };

        // A debug build kept out uses the file, and a release kept out only
        // reports the Keychain's refusal: neither takes the login away.
        assert_eq!(kind(mac_store(dir.clone(), true, false)), "FileStore");
        assert_eq!(kind(mac_store(dir.clone(), false, false)), "Keychain");
        assert!(login.exists());
        // With the Keychain in use, debug or release, it's gone.
        assert_eq!(kind(mac_store(dir.clone(), true, true)), "Keychain");
        assert!(!login.exists());
        std::fs::write(&login, "live").unwrap();
        assert_eq!(kind(mac_store(dir.clone(), false, true)), "Keychain");
        assert!(!login.exists());
    }

    #[test]
    fn a_macs_tunnel_only_ends_on_a_linux_host() {
        assert!(serves_mac_peer("linux", false, None));
        assert!(!serves_mac_peer("macos", false, None));
        // The variable alone does nothing: only a test's stand-in host,
        // on the test backend, may.
        assert!(!serves_mac_peer("macos", false, Some("1")));
        assert!(!serves_mac_peer("macos", true, None));
        assert!(!serves_mac_peer("macos", true, Some("yes")));
        assert!(serves_mac_peer("macos", true, Some("1")));
    }

    #[test]
    fn args() {
        assert_eq!(parse(&[]), Ok(Args::Run { socket: None }));
        assert_eq!(
            parse(&["--socket", "/s"]),
            Ok(Args::Run {
                socket: Some("/s".into())
            })
        );
        assert_eq!(
            parse(&["--socket=/s"]),
            Ok(Args::Run {
                socket: Some("/s".into())
            })
        );
        assert_eq!(parse(&["-V"]), Ok(Args::Version));
        assert_eq!(parse(&["--help"]), Ok(Args::Help));
        assert!(parse(&["--socket"]).is_err());
        assert!(parse(&["serve"]).is_err());
    }
}
