//! Environment backends: `Compartment`, `Limiter`, `Sleeper` and
//! `Capabilities`, plus the conformance suite every backend passes (PRD:
//! Architecture, Environment backends).
//!
//! The traits come with #4. What's here so far is [`vm`], which drives
//! `vibe-vm`, the helper behind the `linux` backend on the Mac; the backends
//! themselves follow in #4, #11 and #17.

pub mod vm;
