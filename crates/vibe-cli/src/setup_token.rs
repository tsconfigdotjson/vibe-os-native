//! `vibe setup-token`: the Claude login for `linux` agents (PRD: Key user
//! flows, Install and setup; Exceptions to "set up once, then frozen").
//!
//! It runs `claude setup-token`, the same browser sign-in as `/login`, on a
//! terminal of its own that this one is joined to, and picks the long-lived
//! token out of what it prints, joined up again if the terminal wrapped
//! it. The token goes to the Keychain, where
//! `vibed` reads it for every `linux` agent's launch; it's never written to
//! a file. If none can be found in the output, it asks for the token to be
//! pasted.

use std::ffi::{CStr, OsString};
use std::fs::File;
use std::io::{self, BufRead, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use vibe_core::claude_token::{self, TokenStore};

use crate::attach::{RawMode, is_tty, window_size};

/// The most of its output kept to look for the token in: the end of it.
const KEEP: usize = 1 << 20;

fn check(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

fn cloexec(fd: libc::c_int) -> io::Result<()> {
    // SAFETY: plain fcntl calls on a descriptor we own.
    unsafe {
        let flags = check(libc::fcntl(fd, libc::F_GETFD))?;
        check(libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC))?;
    }
    Ok(())
}

/// A new terminal pair, `rows` by `cols`.
fn open_pty(rows: u16, cols: u16) -> io::Result<(File, OwnedFd)> {
    // SAFETY: each call is checked; the descriptors are owned at once. This
    // runs before any other thread, as ptsname isn't thread-safe.
    unsafe {
        // posix_openpt takes no O_CLOEXEC on macOS: that's set after.
        let master =
            OwnedFd::from_raw_fd(check(libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY))?);
        cloexec(master.as_raw_fd())?;
        check(libc::grantpt(master.as_raw_fd()))?;
        check(libc::unlockpt(master.as_raw_fd()))?;
        let name = libc::ptsname(master.as_raw_fd());
        if name.is_null() {
            return Err(io::Error::last_os_error());
        }
        let name = CStr::from_ptr(name).to_owned();
        let slave = OwnedFd::from_raw_fd(check(libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY,
        ))?);
        cloexec(slave.as_raw_fd())?;
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        check(libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws))?;
        Ok((File::from(master), slave))
    }
}

/// Runs `claude setup-token` on a terminal joined to this one, and returns
/// how it exited and all it printed.
fn run_claude(claude: &OsString) -> io::Result<(std::process::ExitStatus, Vec<u8>)> {
    let (rows, cols) = window_size().map_or((24, 80), |s| (s.rows, s.cols));
    // This terminal's size, so its sign-in screen draws as it should.
    let (mut master, slave) = open_pty(rows, cols)?;
    let mut cmd = Command::new(claude);
    cmd.arg("setup-token")
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave.try_clone()?));
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            // A session of its own, whose controlling terminal is the new one.
            check(libc::setsid())?;
            check(libc::ioctl(0, libc::TIOCSCTTY as _, 0))?;
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("can't run {}: {e}", claude.to_string_lossy()),
        )
    })?;
    drop(cmd);
    drop(slave);
    // What's typed goes to it, raw, so its own line editing works.
    let raw = if is_tty(libc::STDIN_FILENO) {
        Some(RawMode::enter()?)
    } else {
        None
    };
    // What's typed is forwarded until Claude exits. Read from the descriptor
    // with a poll, not through `io::stdin()`, whose lock a blocked read
    // would hold: a paste asked for after must find stdin free.
    let mut to_child = master.try_clone()?;
    let done = Arc::new(AtomicBool::new(false));
    let typing = {
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while !done.load(Ordering::SeqCst) {
                let mut fds = libc::pollfd {
                    fd: libc::STDIN_FILENO,
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one pollfd, owned here.
                if unsafe { libc::poll(&mut fds, 1, 100) } <= 0 {
                    continue;
                }
                // SAFETY: reads into a buffer of its length.
                let n =
                    unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 || to_child.write_all(&buf[..n as usize]).is_err() {
                    break;
                }
            }
        })
    };
    let mut seen = Vec::new();
    let mut stdout = io::stdout();
    let mut buf = [0u8; 8192];
    loop {
        match master.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                stdout.write_all(&buf[..n])?;
                stdout.flush()?;
                seen.extend_from_slice(&buf[..n]);
                if seen.len() > KEEP {
                    seen.drain(..seen.len() - KEEP);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // Linux says EIO once the other side has closed.
            Err(_) => break,
        }
    }
    let status = child.wait()?;
    done.store(true, Ordering::SeqCst);
    let _ = typing.join();
    drop(raw);
    Ok((status, seen))
}

/// Reads a pasted secret from the terminal without echoing it.
pub(crate) fn read_hidden(prompt: &str) -> io::Result<String> {
    eprint!("{prompt}");
    io::stderr().flush()?;
    // SAFETY: termios is plain data, filled in by tcgetattr; echo is put
    // back below.
    let saved = unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        check(libc::tcgetattr(libc::STDIN_FILENO, &mut t))?;
        let mut quiet = t;
        quiet.c_lflag &= !libc::ECHO;
        check(libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &quiet))?;
        t
    };
    let mut line = String::new();
    let read = io::stdin().lock().read_line(&mut line);
    // SAFETY: restores what tcgetattr gave.
    unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &saved) };
    eprintln!();
    read?;
    Ok(line.trim().to_owned())
}

/// `vibe setup-token`, with `claude` as Claude Code, keeping the token in
/// `store`.
pub(crate) fn setup_token(claude: &OsString, store: &TokenStore) -> Result<(), String> {
    // Before the sign-in: a token that can't be kept is minted, shown and lost.
    store
        .ready()
        .map_err(|e| format!("can't keep a token: {e}"))?;
    let (status, seen) = run_claude(claude).map_err(|e| e.to_string())?;
    let token = match claude_token::find(&seen) {
        Some(token) => token,
        None if !status.success() => {
            return Err(format!("claude setup-token didn't finish ({status})"));
        }
        None if is_tty(libc::STDIN_FILENO) => {
            let pasted = read_hidden(
                "vibe: paste the token `claude setup-token` printed, then press Return: ",
            )
            .map_err(|e| e.to_string())?;
            claude_token::validate(&pasted).map_err(|e| format!("what was pasted {e}"))?;
            pasted
        }
        None => return Err("claude setup-token printed no token".into()),
    };
    store
        .store(&token)
        .map_err(|e| format!("can't keep the token: {e}"))?;
    let place = match store {
        TokenStore::Keychain { .. } => "the Keychain",
        TokenStore::File(_) => "the test token file",
    };
    // Its end, to check against what Claude showed, never the whole.
    let end = &token[token.len() - 6..];
    println!(
        "\r\nvibe: linux agents will sign in to Claude with the token ending …{end}, kept in {place}."
    );
    Ok(())
}
