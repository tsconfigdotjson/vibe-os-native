//! inotify on Linux: which directories under an agent's trees changed, for
//! the [cache](super::cache) to list again.
//!
//! inotify watches a directory, not a tree, so there's a watch for each
//! directory the cache holds, made as the cache takes it up and let go of
//! as the cache does. A watch is made just before its directory is listed
//! ([`Watcher::watch`]), so whatever changes in it after the listing is
//! reported, and whatever changed before is in the listing. A directory
//! made later is reported by its parent's watch, and its parent listed
//! again takes it up, watching it first in turn.
//!
//! What's reported is a directory whose entries changed (made, removed,
//! moved in or out), or one of whose files was written, truncated or given
//! another link (`IN_ATTRIB`: a link made to it from elsewhere changes its
//! count of links). A watch is on an inode, so a directory moved within the
//! trees keeps its watch, under the new name, and one moved out of them is
//! let go of. A full queue (`IN_Q_OVERFLOW`) or an unmount marks
//! everything. What inotify never reports is a write through `mmap` into a
//! hole in a file; the reconcile walk finds that.
//!
//! Each agent's trees have an instance of their own, opened when its cache
//! is built and closed when it's let go of (the agent is frozen, say),
//! which lets go of every watch at once. Watches are kernel memory, about a
//! kilobyte each with the inode they keep cached, and every process of the
//! user draws on the same `max_user_watches`: this process holds at most
//! half of that across every agent. A tree that would take more, or an
//! instance that can't be opened, is walked instead.

use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::Changes;
use super::cache::{Cache, Dirty, Watcher};

/// What a watch reports.
const MASK: u32 = libc::IN_MODIFY
    | libc::IN_ATTRIB
    | libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_ONLYDIR
    | libc::IN_DONT_FOLLOW
    | libc::IN_EXCL_UNLINK;

/// How much is read from an instance at a time.
const READ: usize = 64 << 10;

/// The most reads a look makes, about fifty thousand events: whatever a
/// writer makes faster than that waits for the next look, so a look always
/// ends.
const READS: usize = 16;

/// Watches this process holds, across every instance.
static HELD: AtomicUsize = AtomicUsize::new(0);

/// Watches this process holds now, across every agent.
pub fn held() -> usize {
    HELD.load(Ordering::SeqCst)
}

/// The most watches this process holds: half the user's
/// `max_user_watches`, as other programs need some too.
pub fn budget() -> usize {
    static BUDGET: OnceLock<usize> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .map_or(8192, |n| n / 2)
    })
}

/// An instance's watches, a directory of one agent's cache each.
#[derive(Debug)]
pub struct Inotify {
    fd: Option<OwnedFd>,
    /// Each watch, and the directory in the cache it's for.
    watches: HashMap<i32, u32>,
    /// What's read from the instance, while it's open.
    buf: Vec<u8>,
    /// The path to watch, NUL-terminated.
    path: Vec<u8>,
    /// Directories already marked by this take.
    seen: HashSet<u32>,
    /// The most watches the process may hold before this stops making
    /// more.
    budget: usize,
    failed: bool,
}

impl Default for Inotify {
    fn default() -> Inotify {
        Inotify::new()
    }
}

impl Inotify {
    /// Opens nothing until a cache is built with it.
    pub fn new() -> Inotify {
        Inotify::with_budget(budget())
    }

    /// One that fails rather than take the process past `budget` watches.
    pub fn with_budget(budget: usize) -> Inotify {
        Inotify {
            fd: None,
            watches: HashMap::new(),
            buf: Vec::new(),
            path: Vec::new(),
            seen: HashSet::new(),
            budget,
            failed: false,
        }
    }

    /// How many watches it holds.
    pub fn watching(&self) -> usize {
        self.watches.len()
    }

    /// Whether its instance is open.
    pub fn open(&self) -> bool {
        self.fd.is_some()
    }

