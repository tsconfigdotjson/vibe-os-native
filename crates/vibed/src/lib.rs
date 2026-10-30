//! `vibed`, the VibeOS daemon. This is its socket server and the core
//! handlers behind it; `main.rs` wires them to the process.

pub mod core;
pub mod dispatch;
pub mod gh;
pub mod logging;
pub mod projects;
pub mod server;
