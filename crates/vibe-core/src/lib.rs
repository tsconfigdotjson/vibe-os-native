//! The VibeOS core: the domain, the SQLite registry, host limits, API keys,
//! and the JSON-RPC contract that every client of `vibed` speaks.

pub mod keys;
pub mod limits;
pub mod model;
pub mod paths;
pub mod rpc;
pub mod store;
pub mod time;
pub mod units;
