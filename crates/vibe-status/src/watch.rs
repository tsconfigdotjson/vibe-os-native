//! File-system events for transcripts: inotify on Linux, kqueue on macOS.
//! They only say a poll is worth it now; what changed is read by polling,
//! which also runs every second as the backstop.

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::time::Duration;

/// Watches a set of files and directories for writes. Its descriptor is
/// readable when one came, so an event loop can wait on it.
#[derive(Debug)]
pub struct Watcher {
    imp: imp::Watcher,
}

impl Watcher {
    pub fn new() -> io::Result<Watcher> {
        Ok(Watcher {
            imp: imp::Watcher::new()?,
        })
    }

    /// Watches exactly `paths` from now on: new ones are added, and those
    /// left out are dropped. A path that doesn't exist is skipped; give it
    /// again once it does.
    pub fn watch(&mut self, paths: &[PathBuf]) {
        self.imp.watch(paths);
    }

    /// Takes every event that came, without waiting, and says whether there
    /// was any.
    pub fn drain(&mut self) -> bool {
        self.imp.drain()
    }

    /// Waits up to `timeout` for an event, and takes what came.
    pub fn wait(&mut self, timeout: Duration) -> bool {
        let mut fd = libc::pollfd {
            fd: self.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX);
        // SAFETY: one valid pollfd.
        unsafe { libc::poll(&mut fd, 1, ms) };
        self.drain()
    }
}

