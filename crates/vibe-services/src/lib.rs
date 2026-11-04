//! Shared MCP services (PRD: Shared MCP services). Every `vibed` runs this
//! broker for its own agents, and holds its own login to each service, so a
//! Linux host's agents keep their services while the Mac is off. A token never
//! leaves the host that holds it. Services rotate refresh tokens, so one login
//! is never copied to several hosts: each signs in for itself. The app can
//! drive that from the Mac, as the host keeps the PKCE verifier and does the
//! exchange itself, and only a code that's useless without it crosses the link.
//!
//! - [`registry`]: the curated set, Linear first.
//! - [`secrets`]: where a login is kept: the Keychain on a Mac, a private file
//!   on a Linux host.
//! - [`oauth`]: discovery, registration and the token endpoint.
//! - [`broker`]: the lifecycle and the calls: refreshes one at a time,
//!   persisting a rotated token before it's used; a clear tool error when auth
//!   fails; an upstream session per agent; policy and audit.
//! - [`relay`]: how shims reach the broker: the local socket, and on a Mac the
//!   relay frames from a VM.
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

pub use broker::{Deps, Place, Services};
pub use registry::{Service, supported};
pub use secrets::{Secret, SecretStore};
