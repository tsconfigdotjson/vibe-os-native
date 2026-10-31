//! The pseudo-terminal and the command on it. The shim leads its own
//! session, and the terminal is that session's controlling terminal, so
//! every process the command starts carries the shim's pid as its session
//! ID (how the `macos` governor finds them), and if the shim dies the
//! kernel hangs up the command (PRD: Failures).

use std::ffi::{CStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use vibe_core::shim::Size;

fn check(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

fn winsize(size: Size) -> libc::winsize {
    libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// Sets the terminal's size; the kernel tells its foreground process group
/// with `SIGWINCH`.
pub(crate) fn set_size(fd: RawFd, size: Size) -> io::Result<()> {
    let ws = winsize(size);
    // SAFETY: fd is a terminal we own, and ws outlives the call.
    check(unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &ws) }).map(drop)
}

fn cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: plain fcntl calls on a descriptor we own.
    unsafe {
        let flags = check(libc::fcntl(fd, libc::F_GETFD))?;
        check(libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC))?;
    }
    Ok(())
}

/// A new terminal pair, master and slave, neither inherited by children.
/// `posix_openpt` rather than `openpty`, which needs libutil on older glibc.
/// Called while the process has one thread, as `ptsname` isn't thread-safe.
fn open_pty(size: Size) -> io::Result<(OwnedFd, OwnedFd)> {
    // SAFETY: each call is checked; the descriptors are owned right away.
    unsafe {
        let master = check(libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY))?;
        let master = OwnedFd::from_raw_fd(master);
        cloexec(master.as_raw_fd())?;
        check(libc::grantpt(master.as_raw_fd()))?;
        check(libc::unlockpt(master.as_raw_fd()))?;
        let name = libc::ptsname(master.as_raw_fd());
        if name.is_null() {
            return Err(io::Error::last_os_error());
        }
        let name = CStr::from_ptr(name).to_owned();
        let slave = check(libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY))?;
        let slave = OwnedFd::from_raw_fd(slave);
        cloexec(slave.as_raw_fd())?;
        set_size(master.as_raw_fd(), size)?;
        Ok((master, slave))
    }
}

/// The command running on the terminal.
#[derive(Debug)]
pub(crate) struct Pty {
    /// The master end: what the command writes, and where input goes.
    pub(crate) master: File,
    pub(crate) child: Child,
}

/// Makes this process, which must lead its session, own a new terminal of
/// `size`, and starts `command` on it in `cwd`, with `tmp` as its `TMPDIR`,
/// as the terminal's foreground process group. The caller ignores
/// `SIGHUP`, as it owns the terminal and closing the master end, as a failed
/// start does, hangs it up; and `SIGTTOU`, as it isn't the terminal's
/// foreground.
pub(crate) fn spawn(command: &[OsString], cwd: &Path, tmp: &Path, size: Size) -> io::Result<Pty> {
    let (master, slave) = open_pty(size)?;
    // SAFETY: we lead our session and have no controlling terminal yet; this
    // makes the slave ours.
    check(unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY as _, 0) })?;
    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..])
        .current_dir(cwd)
        // What the screen model and the terminals that attach understand.
        .env("TERM", "xterm-256color")
        .env("TMPDIR", tmp)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave.try_clone()?));
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            // Its own process group, in our session, in the foreground of
            // the terminal. Taking the foreground from the background would
            // stop us with SIGTTOU, so that's ignored meanwhile.
            check(libc::setpgid(0, 0))?;
            libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            check(libc::tcsetpgrp(0, libc::getpid()))?;
            // The shim ignores hangups, and whoever started vibed may have
            // left others ignored; the command gets the defaults.
            for signal in [
                libc::SIGHUP,
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTERM,
                libc::SIGTSTP,
                libc::SIGTTIN,
                libc::SIGTTOU,
            ] {
                libc::signal(signal, libc::SIG_DFL);
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    // The command has the slave now. Ours close, so reading the master ends
    // once the command's side is closed too.
    drop(cmd);
    drop(slave);
    Ok(Pty {
        master: File::from(master),
        child,
    })
}
