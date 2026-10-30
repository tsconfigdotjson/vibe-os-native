//! `vibe-testguest`: PID 1 of the small Linux guest that tests `vibe-vm` end
//! to end. `scripts/test-guest.sh` builds it into an initrd, and
//! `crates/vibe-env/tests/vm_cycle.rs` boots, saves and restores it. It isn't
//! shipped; the real guest is `vibe-guest` (#8).
//!
//! The initrd's `/init` loads the virtio drivers, mounts the `share`
//! virtio-fs tag at `/mnt/share` and execs this. It serves vsock port
//! [`PORT`]: each connection sends one command line and gets one JSON line
//! back.
//!
//! - `tick`: `{"count":N}`. N counts ticks since boot, in memory only, so a
//!   restored VM carries on from where it was saved and a fresh boot starts
//!   again at 1.
//! - `status`: the count, the kernel's boot id, eth0's MAC address and the
//!   IPv4 address DHCP gave it, the contents of `/mnt/share/probe` and of the
//!   disk whose serial is `agent` (up to the first newline), the uptime, the
//!   vCPUs, the hardware RNG in use, free and held memory, and the
//!   balloon's negotiated features.
//! - `sync`: flushes everything written to the disks.
//! - `dirty MIB`: fills MIB MiB of memory with data that doesn't compress
//!   and frees it again, like a guest that used memory and gave it back.
//!   Without the balloon, a snapshot would still hold those pages.
//! - `hold MIB`: the same, but kept in use until `release`: memory a
//!   snapshot must always hold.
//! - `write-disk TEXT`: writes TEXT and a newline at the start of the
//!   `agent` disk, without syncing, so a snapshot must carry it.
//!
//! `vibe-testguest ask` is something else: not PID 1 but the command
//! `vibe-guest` runs in the second test guest, standing in for Claude with
//! a question pending (see `ask.rs`).

/// The vsock port the test guest serves.
pub const PORT: u32 = 1024;

#[cfg(target_os = "linux")]
mod ask;
#[cfg(target_os = "linux")]
mod guest;

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "linux")]
    {
        if std::env::args().nth(1).as_deref() == Some("ask") {
            ask::run();
        }
        guest::run();
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("vibe-testguest runs as PID 1 of a Linux test VM");
        std::process::ExitCode::from(2)
    }
}