    /// The watches the kernel says its instance holds (from
    /// `/proc/self/fdinfo`), to check none is left over.
    pub fn kernel_watches(&self) -> Option<usize> {
        let fd = self.fd.as_ref()?.as_raw_fd();
        let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).ok()?;
        Some(
            info.lines()
                .filter(|l| l.starts_with("inotify wd:"))
                .count(),
        )
    }

    /// Closes its instance, if it's open, and every watch with it.
    fn close(&mut self) {
        if self.fd.take().is_some() {
            HELD.fetch_sub(self.watches.len(), Ordering::SeqCst);
        }
        self.watches = HashMap::new();
        self.buf = Vec::new();
        self.path = Vec::new();
        self.seen = HashSet::new();
    }

    /// Opens a new instance in place of the one it had.
    fn reopen(&mut self) {
        self.close();
        self.failed = false;
        // SAFETY: plain flags; the descriptor returned is ours alone.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            eprintln!(
                "vibe-env: can't open inotify: {}",
                std::io::Error::last_os_error()
            );
            self.failed = true;
            return;
        }
        // SAFETY: a descriptor just opened, owned here from now on.
        self.fd = Some(unsafe { OwnedFd::from_raw_fd(fd) });
        self.buf = vec![0; READ];
    }
}

impl Drop for Inotify {
    fn drop(&mut self) {
        self.close();
    }
}

impl Watcher for Inotify {
    fn watch(&mut self, path: &Path) -> i32 {
        let Some(fd) = &self.fd else {
            return 0;
        };
        if self.failed {
            return 0;
        }
        if HELD.load(Ordering::SeqCst) >= self.budget {
            self.failed = true;
            return 0;
        }
        self.path.clear();
        self.path.extend_from_slice(path.as_os_str().as_bytes());
        self.path.push(0);
        // SAFETY: an open instance, and a NUL-terminated path.
        let wd =
            unsafe { libc::inotify_add_watch(fd.as_raw_fd(), self.path.as_ptr().cast(), MASK) };
        if wd > 0 {
            return wd;
        }
        // Out of watches, or of kernel memory for them, it fails. Otherwise
        // the directory is gone or not one now, as its parent's watch will
        // say, or it can't be read, so it can't be listed either.
        let errno = std::io::Error::last_os_error().raw_os_error();
        if let Some(libc::ENOSPC | libc::ENOMEM) = errno {
            self.failed = true;
        }
        0
    }

