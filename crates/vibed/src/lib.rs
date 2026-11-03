//! `vibed`, the VibeOS daemon. This is its socket server and the core
//! handlers behind it; `main.rs` wires them to the process.

pub mod acme;
pub mod agents;
pub mod core;
pub mod dispatch;
pub mod doctor;
pub mod external;
pub mod gh;
pub mod hosts;
pub mod limit_watch;
pub mod link;
pub mod linux;
pub mod logging;
pub mod mcp;
pub mod power;
pub mod pressure;
pub mod projects;
pub mod reach;
pub mod remote;
pub mod server;
pub mod sessions;
pub mod sleep;
pub mod status;
pub mod talk;
pub mod toolbox;
pub mod tunnel;
