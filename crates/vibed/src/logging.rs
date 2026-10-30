//! The daemon's log: every line goes to stderr and, once [`to_file`] is
//! called, to `vibed.log` in the logs directory with a timestamp. Anything
//! shaped like an API key or a GitHub or Claude token is redacted first.

use std::borrow::Cow;
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
    let line = redact_tokens(&keys::redact(&line)).into_owned();
    let _ = writeln!(io::stderr().lock(), "{line}");
    if let Some(file) = FILE.get()
        && let Ok(mut file) = file.lock()
    {
        let at = vibe_core::time::rfc3339(vibe_core::time::now());
        let _ = writeln!(file, "{at} {line}");
    }
}

/// How tokens other than ours start: GitHub's (`gh` and git credentials)
/// and Claude's.
const TOKEN_PREFIXES: [&str; 7] = [
    "github_pat_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "sk-ant-",
];

/// Shortest secret part a token has; shorter runs are left alone.
const TOKEN_MIN: usize = 16;

/// Replaces the secret part of anything shaped like a GitHub or Claude
/// token with `…`, keeping its prefix to say what it was.
pub fn redact_tokens(text: &str) -> Cow<'_, str> {
    if !TOKEN_PREFIXES.iter().any(|p| text.contains(p)) {
        return Cow::Borrowed(text);
    }
    let secret = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let found = TOKEN_PREFIXES
            .iter()
            .find(|p| text[i..].starts_with(*p) && (i == 0 || !secret(bytes[i - 1])));
        if let Some(prefix) = found {
            let start = i + prefix.len();
            let len = bytes[start..].iter().take_while(|b| secret(**b)).count();
            if len >= TOKEN_MIN {
                out.push_str(prefix);
                out.push('…');
                i = start + len;
                continue;
            }
        }
        let c = text[i..].chars().next().expect("i is on a char boundary");
        out.push(c);
        i += c.len_utf8();
    }
    Cow::Owned(out)
}

/// Logs a line: see [`write`].
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::logging::write(format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_redacted() {
        let line =
            "push with ghp_abcdefghijklmnopqrstuvwxyz0123 and sk-ant-oat01-ABCDEFGHIJKLMNOP_q, ok";
        assert_eq!(redact_tokens(line), "push with ghp_… and sk-ant-…, ok");
        let pat = "token=github_pat_11ABCDEFG0123456789_abcdefghij";
        assert_eq!(redact_tokens(pat), "token=github_pat_…");
        // Short runs and look-alikes inside words stay.
        for keep in [
            "ghp_short",
            "the ghs_ prefix",
            "xghp_abcdefghijklmnopqrstuvwxyz",
            "é ghp_é",
        ] {
            assert_eq!(redact_tokens(keep), keep);
        }
        assert!(matches!(redact_tokens("nothing here"), Cow::Borrowed(_)));
    }
}
