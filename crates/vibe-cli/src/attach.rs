//! `vibe attach <ref>`: this terminal, raw, joined to an agent's session
//! (PRD: How a click becomes a Terminal window). `vibed` says where the
//! session is; from then on bytes go straight to its shim, so restarting
//! `vibed` doesn't interrupt it. `Ctrl-\` detaches, as in dtach, and so does
//! closing the window (which `vibe toolbox` takes as Save & Close instead).
//! It needs only a terminal, so `ssh -t host vibe attach <ref>` works from
//! anywhere.

use std::fmt;
use std::io;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vibe_core::model::validate_ref;
use vibe_core::paths::Layout;
use vibe_core::rpc::{SessionAttach, SessionExit, SessionParams, SessionsAttach};
use vibe_core::shim::{self, Frame, FrameKind, Hello, PROTOCOL, Role, Size};

use crate::client::{self, ClientError};

/// `Ctrl-\`, as a terminal sends it.
const DETACH: u8 = 0x1c;

/// `Ctrl-\` in the kitty keyboard protocol, which a program may turn on:
/// plain, and with the event type a program may also ask for.
const DETACH_KITTY: [&[u8]; 2] = [b"\x1b[92;5u", b"\x1b[92;5:1u"];

/// What a program may have left on, put back the way a shell expects:
/// attributes, the cursor, bracketed paste, mouse reporting, application
/// cursor keys and keypad, and the kitty keyboard protocol.
const RESET: &str =
    "\x1b[0m\x1b[?25h\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1l\x1b>\x1b[<u";

#[derive(Debug)]
pub(crate) enum AttachError {
    NotATerminal,
    BadRef(String),
    Vibed(ClientError),
    Session(shim::ConnectError),
    Io(io::Error),
}

impl fmt::Display for AttachError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AttachError::NotATerminal => write!(
                f,
                "attach needs a terminal; over SSH, use `ssh -t <host> vibe attach <ref>`"
            ),
            AttachError::BadRef(e) => f.write_str(e),
            AttachError::Vibed(e) => e.fmt(f),
            AttachError::Session(e) => e.fmt(f),
            AttachError::Io(e) => write!(f, "the terminal: {e}"),
        }
    }
}

impl From<io::Error> for AttachError {
    fn from(e: io::Error) -> Self {
        AttachError::Io(e)
    }
}

/// How an attachment ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum End {
    Detached,
    /// The terminal hung up, as when its window closes: a detach, for an
    /// agent's session.
    HungUp,
    Exited(SessionExit),
    /// The shim went away without saying why.
    Lost,
}

fn check(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

pub(crate) fn is_tty(fd: libc::c_int) -> bool {
    // SAFETY: isatty only inspects the descriptor.
    unsafe { libc::isatty(fd) == 1 }
}

pub(crate) fn window_size() -> Option<Size> {
    // SAFETY: ws is written by the ioctl and read only if it succeeds.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        (libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ as _, &mut ws) == 0)
            .then_some(Size {
                rows: ws.ws_row,
                cols: ws.ws_col,
            })
            .filter(|s| s.is_valid())
    }
}

pub(crate) fn tty_name() -> Option<String> {
    // SAFETY: ttyname's buffer is copied at once, while only this thread
    // runs.
    unsafe {
        let name = libc::ttyname(libc::STDIN_FILENO);
        (!name.is_null()).then(|| {
            std::ffi::CStr::from_ptr(name)
                .to_string_lossy()
                .into_owned()
        })
    }
}

/// The terminal in raw mode, back as it was when dropped.
pub(crate) struct RawMode(libc::termios);

impl RawMode {
    pub(crate) fn enter() -> io::Result<RawMode> {
        // SAFETY: termios is plain data, filled in by tcgetattr.
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            check(libc::tcgetattr(libc::STDIN_FILENO, &mut saved))?;
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            check(libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw))?;
            Ok(RawMode(saved))
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restores the attributes saved in enter().
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0) };
    }
}

/// Where the signal handler sends the signals it catches.
static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);

/// Whether a hangup has arrived since [`catch_hangup`] or [`catch`].
static HUNG_UP: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "linux")]
fn errno() -> *mut libc::c_int {
    // SAFETY: returns this thread's errno.
    unsafe { libc::__errno_location() }
}

#[cfg(target_vendor = "apple")]
fn errno() -> *mut libc::c_int {
    // SAFETY: returns this thread's errno.
    unsafe { libc::__error() }
}

