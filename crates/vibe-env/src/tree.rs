//! A native agent's process tree, by session ID (PRD: How macos is
//! governed, Membership).
//!
//! A shim leads a session of its own, and everything the agent starts
//! inherits that session ID, so the session is the agent's tree, jobs that
//! were reparented to init or launchd (`nohup … &`) included. A process that
//! starts a session of its own escapes, and that's accepted. Environment
//! tags aren't used: on macOS 26, `KERN_PROCARGS2` hides the environment of
//! Apple's own binaries.
//!
//! [`SessionTree::freeze`] is the one way a native tree is frozen: the
//! `macos` sleeper and governor and the test backend all use it. The shim,
//! the session's leader, is never frozen, because it must keep answering.

use std::collections::HashSet;
use std::io;
use std::time::{Duration, Instant};

use crate::Proc;

/// The kernel's name for a shim, which leads each native agent's session.
pub const SHIM_NAME: &str = "vibe-shim";

/// The processes of one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionTree {
    /// The session ID: its leader's pid, which is the shim's.
    pub sid: u32,
}

impl SessionTree {
    pub fn new(sid: u32) -> SessionTree {
        SessionTree { sid }
    }

    /// Every live process in the session, its leader included. Zombies
    /// are left out: they've ended.
    pub fn all(&self) -> Vec<Proc> {
        os::session(self.sid)
    }

    /// Every live process in the session but its leader.
    pub fn members(&self) -> Vec<Proc> {
        let mut procs = self.all();
        procs.retain(|p| p.pid != self.sid);
        procs
    }

    /// Whether the session is still the one a shim started: its leader is a
    /// `vibe-shim`, or it has no leader left and only its stragglers remain.
    /// A pid stays unused while it's still some process's session ID, so
    /// one that names a session led by anything else was reused after the
    /// agent's tree had gone entirely, as after a reboot; that session
    /// isn't the agent's.
    pub fn is_shims(&self) -> bool {
        self.all()
            .iter()
            .find(|p| p.pid == self.sid)
            .is_none_or(|leader| leader.name == SHIM_NAME)
    }

    /// Whether anything at all is left in the session.
    pub fn is_empty(&self) -> bool {
        self.all().is_empty()
    }

    /// Stops every member with `SIGSTOP` and returns how many there were.
    /// A process can fork between being listed and being stopped, so the
    /// session is listed again until a pass finds nothing new.
    pub fn freeze(&self) -> io::Result<usize> {
        let mut stopped = HashSet::new();
        for _ in 0..100 {
            let new: Vec<u32> = self
                .members()
                .into_iter()
                .map(|p| p.pid)
                .filter(|pid| !stopped.contains(pid))
                .collect();
            if new.is_empty() {
                return Ok(stopped.len());
            }
            for pid in new {
                signal(pid, libc::SIGSTOP);
                stopped.insert(pid);
            }
        }
        Err(io::Error::other(
            "the agent's processes kept forking while it was being frozen",
        ))
    }

    /// Continues every member with `SIGCONT`.
    pub fn thaw(&self) {
        for p in self.members() {
            signal(p.pid, libc::SIGCONT);
        }
    }

    /// Ends the whole session: `SIGTERM` to the members (continued, so a
    /// frozen one gets it), up to `grace` for them to go, then `SIGKILL`
    /// to everything left, the leader included. Returns whether the session
    /// emptied.
    pub fn end(&self, grace: Duration) -> bool {
        for p in self.members() {
            signal(p.pid, libc::SIGTERM);
            signal(p.pid, libc::SIGCONT);
        }
        if self.wait_empty(grace) {
            return true;
        }
        for p in self.all() {
            signal(p.pid, libc::SIGKILL);
        }
        self.wait_empty(Duration::from_secs(5))
    }

