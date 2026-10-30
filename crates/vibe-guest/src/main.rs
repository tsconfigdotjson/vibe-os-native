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
//! `vibe-guest --vsock-dir DIR` runs it as an ordinary process for tests:
//! it serves Unix sockets `DIR/<port>` instead of vsock, runs commands as
//! whoever runs it, and mounts nothing.

#[cfg(target_os = "linux")]
mod init;

use std::process::ExitCode;

const USAGE: &str = "\
usage: vibe-guest            as PID 1 of a linux VM
       vibe-guest --vsock-dir DIR [--cgroup DIR] [--share TAG:PATH]...

The second form is for tests: it serves the guest's ports as Unix sockets
DIR/<port> instead of vsock, runs commands as the user running it, freezes
them with the cgroup DIR (one it may write to) if given, and waits for the
directories of --share after a thaw. It mounts nothing.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("--version" | "-V")) {
        println!("vibe-guest {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if matches!(args.first().map(String::as_str), Some("--help" | "-h")) {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
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