extern "C" fn on_signal(signal: libc::c_int) {
    if signal == libc::SIGHUP {
        HUNG_UP.store(true, Ordering::SeqCst);
    }
    let fd = SIGNAL_PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = signal as u8;
        // SAFETY: write is async-signal-safe; errno is put back for the
        // code this interrupted.
        unsafe {
            let saved = *errno();
            libc::write(fd, (&byte as *const u8).cast(), 1);
            *errno() = saved;
        }
    }
}

fn install_handler(signal: libc::c_int) -> io::Result<()> {
    // SAFETY: the handler only sets a flag and writes to the pipe.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_signal as *const () as libc::sighandler_t;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        check(libc::sigaction(signal, &action, std::ptr::null_mut()))?;
    }
    Ok(())
}

/// Notes a hangup from now on, rather than dying of it, for a program with
/// something to finish before it attaches: [`hung_up`] says if one came.
pub(crate) fn catch_hangup() -> io::Result<()> {
    install_handler(libc::SIGHUP)
}

/// Whether the terminal has hung up since [`catch_hangup`].
pub(crate) fn hung_up() -> bool {
    HUNG_UP.load(Ordering::SeqCst)
}

/// Catches `signals`, which are then read from the returned pipe, a byte
/// each.
fn catch(signals: &[libc::c_int]) -> io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    let mut fds = [0; 2];
    // SAFETY: fds has room for both ends.
    check(unsafe { libc::pipe(fds.as_mut_ptr()) })?;
    // SAFETY: plain fcntl calls on the descriptors pipe just made. Children
    // mustn't inherit them, and a full pipe must never block the handler.
    unsafe {
        for fd in fds {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        let flags = libc::fcntl(fds[1], libc::F_GETFL);
        libc::fcntl(fds[1], libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    SIGNAL_PIPE.store(fds[1], Ordering::Relaxed);
    for &signal in signals {
        install_handler(signal)?;
    }
    // SAFETY: pipe made it, and only this owns the read end.
    Ok(unsafe { std::fs::File::from_raw_fd(fds[0]) })
}

/// Drops the program's own window-title sequences (OSC 0, 1 and 2) from
/// what it writes, so the window keeps the title `vibe attach` gave it; and
/// notes whether the alternate screen is in use. Sequences split across
/// reads are held until they're whole.
#[derive(Debug, Default)]
pub(crate) struct Filter {
    state: FilterState,
    held: Vec<u8>,
    pub(crate) alternate: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum FilterState {
    #[default]
    Ground,
    Escape,
    Csi,
    Osc,
    OscEscape,
}

/// Past this, a sequence is passed through as it is rather than held.
const MAX_HELD: usize = 4096;

impl Filter {
    pub(crate) fn feed(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        for &b in bytes {
            match self.state {
                FilterState::Ground => {
                    if b == 0x1b {
                        self.held.push(b);
                        self.state = FilterState::Escape;
                    } else {
                        out.push(b);
                    }
                }
                FilterState::Escape => {
                    self.held.push(b);
                    self.state = match b {
                        b'[' => FilterState::Csi,
                        b']' => FilterState::Osc,
                        _ => {
                            out.append(&mut self.held);
                            FilterState::Ground
                        }
                    };
                }
                FilterState::Csi => {
                    self.held.push(b);
                    if (0x40..=0x7e).contains(&b) {
                        self.note_mode();
                        out.append(&mut self.held);
                        self.state = FilterState::Ground;
                    }
                }
                FilterState::Osc => {
                    if b == 0x07 {
                        self.held.push(b);
                        self.end_osc(out);
                    } else if b == 0x1b {
                        self.held.push(b);
                        self.state = FilterState::OscEscape;
                    } else {
                        self.held.push(b);
                    }
                }
                FilterState::OscEscape => {
                    self.held.push(b);
                    if b == b'\\' {
                        self.end_osc(out);
                    } else {
                        // Not a terminator: pass it all on as it came.
                        out.append(&mut self.held);
                        self.state = FilterState::Ground;
                    }
                }
            }
            if self.held.len() > MAX_HELD {
                out.append(&mut self.held);
                self.state = FilterState::Ground;
            }
        }
    }

    fn end_osc(&mut self, out: &mut Vec<u8>) {
        let title = [b"\x1b]0;", b"\x1b]1;", b"\x1b]2;"]
            .iter()
            .any(|p| self.held.starts_with(*p));
        if title {
            self.held.clear();
        } else {
            out.append(&mut self.held);
        }
        self.state = FilterState::Ground;
    }

    /// Follows the alternate screen: `CSI ? 1049 h` and its older forms.
    fn note_mode(&mut self) {
        let Some(body) = self.held.strip_prefix(b"\x1b[?") else {
            return;
        };
        let (params, set) = match body.split_last() {
            Some((b'h', p)) => (p, true),
            Some((b'l', p)) => (p, false),
            _ => return,
        };
        let alternate = params
            .split(|&c| c == b';')
            .any(|p| matches!(p, b"1049" | b"1047" | b"47"));
        if alternate {
            self.alternate = set;
        }
    }
}

/// Where `vibed` says the session is; or, with `vibed` down, where it would
/// be, so a session can be attached to while `vibed` restarts.
fn locate(socket: &Path, agent: &str) -> Result<SessionAttach, AttachError> {
    let params = SessionParams {
        host: None,
        agent: agent.to_owned(),
    };
    match client::call::<SessionsAttach>(socket, &params, Duration::from_secs(10)) {
        Ok(found) => Ok(found),
        Err(ClientError::Connect { .. }) => {
            let layout = Layout::current().map_err(|e| AttachError::BadRef(e.to_string()))?;
            let path: PathBuf = layout.session_socket(agent);
            Ok(SessionAttach {
                host: vibe_core::model::LOCAL_HOST.into(),
                agent: agent.to_owned(),
                title: agent.to_owned(),
                socket: path.to_string_lossy().into_owned(),
                ttys: Vec::new(),
            })
        }
        Err(e) => Err(AttachError::Vibed(e)),
    }
}

/// Why the session's stream stopped, when this end stopped it.
static STOPPED_BY: AtomicU8 = AtomicU8::new(0);
const BY_DETACH: u8 = 1;
const BY_HANGUP: u8 = 2;

/// Notes why this end stops the stream: the first reason given stands, as
/// a hangup also ends the terminal's input.
fn stop(by: u8) {
    let _ = STOPPED_BY.compare_exchange(0, by, Ordering::SeqCst, Ordering::SeqCst);
}

/// How the attachment ended, its stream having stopped.
fn stopped() -> End {
    match STOPPED_BY.load(Ordering::SeqCst) {
        BY_DETACH => End::Detached,
        BY_HANGUP => End::HungUp,
        _ => End::Lost,
    }
}

/// The terminal has gone, as its setup or a write to it failing says: a
/// hangup, whether or not `SIGHUP` has got here yet.
fn terminal_gone() -> End {
    stop(BY_HANGUP);
    stopped()
}

/// Writes `bytes` to the terminal. Some programs leave it non-blocking,
/// which isn't its end: this waits for room. Any other error is.
fn to_terminal(mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        // SAFETY: writes from bytes, within its length.
        let n = unsafe { libc::write(libc::STDOUT_FILENO, bytes.as_ptr().cast(), bytes.len()) };
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        if n > 0 {
            bytes = &bytes[n as usize..];
            continue;
        }
        let e = io::Error::last_os_error();
        match e.kind() {
            io::ErrorKind::Interrupted => {}
            io::ErrorKind::WouldBlock => {
                let mut pfd = libc::pollfd {
                    fd: libc::STDOUT_FILENO,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                // SAFETY: pfd is valid for the call.
                unsafe { libc::poll(&mut pfd, 1, -1) };
            }
            _ => return Err(e),
        }
    }
    Ok(())
}

/// Taking over the terminal failed. If it's no longer a terminal, it has
/// gone: a hangup. If it still is, that's a real error: `tcsetattr` also
/// fails for an orphaned background job, whose terminal is still there, and
/// which must not end the session or save the toolbox for it.
fn raw_mode_failed(e: io::Error, still_a_terminal: bool) -> Result<End, AttachError> {
    if still_a_terminal {
        Err(e.into())
    } else {
        Ok(terminal_gone())
    }
}

pub(crate) fn attach(socket: &Path, agent: &str) -> Result<End, AttachError> {
    if !is_tty(libc::STDIN_FILENO) || !is_tty(libc::STDOUT_FILENO) {
        return Err(AttachError::NotATerminal);
    }
    validate_ref(agent).map_err(AttachError::BadRef)?;
    let found = locate(socket, agent)?;
    attach_to(&found)
}

/// Joins this terminal to the session `found` says, as [`attach`] does.
pub(crate) fn attach_to(found: &SessionAttach) -> Result<End, AttachError> {
    if !is_tty(libc::STDIN_FILENO) || !is_tty(libc::STDOUT_FILENO) {
        return Err(AttachError::NotATerminal);
    }
    let agent = found.agent.as_str();
    STOPPED_BY.store(0, Ordering::SeqCst);
    // Caught before the size is read, so a resize in between isn't lost.
    // SIGHUP too: closing the window must detach even if whoever started
    // this left hangups ignored.
    let signals = catch(&[
        libc::SIGWINCH,
        libc::SIGHUP,
        libc::SIGTERM,
        libc::SIGINT,
        libc::SIGQUIT,
    ])?;
    // One that came before the pipe was there.
    if hung_up() {
        return Ok(terminal_gone());
    }
    let hello = Hello {
        protocol: PROTOCOL,
        role: Role::Attach,
        client: "vibe-cli".into(),
        tty: tty_name(),
        size: window_size(),
    };
    let conn = shim::connect(Path::new(&found.socket), &hello, Duration::from_secs(10))
        .map_err(AttachError::Session)?;
    let stream = conn.stream;
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let raw = match RawMode::enter() {
        Ok(raw) => raw,
        Err(e) => return raw_mode_failed(e, is_tty(libc::STDIN_FILENO)),
    };
    if to_terminal(format!("\x1b]0;{}\x07", found.title).as_bytes()).is_err() {
        return Ok(terminal_gone());
    }

    spawn_keys(stream.try_clone()?, writer.clone());
    spawn_signals(signals, stream.try_clone()?, writer);

    let mut filter = Filter::default();
    let mut out = Vec::new();
    let mut reading = &stream;
    let end = loop {
        let frame = match shim::read_frame(&mut reading) {
            Ok(Some(f)) => f,
            Ok(None) | Err(_) => break stopped(),
        };
        match frame.kind {
            FrameKind::Output => {
                out.clear();
                filter.feed(&frame.payload, &mut out);
                if to_terminal(&out).is_err() {
                    break terminal_gone();
                }
            }
            FrameKind::Exit => match frame.decode() {
                Ok(exit) => break End::Exited(exit),
                Err(_) => break End::Lost,
            },
            _ => {}
        }
    };
    let _ = stream.shutdown(Shutdown::Both);
    drop(raw);
    let leave_alternate = if filter.alternate { "\x1b[?1049l" } else { "" };
    let said = match &end {
        End::Detached | End::HungUp => format!("detached from {agent}"),
        End::Exited(exit) => match (&exit.code, &exit.signal) {
            (Some(code), _) => format!("{agent} exited with code {code}"),
            (None, Some(signal)) => format!("{agent} was ended by {signal}"),
            (None, None) => format!("{agent} ended"),
        },
        End::Lost => format!("lost the session {agent}"),
    };
    // After a hangup the terminal is gone; saying goodbye is best effort.
    let _ = to_terminal(format!("{RESET}{leave_alternate}\r\n[{said}]\r\n").as_bytes());
    Ok(end)
}

/// Sends what's typed to the session, until `Ctrl-\` or the end of input.
/// A raw terminal has no end of input of its own (`Ctrl-D` is a key like
/// any other), so its end is the terminal going away: a hangup.
fn spawn_keys(stream: UnixStream, writer: Arc<Mutex<UnixStream>>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let by = loop {
            // SAFETY: reads into buf, within its length.
            let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                match io::Error::last_os_error().kind() {
                    io::ErrorKind::Interrupted => continue,
                    // Some programs leave the terminal non-blocking; wait
                    // for input rather than take this as its end.
                    io::ErrorKind::WouldBlock => {
                        let mut pfd = libc::pollfd {
                            fd: libc::STDIN_FILENO,
                            events: libc::POLLIN,
                            revents: 0,
                        };
                        // SAFETY: pfd is valid for the call.
                        unsafe { libc::poll(&mut pfd, 1, -1) };
                        continue;
                    }
                    _ => {}
                }
            }
            if n <= 0 {
                break BY_HANGUP;
            }
            let typed = &buf[..n as usize];
            let (keys, detach) = split_detach(typed);
            if !keys.is_empty() {
                let frame = Frame::new(FrameKind::Input, keys);
                let mut w = writer.lock().unwrap_or_else(|e| e.into_inner());
                if shim::write_frame(&mut *w, &frame).is_err() {
                    return;
                }
            }
            if detach {
                break BY_DETACH;
            }
        };
        stop(by);
        let _ = stream.shutdown(Shutdown::Both);
    });
}