    fn added(&mut self, tag: i32, node: u32) {
        // A directory moved within the trees has the watch it had, as a
        // watch is on an inode: it's this directory's now.
        if self.watches.insert(tag, node).is_none() {
            HELD.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn removed(&mut self, tag: i32, node: u32) {
        // Only if it's still this directory's, and not the same directory's
        // under the name it was moved to.
        if self.watches.get(&tag) != Some(&node) {
            return;
        }
        self.watches.remove(&tag);
        HELD.fetch_sub(1, Ordering::SeqCst);
        if let Some(fd) = &self.fd {
            // SAFETY: an open instance; a watch the kernel let go of
            // already (its directory removed) is refused, harmlessly.
            unsafe { libc::inotify_rm_watch(fd.as_raw_fd(), tag) };
        }
    }

    fn cleared(&mut self) {
        // A new instance: every watch of the old one goes at once.
        self.reopen();
    }
}

impl Changes for Inotify {
    fn take(&mut self, cache: Option<&Cache>, dirty: &mut Dirty) -> bool {
        let Some(fd) = &self.fd else {
            return false;
        };
        let fd = fd.as_raw_fd();
        self.seen.clear();
        // Once everything's marked, the cache is built afresh with a new
        // instance, and what's queued in this one doesn't matter.
        for _ in 0..READS {
            if dirty.all {
                break;
            }
            // SAFETY: an open instance, read into a buffer of that length.
            let n = unsafe { libc::read(fd, self.buf.as_mut_ptr().cast(), self.buf.len()) };
            let Ok(n) = usize::try_from(n) else {
                // Nothing more to read now (EAGAIN), or interrupted: the
                // rest is read next time.
                break;
            };
            if n == 0 {
                break;
            }
            for (wd, mask) in events(&self.buf[..n]) {
                note(&mut self.watches, &mut self.seen, wd, mask, cache, dirty);
            }
        }
        true
    }

    fn watcher(&mut self) -> &mut dyn Watcher {
        self
    }

    fn failed(&self) -> bool {
        self.failed
    }

    fn release(&mut self) {
        self.close();
        self.failed = false;
    }
}

/// Each event in `buf`, as read from an instance: its watch and what
/// happened.
fn events(buf: &[u8]) -> impl Iterator<Item = (i32, u32)> + '_ {
    const HEAD: usize = std::mem::size_of::<libc::inotify_event>();
    let mut at = 0;
    std::iter::from_fn(move || {
        let head = buf.get(at..at + HEAD)?;
        let word = |i: usize| [head[i], head[i + 1], head[i + 2], head[i + 3]];
        let wd = i32::from_ne_bytes(word(0));
        let mask = u32::from_ne_bytes(word(4));
        let len = u32::from_ne_bytes(word(12)) as usize;
        at += HEAD + len;
        Some((wd, mask))
    })
}

/// Marks in `dirty` what one event says changed.
fn note(
    watches: &mut HashMap<i32, u32>,
    seen: &mut HashSet<u32>,
    wd: i32,
    mask: u32,
    cache: Option<&Cache>,
    dirty: &mut Dirty,
) {
    if mask & (libc::IN_Q_OVERFLOW | libc::IN_UNMOUNT) != 0 {
        // Events were lost, or the filesystem went.
        dirty.everything();
        return;
    }
    let node = if mask & libc::IN_IGNORED != 0 {
        // The kernel let go of the watch, its directory gone; one let go of
        // here isn't in the map any more.
        let Some(node) = watches.remove(&wd) else {
            return;
        };
        HELD.fetch_sub(1, Ordering::SeqCst);
        node
    } else {
        let Some(&node) = watches.get(&wd) else {
            // A watch let go of since, with its event still queued.
            return;
        };
        node
    };
    if dirty.all || !seen.insert(node) {
        return;
    }
    match cache.and_then(|c| c.path(node)) {
        Some(path) => dirty.mark(&path, false),
        None => dirty.everything(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::walk::{Walker, usage};
    use std::fs;
    use std::path::PathBuf;

    /// Brings `cache` up to date with what `ino` reports.
    fn catch_up(cache: &mut Cache, ino: &mut Inotify, w: &mut Walker) {
        let mut d = Dirty::default();
        assert!(ino.take(Some(cache), &mut d));
        cache.update_watched(&d, w, ino);
    }

    /// Every directory it holds is watched, and the kernel holds no other
    /// watch for it.
    fn watches_match(cache: &Cache, ino: &Inotify, what: &str) {
        assert_eq!(ino.watching(), cache.dirs(), "watches after {what}");
        assert_eq!(
            ino.kernel_watches(),
            Some(cache.dirs()),
            "the kernel's watches after {what}"
        );
    }

    #[test]
    fn events_are_read_whole_names_and_all() {
        let mut buf = Vec::new();
        let mut push = |wd: i32, mask: u32, name: &[u8]| {
            let len = name.len().div_ceil(16) * 16;
            buf.extend_from_slice(&wd.to_ne_bytes());
            buf.extend_from_slice(&mask.to_ne_bytes());
            buf.extend_from_slice(&0u32.to_ne_bytes());
            buf.extend_from_slice(&(len as u32).to_ne_bytes());
            let mut n = name.to_vec();
            n.resize(len, 0);
            buf.extend_from_slice(&n);
        };
        push(3, libc::IN_CREATE, b"some-file-with-a-long-name");
        push(-1, libc::IN_Q_OVERFLOW, b"");
        push(7, libc::IN_IGNORED, b"");
        let seen: Vec<(i32, u32)> = events(&buf).collect();
        assert_eq!(
            seen,
            [
                (3, libc::IN_CREATE),
                (-1, libc::IN_Q_OVERFLOW),
                (7, libc::IN_IGNORED)
            ]
        );
        // A full queue is everything; an event for a watch let go of is
        // nothing.
        let (mut watches, mut seen) = (HashMap::new(), HashSet::new());
        let mut d = Dirty::default();
        note(&mut watches, &mut seen, 9, libc::IN_CREATE, None, &mut d);
        note(&mut watches, &mut seen, 9, libc::IN_IGNORED, None, &mut d);
        assert!(d.is_empty());
        note(
            &mut watches,
            &mut seen,
            -1,
            libc::IN_Q_OVERFLOW,
            None,
            &mut d,
        );
        assert!(d.all);
    }

    /// A small deterministic generator, so a failure can be run again.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Changes of every kind, reported by nothing but inotify itself: the
    /// cache stays what a walk finds, and holds a watch for each directory
    /// and no other.
    #[test]
    fn what_inotify_reports_keeps_the_cache_exact() {
        for seed in [0x9e37_79b9_7f4a_7c15u64, 0x2545_f491_4f6c_dd1d, 0xdead_beef] {
            random_changes(seed);
        }
    }

    fn random_changes(seed: u64) {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b, out) = (
            tmp.path().join("a"),
            tmp.path().join("b"),
            tmp.path().join("out"),
        );
        for d in [&a, &b, &out] {
            fs::create_dir_all(d).unwrap();
        }
        let roots = vec![a.clone(), b.clone()];
        let mut w = Walker::new();
        let mut ino = Inotify::new();
        let mut c = Cache::new(roots.clone());
        c.build_watched(&mut w, &mut ino);
        watches_match(&c, &ino, "a build");
        let mut rng = Rng(seed);
        let mut dirs: Vec<PathBuf> = roots.clone();
        let mut files: Vec<PathBuf> = Vec::new();
        let pick = |rng: &mut Rng, v: &[PathBuf]| v[rng.below(v.len())].clone();
        for round in 0..80 {
            for _ in 0..10 {
                match rng.below(10) {
                    // A new tree, written into straight away.
                    0 => {
                        let parent = pick(&mut rng, &dirs);
                        let top = parent.join(format!("d{}", rng.next() % 1000));
                        let deep = top.join("x/y");
                        if fs::create_dir_all(&deep).is_ok() {
                            let f = deep.join("f");
                            if fs::write(&f, vec![1u8; rng.below(100_000)]).is_ok() {
                                files.push(f);
                            }
                            dirs.extend([top.clone(), top.join("x"), deep]);
                        }
                    }
                    1 | 2 => {
                        let f = pick(&mut rng, &dirs).join(format!("f{}", rng.next() % 1000));
                        if fs::write(&f, vec![1u8; rng.below(200_000)]).is_ok() {
                            files.push(f);
                        }
                    }
                    // A file appended to.
                    3 if !files.is_empty() => {
                        use std::io::Write;
                        let f = pick(&mut rng, &files);
                        if let Ok(mut h) = fs::OpenOptions::new().append(true).open(&f) {
                            let _ = h.write_all(&vec![2u8; rng.below(300_000)]);
                        }
                    }
                    4 if !files.is_empty() => {
                        let f = pick(&mut rng, &files);
                        let l = pick(&mut rng, &dirs).join(format!("l{}", rng.next() % 1000));
                        if fs::hard_link(&f, &l).is_ok() {
                            files.push(l);
                        }
                    }
                    5 if !files.is_empty() => {
                        let f = files.swap_remove(rng.below(files.len()));
                        let _ = fs::remove_file(&f);
                    }
                    6 if dirs.len() > 2 => {
                        let gone = dirs[2 + rng.below(dirs.len() - 2)].clone();
                        if fs::remove_dir_all(&gone).is_ok() {
                            dirs.retain(|x| !x.starts_with(&gone));
                            files.retain(|x| !x.starts_with(&gone));
                        }
                    }
                    // A tree moved elsewhere in the trees, under itself
                    // never.
                    7 if dirs.len() > 2 => {
                        let from = dirs[2 + rng.below(dirs.len() - 2)].clone();
                        let parent = pick(&mut rng, &dirs);
                        let to = parent.join(format!("m{}", rng.next() % 1000));
                        if !parent.starts_with(&from) && fs::rename(&from, &to).is_ok() {
                            let moved = |x: &PathBuf| match x.strip_prefix(&from) {
                                Ok(rest) if rest.as_os_str().is_empty() => to.clone(),
                                Ok(rest) => to.join(rest),
                                Err(_) => x.clone(),
                            };
                            dirs = dirs.iter().map(moved).collect();
                            files = files.iter().map(moved).collect();
                        }
                    }
                    // A tree moved out of the trees, and one moved in.
                    8 if dirs.len() > 2 => {
                        let from = dirs[2 + rng.below(dirs.len() - 2)].clone();
                        let to = out.join(format!("o{}", rng.next()));
                        if fs::rename(&from, &to).is_ok() {
                            dirs.retain(|x| !x.starts_with(&from));
                            files.retain(|x| !x.starts_with(&from));
                        }
                    }
                    9 => {
                        let from = out.join(format!("i{}", rng.next()));
                        fs::create_dir_all(from.join("p")).unwrap();
                        fs::write(from.join("p/q"), vec![3u8; 50_000]).unwrap();
                        let to = pick(&mut rng, &dirs).join(format!("i{}", rng.next() % 1000));
                        if fs::rename(&from, &to).is_ok() {
                            dirs.extend([to.clone(), to.join("p")]);
                            files.push(to.join("p/q"));
                        }
                    }
                    _ => {}
                }
            }
            catch_up(&mut c, &mut ino, &mut w);
            let what = format!("round {round} of {seed:#x}");
            if c.total() != usage(&roots) {
                panic!(
                    "after {what}, kept {} but walked {}:{}",
                    c.total(),
                    usage(&roots),
                    c.differences()
                );
            }
            watches_match(&c, &ino, &what);
        }
        drop(c);
        ino.release();
        assert!(!ino.open());
        assert_eq!(ino.watching(), 0);
    }

    #[test]
    fn a_tree_made_and_filled_at_once_is_seen_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("r");
        fs::create_dir_all(&root).unwrap();
        let roots = vec![root.clone()];
        let mut w = Walker::new();
        let mut ino = Inotify::new();
        let mut c = Cache::new(roots.clone());
        c.build_watched(&mut w, &mut ino);
        // Directories made and written into while the cache takes them up:
        // a writer runs alongside each update.
        for i in 0..20 {
            let r = root.clone();
            let writer = std::thread::spawn(move || {
                for j in 0..30 {
                    let d = r.join(format!("t{i}/u{j}"));
                    fs::create_dir_all(&d).unwrap();
                    fs::write(d.join("f"), vec![1u8; 8192]).unwrap();
                }
            });
            while !writer.is_finished() {
                catch_up(&mut c, &mut ino, &mut w);
            }
            writer.join().unwrap();
        }
        // Once it's caught up with everything reported, it's exact.
        catch_up(&mut c, &mut ino, &mut w);
        catch_up(&mut c, &mut ino, &mut w);
        assert_eq!(c.total(), usage(&roots));
        watches_match(&c, &ino, "trees made while it watched");
    }

    #[test]
    fn out_of_watches_it_fails_rather_than_miss_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("r");
        for i in 0..20 {
            fs::create_dir_all(root.join(format!("d{i}"))).unwrap();
        }
        let mut w = Walker::new();
        // As if the process held nearly all it may.
        let mut ino = Inotify::with_budget(held() + 5);
        let mut c = Cache::new(vec![root]);
        c.build_watched(&mut w, &mut ino);
        assert!(ino.failed());
        // Let go of, it holds nothing, and can start again.
        ino.release();
        assert!(!ino.failed() && !ino.open() && ino.watching() == 0);
    }
}
