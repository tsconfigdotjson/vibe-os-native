//! The walk under every disk measurement: a directory at a time, with
//! nothing allocated per file.
//!
//! Each directory is opened by its path (`O_NOFOLLOW`, so a symbolic link
//! is never followed into), listed whole and closed again, with every entry
//! looked at by `fstatat` against it, so a file costs one system call and
//! no allocation. Only directories are kept for later, as names in one
//! buffer that's reused like a stack. One directory is open at a time, so
//! however deep a tree goes it never runs out of descriptors, and the
//! buffers a [`Walker`] keeps are reused from walk to walk.
//!
//! What it counts is what's allocated on disk (`st_blocks`), as `du` does:
//! each directory, each file and each symbolic link itself, staying on the
//! root's filesystem. A file with more than one link is reported as a
//! [`Link`], for the caller to count once however many of its links it
//! finds.

use std::collections::HashSet;
use std::ffi::{CStr, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// One entry, as `lstat` sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    /// What it takes on disk.
    pub bytes: u64,
    pub links: u64,
    pub dir: bool,
}

impl Stat {
    #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
    fn of(st: &libc::stat) -> Stat {
        Stat {
            // `dev_t` is signed on macOS; it's only ever compared.
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
            bytes: u64::try_from(st.st_blocks).unwrap_or(0).saturating_mul(512),
            links: st.st_nlink as u64,
            dir: st.st_mode & libc::S_IFMT == libc::S_IFDIR,
        }
    }
}

/// A file with more than one link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Link {
    pub dev: u64,
    pub ino: u64,
    pub bytes: u64,
}

impl Link {
    pub fn key(&self) -> (u64, u64) {
        (self.dev, self.ino)
    }
}

/// A directory in a listing, its name in the walker's buffer.
#[derive(Debug, Clone, Copy)]
struct Subdir {
    start: usize,
    len: usize,
    stat: Stat,
}

/// What one directory holds directly.
#[derive(Debug)]
pub struct Listing<'a> {
    /// 0 for the root of the walk.
    pub depth: usize,
    pub path: &'a Path,
    /// The directory itself.
    pub stat: Stat,
    /// Its own blocks, and those of every entry in it with a single link
    /// that isn't a directory.
    pub own: u64,
    /// Its entries with more than one link.
    pub links: &'a [Link],
    subdirs: &'a [Subdir],
    names: &'a [u8],
}

impl Listing<'_> {
    /// Its directories, by name.
    pub fn subdirs(&self) -> impl Iterator<Item = (&OsStr, Stat)> + '_ {
        self.subdirs.iter().map(|s| {
            (
                OsStr::from_bytes(&self.names[s.start..s.start + s.len]),
                s.stat,
            )
        })
    }

    /// Its own name: the last part of its path.
    pub fn name(&self) -> &OsStr {
        self.path.file_name().unwrap_or(self.path.as_os_str())
    }
}

/// What a walk tells, a directory at a time.
pub trait Visit {
    /// A directory, listed. Returns whether to go into its directories:
    /// each is then listed, with all that's under it, before anything else
    /// at its depth or above.
    fn dir(&mut self, listing: &Listing<'_>) -> bool;

    /// A root that isn't a directory.
    fn root_file(&mut self, _stat: Stat) {}
}

/// A directory waiting to be listed.
#[derive(Debug, Clone, Copy)]
struct Pending {
    start: usize,
    len: usize,
    depth: usize,
    stat: Stat,
}

/// Walks trees, keeping its buffers from one walk to the next.
#[derive(Debug, Default)]
pub struct Walker {
    /// The directory being listed.
    path: Vec<u8>,
    /// The length of `path` at each depth down to it.
    depth_len: Vec<usize>,
    /// The names of the directories waiting, in the order they wait.
    names: Vec<u8>,
    pending: Vec<Pending>,
    subdirs: Vec<Subdir>,
    links: Vec<Link>,
    /// Files with several links counted so far, for [`Walker::usage`].
    seen: HashSet<(u64, u64)>,
}

/// More hard links than this kept between walks is let go of once a walk
/// finds far fewer.
const KEEP_SEEN: usize = 1 << 16;

impl Walker {
    pub fn new() -> Walker {
        Walker::default()
    }

