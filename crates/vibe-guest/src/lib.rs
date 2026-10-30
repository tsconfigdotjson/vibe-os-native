//! `vibe-guest`: PID 1 inside each `linux` VM (PRD: What's inside `linux`).
//! The binary stacks the agent's disks, mounts its shares, starts Claude as
//! the user `vibe` on a terminal, and freezes and thaws it around a
//! snapshot. This library is what the host side needs to talk to it, and
//! builds anywhere:
//!
//! - [`boot`]: the part of the kernel command line `vibed` writes for it,
//!   which says what to mount where.
//! - [`proto`]: the two vsock ports it serves, the control channel `vibed`
//!   drives it with and the terminal `vibe-shim` attaches to.
//!
//! The contract is in `protocol/vibe-guest/`, and `vibe_env::guest` is the
//! host's driver for it.

pub mod boot;
pub mod proto;