    /// Waits up to `timeout` for the session to empty.
    pub fn wait_empty(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.is_empty() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn signal(pid: u32, sig: i32) {
    if let Ok(pid) = i32::try_from(pid)
        && pid > 1
    {
        // SAFETY: kill has no memory-safety preconditions; a process that
        // has gone just makes it fail with ESRCH.
        unsafe { libc::kill(pid, sig) };
    }
}

#[cfg(target_os = "linux")]
mod os {
    use std::fs;

    use crate::Proc;

    /// Every live process of session `sid`, from `/proc`.
    pub(super) fn session(sid: u32) -> Vec<Proc> {
        let Ok(entries) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        let clock = Clock::now();
        entries
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
            .filter_map(|pid| read(pid, sid, &clock))
            .map(|(_, p)| p)
            .collect()
    }

    struct Clock {
        boot_ms: u64,
        ticks: u64,
        page: u64,
    }

    impl Clock {
        fn now() -> Clock {
            let boot_s = fs::read_to_string("/proc/stat")
                .ok()
                .and_then(|s| {
                    s.lines()
                        .find_map(|l| l.strip_prefix("btime ")?.trim().parse::<u64>().ok())
                })
                .unwrap_or(0);
            // SAFETY: sysconf has no preconditions.
            let (ticks, page) = unsafe {
                (
                    libc::sysconf(libc::_SC_CLK_TCK),
                    libc::sysconf(libc::_SC_PAGESIZE),
                )
            };
            Clock {
                boot_ms: boot_s * 1000,
                ticks: u64::try_from(ticks).ok().filter(|t| *t > 0).unwrap_or(100),
                page: u64::try_from(page).ok().filter(|p| *p > 0).unwrap_or(4096),
            }
        }
    }

    /// A process's session and what it is, unless it has gone or is a
    /// zombie.
    fn read(pid: u32, sid: u32, clock: &Clock) -> Option<(u32, Proc)> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (s, p) = parse_stat(pid, &stat, clock)?;
        if s != sid {
            return None;
        }
        let args = fs::read(format!("/proc/{pid}/cmdline"))
            .map(|b| {
                b.split(|c| *c == 0)
                    .filter(|a| !a.is_empty())
                    .map(|a| String::from_utf8_lossy(a).into_owned())
                    .collect()
            })
            .unwrap_or_default();
        Some((s, Proc { args, ..p }))
    }

    /// `/proc/<pid>/stat`: `pid (comm) state ppid pgrp session …`. The
    /// name may hold spaces and parentheses, so it ends at the last `)`.
    fn parse_stat(pid: u32, stat: &str, clock: &Clock) -> Option<(u32, Proc)> {
        let open = stat.find('(')?;
        let close = stat.rfind(')')?;
        let name = stat.get(open + 1..close)?.to_owned();
        let f: Vec<&str> = stat.get(close + 1..)?.split_whitespace().collect();
        let num = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok());
        let state = *f.first()?;
        if state == "Z" || state == "X" {
            return None;
        }
        let session = u32::try_from(num(3)?).ok()?;
        let cpu_ticks = num(11)? + num(12)?;
        Some((
            session,
            Proc {
                pid,
                ppid: u32::try_from(num(1)?).ok()?,
                name,
                args: Vec::new(),
                stopped: state == "T" || state == "t",
                started_ms: clock.boot_ms + num(19)? * 1000 / clock.ticks,
                cpu_ms: cpu_ticks * 1000 / clock.ticks,
                memory: num(21)? * clock.page,
            },
        ))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn stat_lines() {
            let clock = Clock {
                boot_ms: 1_000_000,
                ticks: 100,
                page: 4096,
            };
            let line = "42 (we(ird) name) T 7 42 40 34816 42 4194560 100 0 0 0 150 50 0 0 \
                        20 0 1 0 250 1000 3 18446744073709551615";
            let (sid, p) = parse_stat(42, line, &clock).unwrap();
            assert_eq!(sid, 40);
            assert_eq!(p.name, "we(ird) name");
            assert_eq!(p.ppid, 7);
            assert!(p.stopped);
            assert_eq!(p.cpu_ms, 2000);
            assert_eq!(p.started_ms, 1_002_500);
            assert_eq!(p.memory, 3 * 4096);
            let zombie = line.replace(") T ", ") Z ");
            assert!(parse_stat(42, &zombie, &clock).is_none());
            assert!(parse_stat(42, "42 (x", &clock).is_none());
        }
    }
}

#[cfg(target_os = "macos")]
mod os {
    use std::ffi::CStr;
    use std::mem::{MaybeUninit, size_of};

    use crate::Proc;

    /// Every live process of session `sid`, from libproc. Command lines
    /// aren't read: `KERN_PROCARGS2` hides Apple's binaries' anyway.
    pub(super) fn session(sid: u32) -> Vec<Proc> {
        let Ok(sid) = i32::try_from(sid) else {
            return Vec::new();
        };
        let timebase = timebase();
        all_pids()
            .into_iter()
            // SAFETY: getsid has no memory-safety preconditions.
            .filter(|&pid| pid > 0 && unsafe { libc::getsid(pid) } == sid)
            .filter_map(|pid| read(pid, timebase))
            .collect()
    }

    fn all_pids() -> Vec<i32> {
        // SAFETY: with a null buffer, proc_listallpids only counts.
        let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        let Ok(n) = usize::try_from(n) else {
            return Vec::new();
        };
        // Room for some that start meanwhile.
        let mut pids = vec![0i32; n + 64];
        let bytes = i32::try_from(pids.len() * size_of::<i32>()).unwrap_or(i32::MAX);
        // SAFETY: the buffer holds `bytes` bytes.
        let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        pids.truncate(usize::try_from(got).unwrap_or(0).min(pids.len()));
        pids
    }

    /// Mach time units per nanosecond, as (numer, denom).
    #[allow(deprecated)]
    fn timebase() -> (u64, u64) {
        let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: it fills in the struct it's given.
        let ok = unsafe { libc::mach_timebase_info(&mut info) } == 0;
        if ok && info.denom != 0 {
            (u64::from(info.numer), u64::from(info.denom))
        } else {
            (1, 1)
        }
    }

