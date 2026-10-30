//! `vibe-shim`: one per awake agent. It owns the agent's terminal end, a
//! scrollback ring buffer and a screen model, and serves `run/<ref>.sock`
//! (PRD: Architecture). The real shim lands in #3.

use std::process::ExitCode;

fn main() -> ExitCode {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("--version" | "-V") => {
            println!("vibe-shim {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("vibe-shim: started by vibed only; not implemented yet (#3)");
            ExitCode::from(2)
        }
    }
}