    /// The bytes `roots` take on disk: each file once however many links
    /// it has, without following symbolic links or leaving a root's
    /// filesystem. A root that isn't there is nothing.
    pub fn usage<P: AsRef<Path>>(&mut self, roots: &[P]) -> u64 {
        let mut total = Total {
            seen: std::mem::take(&mut self.seen),
            bytes: 0,
        };
        for root in roots {
            self.walk(root.as_ref(), usize::MAX, &mut total);
        }
        let mut seen = total.seen;
        if seen.capacity() > KEEP_SEEN && seen.len() < seen.capacity() / 4 {
            seen = HashSet::new();
        } else {
            seen.clear();
        }
        self.seen = seen;
        total.bytes
    }

    /// Walks `root`, going no deeper than `max_depth` below it.
    pub fn walk(&mut self, root: &Path, max_depth: usize, v: &mut impl Visit) {
        self.pending.clear();
        self.names.clear();
        self.depth_len.clear();
        self.path.clear();
        self.path.extend_from_slice(root.as_os_str().as_bytes());
        while self.path.len() > 1 && self.path.ends_with(b"/") {
            self.path.pop();
        }
        let Some(stat) = self.lstat() else {
            return;
        };
        if !stat.dir {
            v.root_file(stat);
            return;
        }
        let root_len = self.path.len();
        self.pending.push(Pending {
            start: 0,
            len: 0,
            depth: 0,
            stat,
        });
        while let Some(p) = self.pending.pop() {
            if p.depth == 0 {
                self.path.truncate(root_len);
            } else {
                self.path.truncate(self.depth_len[p.depth - 1]);
                self.path.push(b'/');
                self.path
                    .extend_from_slice(&self.names[p.start..p.start + p.len]);
                // Its name was the last waiting; it's in the path now.
                self.names.truncate(p.start);
            }
            self.depth_len.truncate(p.depth);
            self.depth_len.push(self.path.len());
            let mark = self.names.len();
            let own = self.list(p.stat, stat.dev);
            let descend = v.dir(&Listing {
                depth: p.depth,
                path: Path::new(OsStr::from_bytes(&self.path)),
                stat: p.stat,
                own,
                links: &self.links,
                subdirs: &self.subdirs,
                names: &self.names,
            }) && p.depth < max_depth;
            if descend {
                self.pending.extend(self.subdirs.iter().map(|s| Pending {
                    start: s.start,
                    len: s.len,
                    depth: p.depth + 1,
                    stat: s.stat,
                }));
            } else {
                self.names.truncate(mark);
            }
        }
    }

    /// `lstat` of `path`.
    fn lstat(&mut self) -> Option<Stat> {
        self.path.push(0);
        // SAFETY: `path` is NUL-terminated, and `st` is plain data for lstat
        // to fill.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::lstat(self.path.as_ptr().cast(), &mut st) };
        self.path.pop();
        (r == 0).then(|| Stat::of(&st))
    }

