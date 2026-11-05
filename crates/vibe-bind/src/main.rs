//! `vibe-bind`: opens port 443 for `vibed`, which runs as its user, and hands it
//! over (PRD: External access). Linux keeps ports below 1024 for root, and this is
//! the least that gets round that: a root-owned copy of this binary, which only the
//! owner's group can run, holds `cap_net_bind_service` as a file capability. It runs
//! for a moment, can open this one port and no other, and passes the listening
//! socket down its stdin, a Unix socket from `vibed`. So nothing's changed for the
//! rest of the system, and upgrades of `vibed` leave it as it is.
//!
//! Change this only if it must: a new build means root again, so it'd go under a
//! new name (`vibe-bind-2`).
//!
//! `--check` only says whether the port could be opened: exit 0, or 13 if that's
//! denied. Without it, 98 means something else has the port.

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux::main()
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("vibe-bind: for a Linux host");
    std::process::ExitCode::from(2)
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::process::ExitCode;

    const PORT: u16 = 443;

    /// 443, but for a debug build in a test: a release never reads this, so what's
    /// installed can't be pointed at another port.
    fn port() -> u16 {
        if cfg!(debug_assertions)
            && let Some(port) = std::env::var("VIBE_BIND_TEST_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
        {
            return port;
        }
        PORT
    }

    /// `result` as an `io::Result`, for the syscalls here, which say -1.
    fn checked(result: libc::c_int) -> io::Result<libc::c_int> {
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result)
        }
    }

    /// A socket of `family` bound to `addr`, and listening.
    fn listen(family: libc::c_int, addr: *const libc::sockaddr, len: usize) -> io::Result<OwnedFd> {
        // SAFETY: each call is given a descriptor this function owns and pointers
        // valid for the call.
        unsafe {
            let fd = OwnedFd::from_raw_fd(checked(libc::socket(family, libc::SOCK_STREAM, 0))?);
            let on: libc::c_int = 1;
            let off: libc::c_int = 0;
            let int = size_of::<libc::c_int>() as libc::socklen_t;
            if family == libc::AF_INET6 {
                checked(libc::setsockopt(
                    fd.as_raw_fd(),
                    libc::IPPROTO_IPV6,
                    libc::IPV6_V6ONLY,
                    (&raw const off).cast(),
                    int,
                ))?;
            }
            checked(libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                (&raw const on).cast(),
                int,
            ))?;
            checked(libc::bind(fd.as_raw_fd(), addr, len as libc::socklen_t))?;
            checked(libc::listen(fd.as_raw_fd(), 1024))?;
            Ok(fd)
        }
    }

    /// The port on every IPv6 and IPv4 address, or IPv4 alone on a machine with no
    /// IPv6, as `vibed`'s own `bind_public` does.
    fn open(port: u16) -> io::Result<OwnedFd> {
        // SAFETY: all-zero is a valid sockaddr_in6, and it's only read by `listen`.
        let mut six: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
        six.sin6_family = libc::AF_INET6 as libc::sa_family_t;
        six.sin6_port = port.to_be();
        match listen(
            libc::AF_INET6,
            (&raw const six).cast(),
            size_of::<libc::sockaddr_in6>(),
        ) {
            Ok(fd) => Ok(fd),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::AddrInUse
                ) =>
            {
                Err(e)
            }
            Err(_) => {
                // SAFETY: as above.
                let mut four: libc::sockaddr_in = unsafe { std::mem::zeroed() };
                four.sin_family = libc::AF_INET as libc::sa_family_t;
                four.sin_port = port.to_be();
                listen(
                    libc::AF_INET,
                    (&raw const four).cast(),
                    size_of::<libc::sockaddr_in>(),
                )
            }
        }
    }

    /// Sends `fd` down the Unix socket on stdin.
    fn send(fd: &OwnedFd) -> io::Result<()> {
        // SAFETY: the control buffer is sized by CMSG_SPACE and aligned for a
        // cmsghdr, the iovec and the byte it names outlive the call, and `fd` stays
        // open across it.
        unsafe {
            let mut control = [0u64; 4];
            let mut byte = 0u8;
            let mut iov = libc::iovec {
                iov_base: (&raw mut byte).cast(),
                iov_len: 1,
            };
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &raw mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = libc::CMSG_SPACE(size_of::<libc::c_int>() as u32) as _;
            let header = libc::CMSG_FIRSTHDR(&raw const msg);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(size_of::<libc::c_int>() as u32) as _;
            libc::CMSG_DATA(header)
                .cast::<libc::c_int>()
                .write_unaligned(fd.as_raw_fd());
            if libc::sendmsg(libc::STDIN_FILENO, &raw const msg, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn code(e: &io::Error) -> ExitCode {
        ExitCode::from(match e.kind() {
            io::ErrorKind::PermissionDenied => 13,
            io::ErrorKind::AddrInUse => 98,
            _ => 1,
        })
    }

    pub(super) fn main() -> ExitCode {
        let check = match std::env::args().nth(1).as_deref() {
            None => false,
            Some("--check") => true,
            Some(other) => {
                eprintln!("vibe-bind: unexpected {other}");
                return ExitCode::from(2);
            }
        };
        let port = port();
        match open(port) {
            Ok(_) if check => ExitCode::SUCCESS,
            // Allowed, and something else has it just now.
            Err(e) if check && e.kind() == io::ErrorKind::AddrInUse => ExitCode::SUCCESS,
            Ok(fd) => match send(&fd) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("vibe-bind: can't hand over port {port}: {e}");
                    ExitCode::FAILURE
                }
            },
            Err(e) => {
                eprintln!("vibe-bind: can't open port {port}: {e}");
                code(&e)
            }
        }
    }
}
