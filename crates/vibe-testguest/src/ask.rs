//! `vibe-testguest ask`: a stand-in for Claude with an `AskUserQuestion`
//! pending, for the test that sleeps and wakes a VM under `vibe-guest`
//! (`crates/vibe-env/tests/vm_cycle.rs`). It draws a question and waits
//! for an answer, on a terminal in raw mode as Claude's TUI is, and says
//! what it sees of the guest `vibe-guest` gave it:
//!
//! ```text
//! as vibe uid 501 gid 20, pid 57
//! token set (24 bytes), XDG_RUNTIME_DIR /run/user/501 private, PATH from /opt/vibeos/bin
//! cwd /tmp/vmcX/guest/share
//! ```
//!
//! Keys: `1` or `2` answers; `r` reads `probe` in the working directory and
//! prints it with the guest's clock; `w` writes `written` there; `x` exits
//! with code 7.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;

pub(crate) fn run() -> ! {
    let mut out = io::stdout().lock();
    let raw = raw_mode();
    let _ = write!(out, "\x1b[2J\x1b[H");
    let _ = write!(out, "{}\r\n", who());
    let _ = write!(out, "{}\r\n", environment());
    let cwd = std::env::current_dir().map_or_else(|e| e.to_string(), |d| d.display().to_string());
    let _ = write!(out, "cwd {cwd}\r\n\r\n");
    let _ = write!(
        out,
        " \u{2610} Colour\r\n Which colour should the button be?\r\n \u{276f} 1. Red\r\n   2. Blue\r\n\r\n"
    );
    if let Err(e) = &raw {
        let _ = write!(out, "(no raw mode: {e})\r\n");
    }
    let _ = out.flush();
    let mut key = [0u8; 1];
    loop {
        let n = io::stdin().read(&mut key).unwrap_or(0);
        if n == 0 {
            std::process::exit(0);
        }
        let line = match key[0] {
            b'1' => "answered: Red".to_owned(),
            b'2' => "answered: Blue".to_owned(),
            b'r' => {
                let probe = fs::read_to_string("probe")
                    .map_or_else(|e| format!("error {e}"), |s| s.trim().to_owned());
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                format!("probe: {probe} clock: {now}")
            }
            b'w' => match fs::write("written", "by vibe\n") {
                Ok(()) => "wrote: ok".to_owned(),
                Err(e) => format!("wrote: error {e}"),
            },
            b'x' => std::process::exit(7),
            _ => continue,
        };
        let _ = write!(out, "{line}\r\n");
        let _ = out.flush();
    }
}

fn who() -> String {
    // SAFETY: these calls have no memory effects.
    let (uid, gid, pid) = unsafe { (libc::getuid(), libc::getgid(), libc::getpid()) };
    let user = std::env::var("USER").unwrap_or_default();
    format!("as {user} uid {uid} gid {gid}, pid {pid}")
}

fn environment() -> String {
    let token = match std::env::var("CLAUDE_CODE_OAUTH_TOKEN") {
        Ok(t) => format!("set ({} bytes)", t.len()),
        Err(_) => "unset".into(),
    };
    // SAFETY: getuid has no memory effects.
    let uid = unsafe { libc::getuid() };
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();
    let private = fs::metadata(&runtime)
        .is_ok_and(|m| m.uid() == uid && m.mode() & 0o777 == 0o700 && m.is_dir());
    let path = std::env::var("PATH").unwrap_or_default();
    let first = path.split(':').next().unwrap_or_default().to_owned();
    format!(
        "token {token}, XDG_RUNTIME_DIR {runtime} {}, PATH from {first}",
        if private { "private" } else { "NOT private" }
    )
}

/// Raw mode, as a TUI sets it: keys arrive one at a time, unechoed.
fn raw_mode() -> io::Result<()> {
    // SAFETY: termios is filled by tcgetattr before use.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut t) != 0 {
            return Err(io::Error::last_os_error());
        }
        libc::cfmakeraw(&mut t);
        if libc::tcsetattr(0, libc::TCSANOW, &t) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
