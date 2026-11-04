//! Shared MCP services, on the Mac (PRD: Shared MCP services). A service is
//! connected once, in the app, and every agent VibeOS starts can use it: the
//! Mac is the proxy and the only place its tokens live.
//!
//! - [`registry`]: the curated set, Linear first.
//! - [`secrets`]: where a login is kept: the Keychain.
//! - [`oauth`]: discovery, registration and the token endpoint.
//! - [`broker`]: the lifecycle and the calls: refreshes one at a time,
//!   persisting a rotated token before it's used; a clear tool error when auth
//!   fails; an upstream session per agent; policy and audit.
//! - [`relay`]: how shims reach the broker: the relay frames from VMs and
//!   Linux hosts, and the local socket on the Mac.
//! - [`testing`]: stand-ins for a service and its OAuth server. Nothing in a
//!   test ever talks to a real one.

pub mod broker;
pub(crate) mod http;
pub mod oauth;
pub mod registry;
pub mod relay;
pub mod secrets;
pub mod testing;
mod util;

pub use broker::{Deps, Services};
pub use registry::{Service, supported};
pub use secrets::{Secret, SecretStore};