/// What to send of `typed`, and whether it asks to detach: everything
/// before the first detach key.
pub(crate) fn split_detach(typed: &[u8]) -> (&[u8], bool) {
    let plain = typed.iter().position(|&b| b == DETACH);
    let kitty = DETACH_KITTY
        .iter()
        .filter_map(|key| typed.windows(key.len()).position(|w| w == *key))
        .min();
    match (plain, kitty) {
        (None, None) => (typed, false),
        (Some(i), None) | (None, Some(i)) => (&typed[..i], true),
        (Some(a), Some(b)) => (&typed[..a.min(b)], true),
    }
}

/// Passes size changes on, and ends the attachment on a polite kill.
fn spawn_signals(mut signals: std::fs::File, stream: UnixStream, writer: Arc<Mutex<UnixStream>>) {
    use std::io::Read;
    std::thread::spawn(move || {
        let mut byte = [0u8; 1];
        while signals.read_exact(&mut byte).is_ok() {
            if libc::c_int::from(byte[0]) == libc::SIGWINCH {
                let Some(size) = window_size() else { continue };
                let frame = Frame::json(FrameKind::Resize, &size);
                let mut w = writer.lock().unwrap_or_else(|e| e.into_inner());
                if shim::write_frame(&mut *w, &frame).is_err() {
                    return;
                }
            } else {
                // A hangup is told apart, for `vibe toolbox`; any other
                // polite kill is a detach.
                let by = if libc::c_int::from(byte[0]) == libc::SIGHUP {
                    BY_HANGUP
                } else {
                    BY_DETACH
                };
                stop(by);
                let _ = stream.shutdown(Shutdown::Both);
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(chunks: &[&[u8]]) -> (Vec<u8>, Filter) {
        let mut f = Filter::default();
        let mut out = Vec::new();
        for c in chunks {
            f.feed(c, &mut out);
        }
        (out, f)
    }

    #[test]
    fn titles_are_dropped_and_the_rest_kept() {
        let (out, _) = filter(&[b"a\x1b]0;claude\x07b\x1b]2;x\x1b\\c\x1b]1;icon\x07d"]);
        assert_eq!(out, b"abcd");
        // Other OSCs, CSIs and plain escapes pass through untouched.
        let keep: &[u8] = b"\x1b]8;;https://x\x07link\x1b]8;;\x07\x1b[31mred\x1b[0m\x1b7\x1bM";
        assert_eq!(filter(&[keep]).0, keep);
    }

    #[test]
    fn sequences_split_across_reads() {
        let (out, _) = filter(&[b"a\x1b", b"]0;ti", b"tle\x1b", b"\\b\x1b[3", b"1mc"]);
        assert_eq!(out, b"ab\x1b[31mc");
    }

    #[test]
    fn a_runaway_sequence_is_passed_on() {
        let mut long = b"\x1b]0;".to_vec();
        long.extend(std::iter::repeat_n(b'x', MAX_HELD + 10));
        let (out, f) = filter(&[&long, b"\x07z"]);
        assert!(out.ends_with(b"x\x07z"));
        assert_eq!(f.state, FilterState::Ground);
        // An escape that isn't a terminator ends the OSC.
        assert_eq!(filter(&[b"\x1b]9;n\x1bXy"]).0, b"\x1b]9;n\x1bXy");
    }

    #[test]
    fn the_alternate_screen_is_followed() {
        let (_, f) = filter(&[b"\x1b[?1049h"]);
        assert!(f.alternate);
        let (_, f) = filter(&[b"\x1b[?1049h", b"\x1b[?1049l"]);
        assert!(!f.alternate);
        let (_, f) = filter(&[b"\x1b[?25;47h"]);
        assert!(f.alternate);
        let (_, f) = filter(&[b"\x1b[?2004h\x1b[1049h"]);
        assert!(!f.alternate, "not a private mode");
    }

    #[test]
    fn failing_to_take_over_the_terminal_is_a_hangup_only_if_it_has_gone() {
        STOPPED_BY.store(0, Ordering::SeqCst);
        let e = || io::Error::from_raw_os_error(libc::EIO);
        assert!(matches!(
            raw_mode_failed(e(), true),
            Err(AttachError::Io(_))
        ));
        assert_eq!(STOPPED_BY.load(Ordering::SeqCst), 0, "no hangup was noted");
        assert!(matches!(raw_mode_failed(e(), false), Ok(End::HungUp)));
    }

    #[test]
    fn the_detach_key() {
        assert_eq!(split_detach(b"ls\r"), (&b"ls\r"[..], false));
        assert_eq!(split_detach(b"ab\x1ccd"), (&b"ab"[..], true));
        assert_eq!(split_detach(b"\x1c"), (&b""[..], true));
        assert_eq!(split_detach(b"x\x1b[92;5u"), (&b"x"[..], true));
        assert_eq!(split_detach(b"a\x1b[92;5ub\x1c"), (&b"a"[..], true));
        assert_eq!(split_detach(b"ab\x1b[92;5:1u"), (&b"ab"[..], true));
        // Ctrl-\ with Shift is another key.
        assert_eq!(split_detach(b"\x1b[92;6u"), (&b"\x1b[92;6u"[..], false));
    }
}
