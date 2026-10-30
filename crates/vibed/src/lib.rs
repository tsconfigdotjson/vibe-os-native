//! `vibed`, the VibeOS daemon. This is its socket server and the core
//! handlers behind it; `main.rs` wires them to the process.

pub mod dispatch;
pub mod server;

/// Writes a line to stderr. Unlike `eprintln!` it never panics, so a closed
/// stderr can't take the daemon down. Real logging comes with #2.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr().lock(), $($arg)*);
    }};
}
