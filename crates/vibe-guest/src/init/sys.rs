//! Thin wrappers over the system calls and `/proc` files the guest uses.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;

use vibe_guest::proto::Memory;

pub(crate) fn check(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

pub(crate) fn cstr(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL byte"))
}

/// mount(2). `data` is the filesystem's options.
pub(crate) fn mount(
    source: &str,
    target: &Path,
    fstype: &str,
    flags: libc::c_ulong,
    data: &str,
) -> io::Result<()> {
    let source = CString::new(source)?;
    let target = cstr(target)?;
    let fstype = CString::new(fstype)?;
    let data = CString::new(data)?;
    // SAFETY: every pointer is a NUL-terminated string that outlives the
    // call.
    check(unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            flags,
            data.as_ptr().cast(),
        )
    })
    .map(drop)
}

/// Moves the mount at `from` to `to`.
pub(crate) fn move_mount(from: &Path, to: &Path) -> io::Result<()> {
    let from = cstr(from)?;
    let to = cstr(to)?;
    // SAFETY: as for mount.
    check(unsafe {
        libc::mount(
            from.as_ptr(),
            to.as_ptr(),
            std::ptr::null(),
            libc::MS_MOVE,
            std::ptr::null(),
        )
    })
    .map(drop)
}

/// Whether something is mounted at `path`, from `/proc/self/mountinfo`.
pub(crate) fn mounted(path: &Path) -> bool {
    let Ok(info) = fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    let want = path.to_string_lossy();
    info.lines()
        .filter_map(|l| l.split(' ').nth(4))
        .any(|point| unescape_mountinfo(point) == want)
}

/// mountinfo writes a space, tab, newline or backslash in a path as an
/// octal escape.
fn unescape_mountinfo(s: &str) -> String {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    let octal = |d: &[u8]| -> Option<u8> {
        let mut v: u32 = 0;
        for &c in d {
            if !(b'0'..=b'7').contains(&c) {
                return None;
            }
            v = v * 8 + u32::from(c - b'0');
        }
        u8::try_from(v).ok()
    };
    while i < b.len() {
        if b[i] == b'\\'
            && let Some(v) = b.get(i + 1..i + 4).and_then(octal)
        {
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `mkdir -p` with `mode` for what it makes.
pub(crate) fn mkdir_p(path: &Path, mode: u32) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(path)
}

/// `mkdir -p`, giving every directory it makes to `uid`:`gid`.
pub(crate) fn mkdir_p_owned(path: &Path, mode: u32, uid: u32, gid: u32) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut at = Some(path);
    while let Some(p) = at {
        if p.exists() {
            break;
        }
        missing.push(p);
        at = p.parent();
    }
    for dir in missing.into_iter().rev() {
        match fs::DirBuilder::new().mode(mode).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
        chown(dir, uid, gid)?;
    }
    Ok(())
}

pub(crate) fn chown(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    std::os::unix::fs::chown(path, Some(uid), Some(gid))
}

pub(crate) fn chmod(path: &Path, mode: u32) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

/// The guest's uptime. `CLOCK_BOOTTIME`, like `/proc/uptime` and process
/// start times; it stops while the VM sleeps.
pub(crate) fn uptime_ms() -> u64 {
    clock_ms(libc::CLOCK_BOOTTIME).max(0) as u64
}

fn clock_ms(clock: libc::clockid_t) -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is a valid timespec for the call to fill.
    unsafe { libc::clock_gettime(clock, &mut ts) };
    ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000
}

/// Sets the wall clock to `time_ms` after the Unix epoch; how far it moved.
pub(crate) fn set_clock(time_ms: u64) -> io::Result<i64> {
    let before = clock_ms(libc::CLOCK_REALTIME);
    let ts = libc::timespec {
        tv_sec: (time_ms / 1000) as _,
        tv_nsec: ((time_ms % 1000) * 1_000_000) as _,
    };
    // SAFETY: ts is a valid timespec.
    check(unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) })?;
    Ok(time_ms as i64 - before)
}

pub(crate) fn memory() -> Memory {
    let text = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
            .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    Memory {
        total: field("MemTotal"),
        available: field("MemAvailable"),
    }
}

pub(crate) fn boot_id() -> String {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

/// The clock ticks and page size `/proc` counts in.
pub(crate) fn clock_ticks() -> u64 {
    // SAFETY: sysconf has no memory effects.
    let n = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if n > 0 { n as u64 } else { 100 }
}

pub(crate) fn page_size() -> u64 {
    // SAFETY: as above.
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if n > 0 { n as u64 } else { 4096 }
}

/// A signal's name, as in `SIGKILL`: what the host is told, since numbers
/// differ between Linux and macOS.
pub(crate) fn signal_name(signal: i32) -> String {
    let name = match signal {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGXCPU => "SIGXCPU",
        libc::SIGXFSZ => "SIGXFSZ",
        libc::SIGSYS => "SIGSYS",
        _ => return format!("signal {signal}"),
    };
    name.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mountinfo_escapes() {
        assert_eq!(unescape_mountinfo(r"/a\040b"), "/a b");
        assert_eq!(unescape_mountinfo(r"/a\134b\011"), "/a\\b\t");
        assert_eq!(unescape_mountinfo(r"/plain\0"), r"/plain\0");
        assert!(mounted(Path::new("/proc")));
        assert!(!mounted(Path::new("/proc/self")));
    }

    #[test]
    fn proc_readers() {
        let m = memory();
        assert!(
            m.total > 0 && m.available > 0 && m.available <= m.total,
            "{m:?}"
        );
        assert_eq!(boot_id().len(), 36);
        assert!(uptime_ms() > 0);
        assert_eq!(signal_name(libc::SIGKILL), "SIGKILL");
        assert_eq!(signal_name(64), "signal 64");
    }

    #[test]
    fn owned_directories() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a/b/c");
        // SAFETY: getuid and getgid have no memory effects.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        mkdir_p_owned(&deep, 0o750, uid, gid).unwrap();
        mkdir_p_owned(&deep, 0o750, uid, gid).unwrap();
        let mode = fs::metadata(dir.path().join("a/b"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o750);
    }
}
