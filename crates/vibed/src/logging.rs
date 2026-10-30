//! The daemon's log: every line goes to stderr and, once [`to_file`] is
//! called, to `vibed.log` in the logs directory with a timestamp. Anything
//! shaped like an API key is redacted first.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use vibe_core::keys;

/// Past this size, the log is moved to `vibed.log.1` when the daemon starts.
const ROTATE_BYTES: u64 = 10 << 20;

static FILE: OnceLock<Mutex<File>> = OnceLock::new();

/// Also writes the log to `path` from now on, private to this user. Only
/// the first call in a process takes effect.
pub fn to_file(path: &Path) -> io::Result<()> {
    if let Ok(meta) = fs::metadata(path)
        && meta.len() > ROTATE_BYTES
    {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        fs::rename(path, old)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    let _ = FILE.set(Mutex::new(file));
    Ok(())
}

/// Writes one line. Never panics, so a closed stderr or a full disk can't
/// take the daemon down.
pub fn write(args: fmt::Arguments<'_>) {
    let line = args.to_string();
    let line = keys::redact(&line);
    let _ = writeln!(io::stderr().lock(), "{line}");
    if let Some(file) = FILE.get()
        && let Ok(mut file) = file.lock()
    {
        let at = vibe_core::time::rfc3339(vibe_core::time::now());
        let _ = writeln!(file, "{at} {line}");
    }
}

/// Logs a line: see [`write`].
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::logging::write(format_args!($($arg)*))
    };
}
