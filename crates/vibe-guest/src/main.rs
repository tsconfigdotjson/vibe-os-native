//! `vibe-guest`: PID 1 inside each `linux` VM (PRD: What's inside `linux`).
//! It stacks the agent's disks into its root, mounts the shares at their
//! host paths, brings up the network, and serves the control channel and
//! the agent's terminal over vsock: it starts Claude as `vibe` on a
//! terminal, reports its processes, and freezes and thaws it around a
//! snapshot. The protocol is the library half of this crate.
//!
//! It ships as a static `aarch64-unknown-linux-musl` binary, the initrd's
//! `/init`: `cargo build -p vibe-guest --target aarch64-unknown-linux-musl`.
//!
//! It's also the VM's `vibe` for shared MCP services: an agent runs it, copied
//! to `/run/vibe/bin/vibe`, as `vibe mcp-proxy <service>` (`vibe_relay::proxy`).
//!
//! `vibe-guest --vsock-dir DIR` runs it as an ordinary process for tests:
//! it serves Unix sockets `DIR/<port>` instead of vsock, runs commands as
//! whoever runs it, and mounts nothing.

#[cfg(target_os = "linux")]
mod init;
mod services_only;

use std::process::ExitCode;

use vibe_relay::{local, proxy};

const USAGE: &str = "\
usage: vibe-guest            as PID 1 of a linux VM
       vibe-guest --vsock-dir DIR [--cgroup DIR] [--share TAG:PATH]...
       vibe-guest --vsock-dir DIR --services-only
       vibe mcp-proxy SERVICE

The second form is for tests: it serves the guest's ports as Unix sockets
DIR/<port> instead of vsock, runs commands as the user running it, freezes
them with the cgroup DIR (one it may write to) if given, and waits for the
directories of --share after a thaw. It mounts nothing. Its shared services
socket is DIR/services.sock.

The third is for tests on any OS: only those shared services, serving
DIR/services.sock and the relay's DIR/<port>.

The fourth is an agent's shared MCP service over stdin and stdout, through this
VM's relay to the Mac (or vibe-guest mcp-proxy SERVICE, for tests; the socket is
$VIBE_SERVICES_SOCKET, or /run/vibe/services.sock).";

/// Runs the shim if that's what this was started as.
fn run_shim(args: &[String]) -> Option<ExitCode> {
    let invoked = std::env::args_os().next()?;
    let named_vibe = std::path::Path::new(&invoked)
        .file_name()
        .is_some_and(|n| n == "vibe");
    let proxy = args.first().is_some_and(|w| w == "mcp-proxy");
    if !named_vibe && !proxy {
        return None;
    }
    let Some(service) = args.get(1).filter(|_| proxy) else {
        eprintln!("vibe: in a VM, this vibe only does mcp-proxy SERVICE");
        return Some(ExitCode::from(2));
    };
    let socket = std::env::var_os(local::SOCKET_ENV)
        .filter(|v| !v.is_empty())
        .map_or_else(|| local::GUEST_SOCKET.into(), std::path::PathBuf::from);
    let config = proxy::Config {
        service: service.clone(),
        socket,
        agent: local::agent_from_env(),
        cache: local::cache_dir(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        timing: proxy::Timing::default(),
    };
    let ended = proxy::run(config, std::io::stdin().lock(), std::io::stdout());
    // Its threads go with the process, whatever they're waiting on.
    std::process::exit(i32::from(ended.is_err()));
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(ended) = run_shim(&args) {
        return ended;
    }
    if matches!(args.first().map(String::as_str), Some("--version" | "-V")) {
        println!("vibe-guest {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if matches!(args.first().map(String::as_str), Some("--help" | "-h")) {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    // Never as PID 1, which mustn't return: only ever a test's ordinary process.
    if std::process::id() != 1 && args.iter().any(|a| a == "--services-only") {
        return services_only::main(&args);
    }
    #[cfg(target_os = "linux")]
    {
        init::main(args)
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("vibe-guest runs as PID 1 of a Linux VM\n\n{USAGE}");
        ExitCode::from(2)
    }
}