    /// Lists the directory at `path`, which its parent's listing said is
    /// `stat`: its directories into `subdirs` (their names into `names`),
    /// its files with several links into `links`. Returns its own blocks
    /// and its other files'. One it can't list (gone, unreadable, or not
    /// the directory that was listed) holds nothing.
    fn list(&mut self, stat: Stat, dev: u64) -> u64 {
        self.subdirs.clear();
        self.links.clear();
        let mut own = stat.bytes;
        self.path.push(0);
        // SAFETY: `path` is NUL-terminated.
        let fd = unsafe {
            libc::open(
                self.path.as_ptr().cast(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        self.path.pop();
        if fd < 0 {
            return own;
        }
        // SAFETY: `fd` is open, and `st` is plain data for fstat to fill.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } != 0 || {
            let opened = Stat::of(&st);
            opened.dev != stat.dev || opened.ino != stat.ino
        } {
            // SAFETY: `fd` is open and closed once.
            unsafe { libc::close(fd) };
            return own;
        }
        // SAFETY: `fd` is an open directory; on success the stream owns it.
        let dir = unsafe { libc::fdopendir(fd) };
        if dir.is_null() {
            // SAFETY: fdopendir failed, so `fd` is still ours to close.
            unsafe { libc::close(fd) };
            return own;
        }
        loop {
            // SAFETY: `dir` is an open stream; the entry stays valid until
            // the next readdir or closedir, and its name is NUL-terminated.
            let entry = unsafe { libc::readdir(dir) };
            if entry.is_null() {
                break;
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            let bytes = name.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            // SAFETY: `fd` is the stream's directory, `name` an entry in it,
            // and `st` plain data for fstatat to fill.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstatat(fd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0
            {
                continue;
            }
            let s = Stat::of(&st);
            if s.dev != dev {
                // Something mounted here isn't this tree's.
                continue;
            }
            if s.dir {
                self.subdirs.push(Subdir {
                    start: self.names.len(),
                    len: bytes.len(),
                    stat: s,
                });
                self.names.extend_from_slice(bytes);
            } else if s.links > 1 {
                self.links.push(Link {
                    dev: s.dev,
                    ino: s.ino,
                    bytes: s.bytes,
                });
            } else {
                own = own.saturating_add(s.bytes);
            }
        }
        // SAFETY: `dir` is open, and closing it closes `fd`.
        unsafe { libc::closedir(dir) };
        own
    }
}

/// Adds up a walk, each file with several links once.
struct Total {
    seen: HashSet<(u64, u64)>,
    bytes: u64,
}

impl Total {
    fn link(&mut self, l: &Link) {
        if self.seen.insert(l.key()) {
            self.bytes = self.bytes.saturating_add(l.bytes);
        }
    }
}

impl Visit for Total {
    fn dir(&mut self, listing: &Listing<'_>) -> bool {
        self.bytes = self.bytes.saturating_add(listing.own);
        for l in listing.links {
            self.link(l);
        }
        true
    }

    fn root_file(&mut self, s: Stat) {
        if s.links > 1 {
            self.link(&Link {
                dev: s.dev,
                ino: s.ino,
                bytes: s.bytes,
            });
        } else {
            self.bytes = self.bytes.saturating_add(s.bytes);
        }
    }
}

/// The bytes `paths` take on disk, as [`Walker::usage`] counts them.
pub fn usage<P: AsRef<Path>>(paths: &[P]) -> u64 {
    Walker::new().usage(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;

    /// The old walk, an allocation per entry: what the new one must agree
    /// with, but for staying on one filesystem.
    fn naive(paths: &[PathBuf]) -> u64 {
        let mut seen = HashSet::new();
        let mut total = 0u64;
        let mut stack: Vec<PathBuf> = paths.to_vec();
        while let Some(path) = stack.pop() {
            let Ok(m) = fs::symlink_metadata(&path) else {
                continue;
            };
            if m.nlink() > 1 && !m.is_dir() && !seen.insert((m.dev(), m.ino())) {
                continue;
            }
            total += m.blocks() * 512;
            if m.is_dir()
                && let Ok(entries) = fs::read_dir(&path)
            {
                stack.extend(entries.filter_map(|e| e.ok().map(|e| e.path())));
            }
        }
        total
    }

    /// A tree of every kind of entry: nested directories, empty ones, small
    /// and large files, a hard link within it, and symbolic links to a
    /// file, to a directory outside it and to nothing.
    fn tree(root: &Path, outside: &Path) {
        fs::create_dir_all(root.join("a/b/c/d")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::create_dir_all(outside).unwrap();
        fs::write(outside.join("big"), vec![7u8; 3 << 20]).unwrap();
        for i in 0..40 {
            let d = root.join(format!("a/b/{}", i % 3));
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join(format!("f{i}")), vec![1u8; i * 1000]).unwrap();
        }
        fs::write(root.join("a/b/c/d/large"), vec![2u8; 1 << 20]).unwrap();
        fs::hard_link(root.join("a/b/c/d/large"), root.join("a/again")).unwrap();
        std::os::unix::fs::symlink(outside, root.join("a/out")).unwrap();
        std::os::unix::fs::symlink(root.join("a/again"), root.join("to-file")).unwrap();
        std::os::unix::fs::symlink("/nowhere/at/all", root.join("dangling")).unwrap();
    }

    #[test]
    fn it_agrees_with_a_naive_walk() {
        let dir = tempfile::tempdir().unwrap();
        let (root, outside) = (dir.path().join("root"), dir.path().join("outside"));
        tree(&root, &outside);
        let roots = [root.clone(), dir.path().join("missing")];
        let want = naive(&roots);
        assert_eq!(usage(&roots), want);
        // The same walker again gives the same answer.
        let mut w = Walker::new();
        assert_eq!(w.usage(&roots), want);
        assert_eq!(w.usage(&roots), want);
        // A root that's a file is that file.
        let file = root.join("a/b/c/d/large");
        assert_eq!(usage(std::slice::from_ref(&file)), naive(&[file]));
    }

    #[test]
    fn hard_links_count_once_across_roots_and_links_arent_followed() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, outside) = (
            dir.path().join("a"),
            dir.path().join("b"),
            dir.path().join("outside"),
        );
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let empty = usage(&[&a, &b]);
        fs::write(a.join("one"), vec![1u8; 1 << 20]).unwrap();
        let one = usage(&[&a, &b]);
        assert!(one >= empty + (1 << 20), "{empty} then {one}");
        // A link to it in the same root, and in the other: nothing more.
        fs::hard_link(a.join("one"), a.join("two")).unwrap();
        fs::hard_link(a.join("one"), b.join("three")).unwrap();
        assert_eq!(usage(&[&a, &b]), one);
        // Counted once even when a root is the file itself.
        assert_eq!(usage(&[a.join("one"), a.clone()]), usage(&[&a]));
        // A link to a big directory outside, or a root that's a link to
        // one, adds nothing much.
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("f"), vec![1u8; 4 << 20]).unwrap();
        std::os::unix::fs::symlink(&outside, a.join("big")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.path().join("root-link")).unwrap();
        let after = usage(&[&a, &b]);
        assert!(after < one + (64 << 10), "{one} then {after}");
        assert!(usage(&[dir.path().join("root-link")]) < 64 << 10);
        // Another link to the same file outside every root still counts
        // it, once: it's on disk because of this tree too.
        fs::hard_link(a.join("one"), outside.join("pinned")).unwrap();
        assert_eq!(usage(&[&a, &b]), after);
    }

    #[test]
    fn deep_trees_dont_run_out_of_descriptors() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = dir.path().join("deep");
        // Deeper than a process's usual 256 to 1024 descriptors, were each
        // level kept open, but within PATH_MAX.
        for _ in 0..1500 {
            d.push("d");
        }
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("leaf"), vec![1u8; 10_000]).unwrap();
        let mut levels = 0;
        struct Count<'a>(&'a mut usize);
        impl Visit for Count<'_> {
            fn dir(&mut self, _: &Listing<'_>) -> bool {
                *self.0 += 1;
                true
            }
        }
        Walker::new().walk(
            &dir.path().join("deep"),
            usize::MAX,
            &mut Count(&mut levels),
        );
        assert_eq!(levels, 1501);
        let roots = [dir.path().join("deep")];
        assert_eq!(usage(&roots), naive(&roots));
    }

    #[test]
    fn listings_come_depth_first_with_their_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("r");
        for d in ["x/y", "x/z", "w"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(root.join("x/file"), b"hello").unwrap();
        #[derive(Default)]
        struct Seen(Vec<(usize, String, Vec<String>)>);
        impl Visit for Seen {
            fn dir(&mut self, l: &Listing<'_>) -> bool {
                let mut subs: Vec<String> = l
                    .subdirs()
                    .map(|(n, _)| n.to_string_lossy().into_owned())
                    .collect();
                subs.sort();
                self.0
                    .push((l.depth, l.name().to_string_lossy().into_owned(), subs));
                true
            }
        }
        let mut seen = Seen::default();
        Walker::new().walk(&root, usize::MAX, &mut seen);
        // Each directory comes after its parent, and everything under x
        // comes together.
        let names: Vec<&str> = seen.0.iter().map(|(_, n, _)| n.as_str()).collect();
        assert_eq!(names[0], "r");
        let x = names.iter().position(|n| *n == "x").unwrap();
        let mut under_x: Vec<&str> = names[x + 1..x + 3].to_vec();
        under_x.sort();
        assert_eq!(under_x, ["y", "z"]);
        assert_eq!(seen.0[x].0, 1);
        assert_eq!(seen.0[x].2, ["y", "z"]);
        // Going no deeper than the root lists it alone.
        let mut top = Seen::default();
        Walker::new().walk(&root, 0, &mut top);
        assert_eq!(top.0.len(), 1);
        assert_eq!(top.0[0].2, ["w", "x"]);
    }
}
