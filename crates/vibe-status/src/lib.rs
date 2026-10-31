//! The status engine: wraps agent-tail's `SessionReducer` and feeds it each
//! agent's transcript (PRD: Architecture, Status engine; State and
//! lifecycle, Matching an agent to its transcript).
//!
//! A [`Tail`] follows one agent's transcript: the one its `--session-id`
//! names, or after a `/clear` the newest session recorded in its worktree.
//! It reads only what was appended since the last [`Tail::poll`], and says
//! what the agent is doing as of a time it's given, as a [`Status`]. A new
//! `Tail` reads the whole transcript, so a restarted `vibed` rebuilds the
//! same states. A [`Watcher`] wakes the poller when a transcript is
//! written; polling every second is the backstop.

pub mod messages;
mod tail;
mod watch;

pub use agent_tail::Timestamp;
pub use messages::{Message, Role, last_messages};
pub use tail::{INFERRED_QUIET, Mark, Status, Tail, Wakeup, state_of};
pub use watch::Watcher;

/// How often every transcript is polled even without a file-system event
/// (PRD: Matching an agent to its transcript, Polling).
pub const POLL_EVERY: std::time::Duration = std::time::Duration::from_secs(1);
