//! The VibeOS core: the domain, the SQLite registry, host limits, API keys,
//! the JSON-RPC contract that every client of `vibed` speaks, and the
//! protocol of the per-agent `vibe-shim` sockets.

pub mod claude_token;
pub mod keys;
pub mod limits;
pub mod model;
pub mod paths;
pub mod rpc;
pub mod shim;
pub mod socket;
pub mod store;
pub mod time;
pub mod units;