impl AsRawFd for Watcher {
    fn as_raw_fd(&self) -> RawFd {
        self.imp.as_raw_fd()
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    const MASK: u32 = libc::IN_MODIFY
        | libc::IN_CREATE
        | libc::IN_MOVED_TO
        | libc::IN_MOVED_FROM
        | libc::IN_DELETE
        | libc::IN_CLOSE_WRITE;

    #[derive(Debug)]
    pub(super) struct Watcher {
        fd: OwnedFd,
        watches: HashMap<PathBuf, i32>,
    }

    impl Watcher {
        pub(super) fn new() -> io::Result<Watcher> {
            // SAFETY: no pointers; the result is checked.
            let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Watcher {
                // SAFETY: a new descriptor, owned from here.
                fd: unsafe { OwnedFd::from_raw_fd(fd) },
                watches: HashMap::new(),
            })
        }

        pub(super) fn watch(&mut self, paths: &[PathBuf]) {
            let gone: Vec<PathBuf> = self
                .watches
                .keys()
                .filter(|p| !paths.contains(p))
                .cloned()
                .collect();
            for p in gone {
                if let Some(wd) = self.watches.remove(&p)
                    && !self.watches.values().any(|w| *w == wd)
                {
                    // SAFETY: no pointers. A watch the kernel dropped
                    // already only fails.
                    unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), wd) };
                }
            }
            for p in paths {
                // Added every time: a path made again since is a new inode,
                // and adding an inode already watched only returns its wd.
                let Ok(c) = CString::new(p.as_os_str().as_bytes()) else {
                    continue;
                };
                // SAFETY: a valid C string for the call's duration.
                let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), c.as_ptr(), MASK) };
                if wd >= 0 {
                    self.watches.insert(p.clone(), wd);
                } else {
                    self.watches.remove(p);
                }
            }
        }

        pub(super) fn drain(&mut self) -> bool {
            let mut any = false;
            let mut buf = [0u8; 4096];
            loop {
                // SAFETY: the buffer is valid for its length.
                let n =
                    unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 {
                    return any;
                }
                any = true;
            }
        }

        pub(super) fn as_raw_fd(&self) -> RawFd {
            self.fd.as_raw_fd()
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;

    const NOTES: u32 = libc::NOTE_WRITE
        | libc::NOTE_EXTEND
        | libc::NOTE_DELETE
        | libc::NOTE_RENAME
        | libc::NOTE_ATTRIB;

    #[derive(Debug)]
    pub(super) struct Watcher {
        kq: OwnedFd,
        /// Each path's descriptor, and the inode it's on.
        files: HashMap<PathBuf, (OwnedFd, (u64, u64))>,
    }

    impl Watcher {
        pub(super) fn new() -> io::Result<Watcher> {
            // SAFETY: no arguments; the result is checked.
            let kq = unsafe { libc::kqueue() };
            if kq < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a new descriptor, owned from here.
            let kq = unsafe { OwnedFd::from_raw_fd(kq) };
            // SAFETY: no pointers.
            unsafe { libc::fcntl(kq.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
            Ok(Watcher {
                kq,
                files: HashMap::new(),
            })
        }

        pub(super) fn watch(&mut self, paths: &[PathBuf]) {
            // A descriptor's events go when it's closed.
            self.files.retain(|p, _| paths.contains(p));
            for p in paths {
                let Ok(meta) = std::fs::metadata(p) else {
                    self.files.remove(p);
                    continue;
                };
                let inode = (meta.dev(), meta.ino());
                if self.files.get(p).is_some_and(|(_, i)| *i == inode) {
                    continue;
                }
                let Ok(c) = CString::new(p.as_os_str().as_bytes()) else {
                    continue;
                };
                // SAFETY: a valid C string for the call's duration.
                let fd = unsafe { libc::open(c.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
                if fd < 0 {
                    self.files.remove(p);
                    continue;
                }
                // SAFETY: a new descriptor, owned from here.
                let fd = unsafe { OwnedFd::from_raw_fd(fd) };
                let change = libc::kevent {
                    ident: fd.as_raw_fd() as libc::uintptr_t,
                    filter: libc::EVFILT_VNODE,
                    flags: libc::EV_ADD | libc::EV_CLEAR,
                    fflags: NOTES,
                    data: 0,
                    udata: std::ptr::null_mut(),
                };
                // SAFETY: one valid change, no events out.
                let rc = unsafe {
                    libc::kevent(
                        self.kq.as_raw_fd(),
                        &change,
                        1,
                        std::ptr::null_mut(),
                        0,
                        std::ptr::null(),
                    )
                };
                if rc < 0 {
                    self.files.remove(p);
                    continue;
                }
                self.files.insert(p.clone(), (fd, inode));
            }
        }

        pub(super) fn drain(&mut self) -> bool {
            let mut any = false;
            let zero = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            loop {
                // SAFETY: kevent is plain data; all zeroes is a valid value.
                let mut events: [libc::kevent; 32] = unsafe { std::mem::zeroed() };
                // SAFETY: the events buffer is valid for its length.
                let n = unsafe {
                    libc::kevent(
                        self.kq.as_raw_fd(),
                        std::ptr::null(),
                        0,
                        events.as_mut_ptr(),
                        events.len() as libc::c_int,
                        &zero,
                    )
                };
                if n <= 0 {
                    return any;
                }
                any = true;
                if (n as usize) < events.len() {
                    return any;
                }
            }
        }

        pub(super) fn as_raw_fd(&self) -> RawFd {
            self.kq.as_raw_fd()
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    use std::io;
    use std::os::fd::RawFd;
    use std::path::PathBuf;

    /// No events here: the poll alone finds what changed.
    #[derive(Debug)]
    pub(super) struct Watcher;

    impl Watcher {
        pub(super) fn new() -> io::Result<Watcher> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "no file-system events on this OS",
            ))
        }

        pub(super) fn watch(&mut self, _: &[PathBuf]) {}

        pub(super) fn drain(&mut self) -> bool {
            false
        }

        pub(super) fn as_raw_fd(&self) -> RawFd {
            -1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    #[test]
    fn a_write_to_a_watched_file_or_directory_is_an_event() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("t.jsonl");
        fs::write(&file, "a\n").unwrap();
        let mut w = Watcher::new().unwrap();
        w.watch(&[dir.path().to_owned(), file.clone()]);
        assert!(!w.wait(Duration::from_millis(50)), "nothing yet");
        fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap()
            .write_all(b"b\n")
            .unwrap();
        assert!(w.wait(Duration::from_secs(5)), "an append");
        assert!(!w.drain(), "taken");
        fs::write(dir.path().join("new.jsonl"), "c\n").unwrap();
        assert!(
            w.wait(Duration::from_secs(5)),
            "a new file in the directory"
        );
        w.drain();
        // Unwatched, a write says nothing; dropping the watches may.
        w.watch(&[]);
        w.wait(Duration::from_millis(50));
        fs::write(&file, "d\n").unwrap();
        assert!(!w.wait(Duration::from_millis(100)));
        // A path that isn't there is skipped, and can be watched later.
        let later = dir.path().join("later");
        w.watch(std::slice::from_ref(&later));
        fs::create_dir(&later).unwrap();
        w.watch(std::slice::from_ref(&later));
        fs::write(later.join("x"), "e\n").unwrap();
        assert!(w.wait(Duration::from_secs(5)));
    }
}
