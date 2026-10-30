//! `vibe-guest`: PID 1 inside each `linux` VM. It starts Claude on a PTY,
//! reports the process tree, and freezes and thaws it around a snapshot
//! (PRD: Architecture). The real init lands in #8.
//!
//! It ships as a static `aarch64-unknown-linux-musl` binary:
//! `cargo build -p vibe-guest --target aarch64-unknown-linux-musl`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("--version" | "-V") => {
            println!("vibe-guest {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("vibe-guest: runs as PID 1 in a linux VM; not implemented yet (#8)");
            ExitCode::from(2)
        }
    }
}
