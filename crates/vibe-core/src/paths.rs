//! Where `vibed`'s socket lives. Clients and the daemon resolve it the same
//! way; `protocol/vibed-rpc.json` records the defaults for the Swift side.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

/// Overrides the socket path for the daemon and every client.
pub const SOCKET_ENV: &str = "VIBE_SOCKET";

/// The socket path relative to `$HOME` on macOS.
pub const MACOS_SOCKET: &str = "Library/Application Support/VibeOS/run/vibed.sock";

/// The socket path relative to `$HOME` on a Linux host.
pub const LINUX_SOCKET: &str = ".local/share/vibeos/run/vibed.sock";

/// The default socket path for `os` (as in `std::env::consts::OS`), relative
/// to `$HOME`. Every Unix other than macOS uses the Linux layout.
pub fn default_socket_relative(os: &str) -> &'static str {
    if os == "macos" {
        MACOS_SOCKET
    } else {
        LINUX_SOCKET
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoHomeError;

impl fmt::Display for NoHomeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "can't find the vibed socket: $HOME is not set (set {SOCKET_ENV})"
        )
    }
}

impl std::error::Error for NoHomeError {}

/// Resolves the socket path: an explicit path wins, then `$VIBE_SOCKET`, then
/// the platform default under `$HOME`. Empty values count as unset.
pub fn resolve_socket(
    explicit: Option<PathBuf>,
    env_socket: Option<OsString>,
    home: Option<OsString>,
    os: &str,
) -> Result<PathBuf, NoHomeError> {
    let nonempty = |v: OsString| (!v.is_empty()).then_some(v);
    if let Some(path) = explicit.filter(|p| !p.as_os_str().is_empty()) {
        return Ok(path);
    }
    if let Some(path) = env_socket.and_then(nonempty) {
        return Ok(PathBuf::from(path));
    }
    let home = home.and_then(nonempty).ok_or(NoHomeError)?;
    Ok(PathBuf::from(home).join(default_socket_relative(os)))
}

/// [`resolve_socket`] for this process: its environment and its OS.
pub fn socket_path(explicit: Option<PathBuf>) -> Result<PathBuf, NoHomeError> {
    resolve_socket(
        explicit,
        std::env::var_os(SOCKET_ENV),
        std::env::var_os("HOME"),
        std::env::consts::OS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    #[test]
    fn explicit_path_wins() {
        let p = resolve_socket(
            Some("/a.sock".into()),
            os("/b.sock"),
            os("/home/u"),
            "linux",
        );
        assert_eq!(p, Ok(PathBuf::from("/a.sock")));
    }

    #[test]
    fn env_beats_default() {
        let p = resolve_socket(None, os("/b.sock"), os("/home/u"), "macos");
        assert_eq!(p, Ok(PathBuf::from("/b.sock")));
    }

    #[test]
    fn defaults_per_os() {
        assert_eq!(
            resolve_socket(None, None, os("/Users/u"), "macos"),
            Ok(PathBuf::from(
                "/Users/u/Library/Application Support/VibeOS/run/vibed.sock"
            ))
        );
        assert_eq!(
            resolve_socket(None, None, os("/home/u"), "linux"),
            Ok(PathBuf::from("/home/u/.local/share/vibeos/run/vibed.sock"))
        );
        assert_eq!(default_socket_relative("freebsd"), LINUX_SOCKET);
    }

    #[test]
    fn empty_values_count_as_unset() {
        let p = resolve_socket(Some(PathBuf::new()), os(""), os("/home/u"), "linux");
        assert_eq!(p, Ok(PathBuf::from("/home/u").join(LINUX_SOCKET)));
        assert_eq!(
            resolve_socket(None, os(""), os(""), "linux"),
            Err(NoHomeError)
        );
        assert_eq!(resolve_socket(None, None, None, "linux"), Err(NoHomeError));
    }
}
