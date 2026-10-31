//! `vibed`: serves the JSON-RPC socket until SIGTERM or SIGINT.

use std::ffi::OsString;
use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use vibe_core::model::Environment;
use vibe_core::paths::{self, Layout};
use vibe_core::rpc::MAX_MESSAGE_BYTES;
use vibe_env::linux::{self, LinuxVm};
use vibe_env::macos::Macos;
use vibe_env::testing::{TestBackend, TestToolbox};
use vibe_env::toolbox::Toolbox;
use vibed::agents::Backends;
use vibed::core::{Config, Core, DaemonInfo};
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
  VIBE_CLAUDE     on a Mac, the claude macos agents run (default: the one on
                  the login shell's PATH)
  VIBE_SSH        the ssh that reaches Linux hosts (default: ssh on PATH)
  VIBE_HOST_BUNDLE
                  the Linux builds of vibed, vibe and vibe-shim that Add Linux
                  host installs, as <dir>/<x86_64|aarch64>/<binary> (default:
                  linux/ beside vibed, or in the app's Resources)
  VIBE_TEST_BACKEND
                  for tests only: run every environment's agents natively,
                  unlimited but for a supervisor, with this program as
                  Claude Code";

/// How often `gh auth status` runs (PRD: Auth, security and trust).
const GH_CHECK_EVERY: Duration = Duration::from_secs(60 * 60);

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
    match runtime.block_on(run(path, layout)) {
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
    let (listener, guard) = server::bind(&path)?;
    let listener = UnixListener::from_std(listener)?;
    let mut config = Config::new(DaemonInfo::current(), layout);
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
        });
    }
    if let Some(claude) = std::env::var_os("VIBE_TEST_BACKEND").filter(|v| !v.is_empty()) {
        vibed::log!(
            "vibed: VIBE_TEST_BACKEND is set: agents run under the test backend, with {} as \
             Claude Code",
            PathBuf::from(&claude).display()
        );
        config.backends = Backends::all(Arc::new(TestBackend::new(PathBuf::from(claude))));
        // Its toolbox shell is a plain shell, and keeps empty versions.
        config.toolbox = Some(Arc::new(TestToolbox::new(
            config.layout.clone(),
            vec!["/bin/sh".into()],
        )));
    }
    // linux agents on a Mac run in VMs; a Linux host's run under cgroups
    // (#17).
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
                images = Some(vibed::linux::spawn(
                    config.layout.clone(),
                    vibed::linux::Sources::from_env(),
                    vibed::linux::CHECK_EVERY,
                ));
            }
            None => vibed::log!(
                "vibed: no vibe-vm beside vibed, nor in VIBE_VM, so linux agents can't start"
            ),
        }
    }
    // macos agents run natively, under the governor.
    let mut macos = None;
    if config.backends.get(Environment::Macos).is_none() && cfg!(target_os = "macos") {
        let mut backend = Macos::new(config.layout.home());
        if let Some(claude) = std::env::var_os("VIBE_CLAUDE").filter(|v| !v.is_empty()) {
            backend = backend.with_claude(PathBuf::from(claude));
        }
        vibed::log!("vibed: macos agents run natively, under the governor");
        let backend = Arc::new(backend);
        macos = Some(Arc::clone(&backend));
        config.backends = config.backends.with(Environment::Macos, backend);
    }
    let gh: OsString = config.gh.clone();
    let core = Arc::new(Core::open(config)?);
    vibed::log!(
        "vibed: state in {}, gh is {}",
        core.layout().data_dir().display(),
        gh.to_string_lossy()
    );
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
    // Every agent's transcript, read whole first: the states from before a
    // restart, rebuilt.
    let status = core.spawn_status();
    let maintenance = core.spawn_maintenance(MAINTENANCE_EVERY);
    // Pauses and warnings from agents' limits, as events and states.
    let limit_watch = core.spawn_limit_watch(vibed::limit_watch::LIMITS_EVERY);
    // Saves the toolbox when its shell exits, and runs the weekly update.
    let toolbox = core.spawn_toolbox(TOOLBOX_POLL, vibed::toolbox::UPDATE_CHECK_EVERY);
    // MCP on loopback, and on the Tailscale address when host.toml says.
    let mcp = core.spawn_mcp(vibed::mcp::LISTEN_CHECK_EVERY);
    // The links to Linux hosts, each kept up on its own.
    core.spawn_links();
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
    mcp.abort();
    gh_checks.abort();
    status.abort();
    maintenance.abort();
    limit_watch.abort();
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
    drop(guard);
    vibed::log!("vibed: stopped");
    Ok(())
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
