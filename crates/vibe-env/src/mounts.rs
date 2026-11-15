//! A native agent's mounts are its own (#169).
//!
//! On most Linux servers `/` and everything under it is mounted *shared*
//! (systemd's default), and a native agent ran in the server's own mount
//! namespace. An agent running as root that bind-mounted `/sys` or `/dev`
//! recursively into a scratch root, as building a chroot or an image does,
//! put each copy in the same peer group as the server's mount; unmounting
//! the copies recursively then unmounted the server's own `/sys/fs/cgroup`,
//! `/dev/pts`, `/dev/shm` and more, which left systemd unable to make any
//! unit and every new terminal failing with `ENODEV`.
//!
//! So an agent's processes start in a mount namespace of their own whose
//! mounts are *slaves* of the server's ([`contain`]): what the server mounts
//! and unmounts still reaches the agent, and nothing the agent mounts or
//! unmounts reaches the server. The namespace is the agent's whole session
//! (its terminal's command and everything it starts) and each of its
//! `.vibe/setup.sh` runs; it goes when its last process does, and so do the
//! agent's own mounts. An agent that can't make one can't mount anything on
//! the server either, as both need `CAP_SYS_ADMIN`.
//!
//! [`cgroup2_missing`] and [`terminal_missing`] say plainly when the
//! server has lost what agents need, rather than leaving it to whatever
//! fails first to name the wrong thing.

use std::io;
use std::path::Path;

/// What brings back what [`cgroup2_missing`] and [`terminal_missing`] find
/// gone, said once for both.
const REMOUNT: &str = "Something on this server unmounted it (an agent's recursive unmount \
                       did, in vibe-os-native#169); as root, mount it again, or reboot";

/// Puts the calling process in a new mount namespace whose mounts are
/// slaves of the ones it had, so mounts and unmounts made in it never
/// propagate back. True once it's done; false where the process may not
/// make one (no `CAP_SYS_ADMIN`, or a seccomp filter refuses it), which
/// leaves it unable to mount or unmount anything in the one it's in too.
///
/// Only system calls, so it may run between `fork` and `exec`; the process
/// must have one thread, as a new mount namespace needs a filesystem
/// context of its own. Every platform but Linux is left as it is.
pub fn contain() -> io::Result<bool> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: unshare has no memory-safety preconditions.
        if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
            let e = io::Error::last_os_error();
            return match e.raw_os_error() {
                Some(libc::EPERM) => Ok(false),
                _ => Err(e),
            };
        }
        // Every mount, recursively, receives propagation and sends none.
        // SAFETY: the strings are NUL-terminated and outlive the call; a
        // propagation change takes no source, type or data.
        let rc = unsafe {
            libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_SLAVE,
                std::ptr::null(),
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(true)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(false)
    }
}

/// Why [`contain`] failed, for whoever starts the agent to say.
pub fn contain_error(e: &io::Error) -> String {
    format!(
        "can't give the agent a mount namespace of its own, which keeps its mounts and \
         unmounts from reaching this server's (vibe-os-native#169): {e}"
    )
}

/// Why agents can't start here, if cgroup v2 isn't mounted at `root` any
/// more: systemd makes no unit without it, and `systemd-run` fails with a
/// message that doesn't say so.
pub fn cgroup2_missing(root: &Path) -> Option<String> {
    if root.join("cgroup.controllers").is_file() {
        return None;
    }
    Some(format!(
        "this server's cgroup v2 filesystem isn't mounted at {} any more, so systemd can't \
         make the agent's scope (nor any other unit). {REMOUNT}: `mount -t cgroup2 -o \
         nsdelegate,memory_recursiveprot cgroup2 {}`, then `systemctl daemon-reexec`",
        root.display(),
        root.display()
    ))
}

/// Why agents can't have a terminal here, if opening `ptmx` (`/dev/ptmx`)
/// fails for want of `/dev/pts`.
pub fn terminal_missing(ptmx: &Path) -> Option<String> {
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(ptmx)
    {
        Ok(_) => None,
        Err(e) => terminal_error(ptmx, &e),
    }
}

/// What failing to open `ptmx` with `e` means, if it means `/dev/pts`
/// isn't mounted: `ENODEV` where `/dev/ptmx` is the device, `ENOENT` where
/// it's a link into `/dev/pts`. Anything else is said as it is.
pub fn terminal_error(ptmx: &Path, e: &io::Error) -> Option<String> {
    let gone = matches!(e.raw_os_error(), Some(libc::ENODEV | libc::ENOENT));
    gone.then(|| {
        format!(
            "can't open a terminal for the agent ({}: {e}): /dev/pts isn't mounted on this \
             server. {REMOUNT}: `mount -t devpts -o gid=5,mode=620,ptmxmode=000 devpts \
             /dev/pts`",
            ptmx.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_without_cgroup2_is_told_so() {
        let dir = tempfile::tempdir().unwrap();
        let message = cgroup2_missing(dir.path()).unwrap();
        assert!(message.contains("cgroup v2"), "{message}");
        assert!(message.contains("mount -t cgroup2"), "{message}");
        std::fs::write(dir.path().join("cgroup.controllers"), "memory pids\n").unwrap();
        assert_eq!(cgroup2_missing(dir.path()), None);
    }

    #[test]
    fn a_missing_ptmx_is_said_to_be_dev_pts() {
        let dir = tempfile::tempdir().unwrap();
        let message = terminal_missing(&dir.path().join("ptmx")).unwrap();
        assert!(message.contains("/dev/pts isn't mounted"), "{message}");
        assert!(message.contains("mount -t devpts"), "{message}");
        // The ENODEV the shim met in #169.
        let message = terminal_error(
            Path::new("/dev/ptmx"),
            &io::Error::from_raw_os_error(libc::ENODEV),
        )
        .unwrap();
        assert!(message.contains("/dev/ptmx"), "{message}");
        assert!(message.contains("/dev/pts isn't mounted"), "{message}");
        // Anything else isn't taken for it.
        let other = io::Error::from_raw_os_error(libc::EMFILE);
        assert_eq!(terminal_error(Path::new("/dev/ptmx"), &other), None);
    }

    #[test]
    fn this_machine_has_a_terminal() {
        assert_eq!(terminal_missing(Path::new("/dev/ptmx")), None);
    }
}
