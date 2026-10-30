//! Environment backends: `Compartment`, `Limiter`, `Sleeper` and
//! `Capabilities`, plus the conformance suite every backend passes (PRD:
//! Architecture, Environment backends).
//!
//! The traits come with #4. What's here so far is the `linux` backend's
//! plumbing on the Mac: [`vm`] drives `vibe-vm`, the helper that runs each
//! agent's VM, and [`guest`] drives `vibe-guest`, PID 1 inside it. The
//! backends themselves follow in #4, #11 and #17.

pub mod guest;
pub mod vm;