    fn read(pid: i32, (numer, denom): (u64, u64)) -> Option<Proc> {
        let mut bsd = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = i32::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
        // SAFETY: the buffer is a proc_bsdinfo of `size` bytes.
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                bsd.as_mut_ptr().cast(),
                size,
            )
        };
        if got != size {
            return None;
        }
        // SAFETY: proc_pidinfo filled it, and it was zeroed before.
        let bsd = unsafe { bsd.assume_init() };
        // SZOMB
        if bsd.pbi_status == 5 {
            return None;
        }
        let mut usage = MaybeUninit::<libc::rusage_info_v2>::zeroed();
        // SAFETY: the buffer is a rusage_info_v2, the flavor asked for.
        let rc = unsafe {
            libc::proc_pid_rusage(
                pid,
                libc::RUSAGE_INFO_V2,
                usage.as_mut_ptr().cast::<libc::rusage_info_t>(),
            )
        };
        let (cpu_ms, memory) = if rc == 0 {
            // SAFETY: proc_pid_rusage filled it.
            let u = unsafe { usage.assume_init() };
            let ticks = u.ri_user_time + u.ri_system_time;
            (ticks * numer / denom / 1_000_000, u.ri_phys_footprint)
        } else {
            (0, 0)
        };
        // SAFETY: pbi_comm is NUL-terminated within its length.
        let name = unsafe { CStr::from_ptr(bsd.pbi_comm.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        Some(Proc {
            pid: u32::try_from(pid).ok()?,
            ppid: bsd.pbi_ppid,
            name,
            args: Vec::new(),
            stopped: bsd.pbi_status == libc::SSTOP,
            started_ms: bsd.pbi_start_tvsec * 1000 + bsd.pbi_start_tvusec / 1000,
            cpu_ms,
            memory,
        })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod os {
    use crate::Proc;

    pub(super) fn session(_sid: u32) -> Vec<Proc> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    /// A shell leading a session of its own, which starts `children`
    /// sleeps, one of them `nohup`ed into the background so it outlives
    /// its parent's view. Returns the shell, whose pid is the session ID.
    fn session_with(children: usize) -> std::process::Child {
        let script = format!(
            "i=0; while [ $i -lt {children} ]; do sleep 30 & i=$((i+1)); done; \
             nohup sleep 30 >/dev/null 2>&1 & echo ready; wait"
        );
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // SAFETY: setsid is async-signal-safe, as pre_exec requires.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().unwrap();
        let mut out = child.stdout.take().unwrap();
        let mut buf = [0u8; 6];
        std::io::Read::read_exact(&mut out, &mut buf).unwrap();
        child
    }

    fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ok() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_session_freezes_thaws_and_ends_whole() {
        let mut leader = session_with(3);
        let tree = SessionTree::new(leader.id());
        // Three sleeps and the nohup one; the leader isn't a member.
        wait_for("four members", || tree.members().len() == 4);
        assert!(tree.all().iter().any(|p| p.pid == tree.sid));
        let me = std::process::id();
        assert!(tree.all().iter().all(|p| p.pid != me), "not this process");
        for p in tree.members() {
            assert_eq!(p.ppid, tree.sid, "{p:?}");
            assert!(p.started_ms > 0 && !p.stopped, "{p:?}");
        }

        assert_eq!(tree.freeze().unwrap(), 4);
        wait_for("every member stopped", || {
            tree.members().iter().all(|p| p.stopped)
        });
        // The leader keeps running.
        assert!(
            !tree
                .all()
                .iter()
                .find(|p| p.pid == tree.sid)
                .unwrap()
                .stopped
        );
        tree.thaw();
        wait_for("every member running", || {
            tree.members().iter().all(|p| !p.stopped)
        });

        // Frozen again, it still ends: the leader's shell and its jobs.
        tree.freeze().unwrap();
        assert!(tree.end(Duration::from_secs(5)));
        assert!(tree.is_empty());
        assert!(tree.members().is_empty());
        let _ = leader.wait();
    }

    #[test]
    fn a_session_led_by_something_else_isnt_a_shims() {
        // As if the pid of a gone shim now led someone's shell.
        let mut leader = session_with(1);
        let tree = SessionTree::new(leader.id());
        assert!(!tree.is_shims());
        tree.end(Duration::from_secs(5));
        let _ = leader.wait();
        // With no leader at all, what's left would be its stragglers.
        assert!(SessionTree::new(u32::MAX - 1).is_shims());
    }

    #[test]
    fn an_unknown_session_is_empty() {
        let tree = SessionTree::new(u32::MAX - 1);
        assert!(tree.is_empty());
        assert_eq!(tree.freeze().unwrap(), 0);
        tree.thaw();
        assert!(tree.end(Duration::from_millis(10)));
    }
}
