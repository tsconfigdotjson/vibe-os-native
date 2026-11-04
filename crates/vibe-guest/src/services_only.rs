//! `vibe-guest --vsock-dir DIR --services-only`, for tests on any OS: the guest's
//! shared services alone, with no agent, no mounts and no other port, which only
//! Linux can do. It's the one guest a Mac can run as an ordinary process. What
//! carries a service is the real thing, `vibe_relay`'s hub; what's here only takes
//! connections to it, on the same sockets the whole guest serves in its test mode.

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use vibe_relay::frame::VSOCK_PORT;
use vibe_relay::hub::{Hub, Timing};

/// How long one write to the Mac's relay may block, as in the whole guest.
const RELAY_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

fn dir_of(args: &[String]) -> Option<PathBuf> {
    let at = args.iter().position(|a| a == "--vsock-dir")?;
    args.get(at + 1).map(PathBuf::from)
}

fn bind(path: &Path) -> std::io::Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    UnixListener::bind(path)
}

/// Serves until it's killed, or says why it can't.
pub(crate) fn main(args: &[String]) -> ExitCode {
    let Some(dir) = dir_of(args) else {
        eprintln!("vibe-guest: --services-only needs --vsock-dir DIR");
        return ExitCode::from(2);
    };
    let hub = Hub::new(Box::new(|_, _| Ok(())), Timing::default());
    let bound = (
        bind(&dir.join("services.sock")),
        bind(&dir.join(VSOCK_PORT.to_string())),
    );
    let (shims, relays) = match bound {
        (Ok(shims), Ok(relays)) => (shims, relays),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("vibe-guest: can't serve in {}: {e}", dir.display());
            return ExitCode::FAILURE;
        }
    };
    {
        let hub = hub.clone();
        std::thread::spawn(move || {
            for conn in shims.incoming().flatten() {
                let hub = hub.clone();
                std::thread::spawn(move || hub.serve(conn));
            }
        });
    }
    for conn in relays.incoming().flatten() {
        let hub = hub.clone();
        std::thread::spawn(move || {
            let (Ok(reader), Ok(closer)) = (conn.try_clone(), conn.try_clone()) else {
                return;
            };
            if conn.set_write_timeout(Some(RELAY_WRITE_TIMEOUT)).is_err() {
                return;
            }
            let _ = hub.carry(reader, conn, move || {
                let _ = closer.shutdown(std::net::Shutdown::Both);
            });
        });
    }
    ExitCode::SUCCESS
}
