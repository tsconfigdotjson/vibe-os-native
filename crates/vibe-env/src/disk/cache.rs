//! What a tree takes on disk, kept a directory at a time, so a change is
//! measured by listing again only the directories it touched.
//!
//! The cache holds each directory once: its own bytes and those of its
//! files with a single link, its files with several links, and its
//! directories. Files with several links are counted once across the whole
//! tree, however many directories hold a link, by a count of the links
//! seen. The total is kept as it changes.
//!
//! It's told what changed as [`Dirty`] directories, which is what FSEvents
//! reports on macOS: a directory whose entries changed is listed again on
//! its own, and its directories are walked only where they're new or were
//! replaced; one marked `recursive` is walked again whole. A path the cache
//! doesn't have yet is taken up from the nearest directory it has.
//!
//! One change can touch a directory nobody reports: a link made in one
//! directory to a file in another (`ln`, `cp -al`) gives the file a second
//! link, so where it was counted alone it must now be counted once among
//! its links, but only the new link's directory changed. So when an update
//! finds a file with several links that it didn't hold before, it walks
//! everything afresh. That's rare but for tools that link many files at once
//! (pnpm, say), and they're measured as a walk would measure them. The total
//! is then always what a walk would find. Nothing here depends on how
//! changes are found, so it's tested on every OS.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use super::walk::{Link, Listing, Stat, Visit, Walker};

/// What changed since the cache was last brought up to date.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Dirty {
    /// Anything may have: walk everything again.
    pub all: bool,
    /// Directories whose entries changed, and whether everything under
    /// each may have changed too.
    pub dirs: HashMap<PathBuf, bool>,
}

/// More directories than this waiting is taken as everything having
/// changed: a walk then costs less than keeping them.
pub const MAX_DIRTY: usize = 4096;

impl Dirty {
    pub fn is_empty(&self) -> bool {
        !self.all && self.dirs.is_empty()
    }

    /// Notes that `dir`'s entries changed, or with `recursive` everything
    /// under it.
    pub fn mark(&mut self, dir: &Path, recursive: bool) {
        if self.all {
            return;
        }
        if self.dirs.len() >= MAX_DIRTY && !self.dirs.contains_key(dir) {
            self.everything();
            return;
        }
        let r = self.dirs.entry(dir.to_owned()).or_insert(recursive);
        *r |= recursive;
    }

    pub fn everything(&mut self) {
        self.all = true;
        self.dirs = HashMap::new();
    }

    /// Takes in what `other` says changed.
    pub fn merge(&mut self, other: Dirty) {
        if other.all {
            self.everything();
            return;
        }
        for (d, r) in other.dirs {
            self.mark(&d, r);
        }
    }

    pub fn clear(&mut self) {
        self.all = false;
        self.dirs = HashMap::new();
    }
}

const NONE: u32 = u32::MAX;

#[derive(Debug, Default)]
struct Node {
    parent: u32,
    name: Box<[u8]>,
    ino: u64,
    /// Its own bytes and its files' with a single link.
    own: u64,
    /// Its files with several links.
    links: Vec<(u64, u64)>,
    children: Vec<u32>,
}

/// One of the tree's roots.
#[derive(Debug)]
struct Root {
    path: PathBuf,
    /// Its directory, if it is one.
    node: u32,
    /// It, when it's something else, such as a symbolic link.
    file: Option<Stat>,
}

/// A tree's usage, a directory at a time.
#[derive(Debug)]
pub struct Cache {
    roots: Vec<Root>,
    nodes: Vec<Node>,
    free: Vec<u32>,
    /// Each file with several links, by device and inode: how many of its
    /// links the cache holds, and its bytes.
    links: HashMap<(u64, u64), (u32, u64)>,
    total: u64,
    /// While an update runs: whether it must walk everything afresh, as it
    /// found a file with several links that it didn't hold before, or a
    /// change it can't place.
    rebuild: Option<bool>,
}

impl Cache {
    /// A cache of `roots`, empty until it's [built](Cache::build). Changes
    /// are matched to the roots by path, so they should be the real paths
    /// change notifications use.
    pub fn new(roots: Vec<PathBuf>) -> Cache {
        Cache {
            roots: roots
                .into_iter()
                .map(|path| Root {
                    path,
                    node: NONE,
                    file: None,
                })
                .collect(),
            nodes: Vec::new(),
            free: Vec::new(),
            links: HashMap::new(),
            total: 0,
            rebuild: None,
        }
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    /// How many directories it holds.
    pub fn dirs(&self) -> usize {
        self.nodes.len() - self.free.len()
    }

    /// Walks every root afresh.
    pub fn build(&mut self, walker: &mut Walker) {
        self.nodes = Vec::new();
        self.free = Vec::new();
        self.links = HashMap::new();
        self.total = 0;
        for i in 0..self.roots.len() {
            self.walk_root(i, walker);
        }
    }

    /// Brings it up to date with what `dirty` says changed.
    pub fn update(&mut self, dirty: &Dirty, walker: &mut Walker) {
        if dirty.all {
            self.build(walker);
            return;
        }
        self.rebuild = Some(false);
        self.apply(dirty, walker);
        if self.rebuild.take() == Some(true) {
            self.build(walker);
        }
    }

    fn apply(&mut self, dirty: &Dirty, walker: &mut Walker) {
        // Shallower first, so a directory listed again has its new and gone
        // directories sorted out before anything under it is looked at.
        let mut todo: Vec<(&PathBuf, bool)> = dirty.dirs.iter().map(|(p, r)| (p, *r)).collect();
        todo.sort_by(|a, b| a.0.as_os_str().as_bytes().cmp(b.0.as_os_str().as_bytes()));
        // What's been walked whole already.
        let mut whole: Vec<PathBuf> = Vec::new();
        for (path, recursive) in todo {
            if self.rebuild == Some(true) {
                // Everything is walked afresh anyway.
                return;
            }
            if whole.iter().any(|w| path.starts_with(w)) {
                continue;
            }
            let Some((root, node, exact)) = self.find(path) else {
                // Nothing outside the roots is watched, so a path that's in
                // none is one spelled otherwise (a link, or another case):
                // what it means can't be told, so everything is walked.
                self.rebuild = Some(true);
                return;
            };
            if node == NONE {
                // The root itself isn't a directory the cache has.
                whole.push(self.roots[root].path.clone());
                self.drop_root(root);
                self.walk_root(root, walker);
                continue;
            }
            if recursive && exact {
                self.rewalk(root, node, walker);
                whole.push(path.clone());
            } else {
                self.relist(root, node, walker);
            }
        }
    }

    /// The root `path` is in, and the directory the cache has for it, or
    /// the nearest one above it; and whether that's `path` itself.
    fn find(&self, path: &Path) -> Option<(usize, u32, bool)> {
        let (root, rest) = self
            .roots
            .iter()
            .enumerate()
            .filter_map(|(i, r)| Some((i, path.strip_prefix(&r.path).ok()?)))
            .max_by_key(|(i, _)| self.roots[*i].path.as_os_str().len())?;
        let mut node = self.roots[root].node;
        if node == NONE {
            return Some((root, NONE, false));
        }
        for c in rest.components() {
            let Component::Normal(name) = c else {
                continue;
            };
            match self.child(node, name) {
                Some(n) => node = n,
                None => return Some((root, node, false)),
            }
        }
        Some((root, node, true))
    }

    fn child(&self, node: u32, name: &OsStr) -> Option<u32> {
        self.nodes[node as usize]
            .children
            .iter()
            .copied()
            .find(|&c| *self.nodes[c as usize].name == *name.as_bytes())
    }

    fn path_of(&self, root: usize, node: u32) -> PathBuf {
        let mut names: Vec<&[u8]> = Vec::new();
        let mut n = node;
        while n != self.roots[root].node {
            let node = &self.nodes[n as usize];
            names.push(&node.name);
            n = node.parent;
        }
        let mut path = self.roots[root].path.clone();
        for name in names.into_iter().rev() {
            path.push(OsStr::from_bytes(name));
        }
        path
    }

    /// Walks root `i`, which holds nothing now, whole.
    fn walk_root(&mut self, i: usize, walker: &mut Walker) {
        let path = self.roots[i].path.clone();
        let mut b = Builder {
            cache: self,
            stack: Vec::new(),
            parent: NONE,
            first: NONE,
            file: None,
        };
        walker.walk(&path, usize::MAX, &mut b);
        let (first, file) = (b.first, b.file);
        self.roots[i].node = first;
        if let Some(s) = file {
            if s.links > 1 {
                self.link(&Link {
                    dev: s.dev,
                    ino: s.ino,
                    bytes: s.bytes,
                });
            } else {
                self.total = self.total.saturating_add(s.bytes);
            }
            self.roots[i].file = Some(s);
        }
    }

    /// Lets go of everything root `i` holds.
    fn drop_root(&mut self, i: usize) {
        let node = std::mem::replace(&mut self.roots[i].node, NONE);
        if node != NONE {
            self.remove(node);
        }
        match self.roots[i].file.take() {
            Some(s) if s.links > 1 => self.unlink((s.dev, s.ino)),
            Some(s) => self.total = self.total.saturating_sub(s.bytes),
            None => {}
        }
    }

    /// Walks again everything under `node`.
    fn rewalk(&mut self, root: usize, node: u32, walker: &mut Walker) {
        let parent = self.nodes[node as usize].parent;
        let path = self.path_of(root, node);
        if parent == NONE {
            self.drop_root(root);
            self.walk_root(root, walker);
            return;
        }
        self.remove(node);
        if !self.walk_under(parent, &path, walker) {
            // Not a directory now: its parent counts what's there.
            self.relist(root, parent, walker);
        }
    }

    /// Walks the directory at `path`, new under `parent`, whole. Returns
    /// whether there was one.
    fn walk_under(&mut self, parent: u32, path: &Path, walker: &mut Walker) -> bool {
        let mut b = Builder {
            cache: self,
            stack: Vec::new(),
            parent,
            first: NONE,
            file: None,
        };
        walker.walk(path, usize::MAX, &mut b);
        b.first != NONE
    }

    /// Lists `node` again, walking only what's new in it.
    fn relist(&mut self, root: usize, node: u32, walker: &mut Walker) {
        let path = self.path_of(root, node);
        let mut r = Relist {
            listed: None,
            subdirs: Vec::new(),
        };
        walker.walk(&path, 0, &mut r);
        let Some(listed) = r.listed else {
            // Gone, or not a directory now: its parent says what's there.
            let parent = self.nodes[node as usize].parent;
            if parent == NONE {
                self.drop_root(root);
                self.walk_root(root, walker);
            } else {
                self.relist(root, parent, walker);
            }
            return;
        };
        if listed.ino != self.nodes[node as usize].ino {
            // Another directory by the same name: none of it is known.
            self.rewalk(root, node, walker);
            return;
        }
        self.set_own(node, listed.own, &listed.links);
        let old = std::mem::take(&mut self.nodes[node as usize].children);
        let mut kept = Vec::with_capacity(r.subdirs.len());
        let mut new: Vec<OsString> = Vec::new();
        let mut by_name: HashMap<&[u8], u32> = old
            .iter()
            .map(|&c| (&*self.nodes[c as usize].name, c))
            .collect();
        for (name, stat) in &r.subdirs {
            match by_name.remove(name.as_bytes()) {
                Some(c) if self.nodes[c as usize].ino == stat.ino => kept.push(c),
                Some(c) => {
                    kept.push(c);
                    new.push(name.clone());
                }
                None => new.push(name.clone()),
            }
        }
        let gone: Vec<u32> = by_name.into_values().collect();
        self.nodes[node as usize].children = kept;
        for c in gone {
            self.remove(c);
        }
        for name in new {
            if let Some(c) = self.child(node, &name) {
                self.remove(c);
            }
            self.walk_under(node, &path.join(&name), walker);
        }
    }

    fn set_own(&mut self, node: u32, own: u64, links: &[Link]) {
        let n = &mut self.nodes[node as usize];
        let old_own = std::mem::replace(&mut n.own, own);
        let old_links = std::mem::take(&mut n.links);
        self.total = self.total.saturating_sub(old_own).saturating_add(own);
        // Added before the old ones go, so a file still here isn't let go
        // of and counted again.
        let keys = links.iter().map(|l| self.link(l)).collect();
        self.nodes[node as usize].links = keys;
        for k in old_links {
            self.unlink(k);
        }
    }

    /// Counts one more link to `l`, and its bytes if it's the first, or
    /// what it takes now if that changed.
    fn link(&mut self, l: &Link) -> (u64, u64) {
        let e = self.links.entry(l.key()).or_insert((0, 0));
        if e.0 == 0
            && let Some(rebuild) = &mut self.rebuild
        {
            *rebuild = true;
        }
        e.0 += 1;
        self.total = self.total.saturating_sub(e.1).saturating_add(l.bytes);
        e.1 = l.bytes;
        l.key()
    }

    fn unlink(&mut self, k: (u64, u64)) {
        if let Some(e) = self.links.get_mut(&k) {
            e.0 -= 1;
            if e.0 == 0 {
                self.total = self.total.saturating_sub(e.1);
                self.links.remove(&k);
            }
        }
    }

    fn alloc(&mut self, node: Node) -> u32 {
        if let Some(i) = self.free.pop() {
            self.nodes[i as usize] = node;
            i
        } else {
            self.nodes.push(node);
            u32::try_from(self.nodes.len() - 1).unwrap_or(NONE)
        }
    }

    /// Removes `node` and everything under it.
    fn remove(&mut self, node: u32) {
        let parent = self.nodes[node as usize].parent;
        if parent != NONE {
            self.nodes[parent as usize].children.retain(|&c| c != node);
        }
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            let gone = std::mem::take(&mut self.nodes[n as usize]);
            self.total = self.total.saturating_sub(gone.own);
            for k in gone.links {
                self.unlink(k);
            }
            stack.extend(gone.children);
            self.free.push(n);
        }
    }
}

/// Adds what a walk finds under `parent` to the cache.
struct Builder<'a> {
    cache: &'a mut Cache,
    /// The directory last listed at each depth of the walk.
    stack: Vec<u32>,
    parent: u32,
    /// The walk's root, once listed.
    first: u32,
    /// The walk's root, when it isn't a directory.
    file: Option<Stat>,
}

impl Visit for Builder<'_> {
    fn dir(&mut self, l: &Listing<'_>) -> bool {
        self.stack.truncate(l.depth);
        let parent = self.stack.last().copied().unwrap_or(self.parent);
        let node = self.cache.alloc(Node {
            parent,
            name: l.name().as_bytes().into(),
            ino: l.stat.ino,
            ..Node::default()
        });
        self.cache.set_own(node, l.own, l.links);
        if parent != NONE {
            self.cache.nodes[parent as usize].children.push(node);
        }
        if l.depth == 0 {
            self.first = node;
        }
        self.stack.push(node);
        true
    }

    fn root_file(&mut self, stat: Stat) {
        self.file = Some(stat);
    }
}

/// One directory, listed again.
struct Relist {
    listed: Option<Listed>,
    subdirs: Vec<(OsString, Stat)>,
}

struct Listed {
    ino: u64,
    own: u64,
    links: Vec<Link>,
}

impl Visit for Relist {
    fn dir(&mut self, l: &Listing<'_>) -> bool {
        self.listed = Some(Listed {
            ino: l.stat.ino,
            own: l.own,
            links: l.links.to_vec(),
        });
        self.subdirs = l.subdirs().map(|(n, s)| (n.to_owned(), s)).collect();
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::walk::usage;
    use std::fs;

    fn check(cache: &Cache, roots: &[PathBuf], what: &str) {
        assert_eq!(cache.total(), usage(roots), "after {what}");
    }

    fn dirty(dirs: &[(&Path, bool)]) -> Dirty {
        let mut d = Dirty::default();
        for (p, r) in dirs {
            d.mark(p, *r);
        }
        d
    }

    #[test]
    fn changes_are_measured_from_the_directories_they_touched() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        fs::create_dir_all(a.join("x/y/z")).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("x/y/z/f"), vec![1u8; 100_000]).unwrap();
        let roots = vec![a.clone(), b.clone()];
        let mut w = Walker::new();
        let mut c = Cache::new(roots.clone());
        c.build(&mut w);
        check(&c, &roots, "a build");
        assert_eq!(c.dirs(), 5);

        // A file grows deep down.
        fs::write(a.join("x/y/z/f"), vec![1u8; 900_000]).unwrap();
        c.update(&dirty(&[(&a.join("x/y/z"), false)]), &mut w);
        check(&c, &roots, "a file grew");

        // A new tree, told by its parent.
        fs::create_dir_all(a.join("x/new/deeper/still")).unwrap();
        fs::write(a.join("x/new/deeper/still/g"), vec![2u8; 300_000]).unwrap();
        fs::write(a.join("x/new/h"), vec![2u8; 50_000]).unwrap();
        c.update(&dirty(&[(&a.join("x"), false)]), &mut w);
        check(&c, &roots, "a new tree");

        // Told by a path the cache doesn't have yet: taken up from the
        // nearest directory it does.
        fs::create_dir_all(b.join("p/q")).unwrap();
        fs::write(b.join("p/q/r"), vec![3u8; 70_000]).unwrap();
        c.update(&dirty(&[(&b.join("p/q"), false)]), &mut w);
        check(&c, &roots, "a path not in the cache");

        // A tree removed, and a stale change under it.
        fs::remove_dir_all(a.join("x/y")).unwrap();
        c.update(
            &dirty(&[(&a.join("x"), false), (&a.join("x/y/z"), false)]),
            &mut w,
        );
        check(&c, &roots, "a tree removed");

        // A directory replaced by a file of its name.
        fs::remove_dir_all(a.join("x/new")).unwrap();
        fs::write(a.join("x/new"), vec![4u8; 20_000]).unwrap();
        c.update(&dirty(&[(&a.join("x/new"), false)]), &mut w);
        check(&c, &roots, "a directory replaced by a file");

        // A directory replaced by another of its name, told only by itself.
        fs::create_dir_all(b.join("p/s")).unwrap();
        c.update(&dirty(&[(&b.join("p"), false)]), &mut w);
        fs::remove_dir_all(b.join("p/s")).unwrap();
        fs::create_dir_all(b.join("p/s/t")).unwrap();
        fs::write(b.join("p/s/t/u"), vec![5u8; 40_000]).unwrap();
        c.update(&dirty(&[(&b.join("p/s"), false)]), &mut w);
        check(&c, &roots, "a directory replaced by another");

        // A directory replaced by a file, told only as everything under it.
        fs::create_dir_all(a.join("x/gone/deep")).unwrap();
        c.update(&dirty(&[(&a.join("x"), true)]), &mut w);
        fs::remove_dir_all(a.join("x/gone")).unwrap();
        fs::write(a.join("x/gone"), vec![9u8; 200_000]).unwrap();
        c.update(&dirty(&[(&a.join("x/gone"), true)]), &mut w);
        check(&c, &roots, "a directory replaced by a file, told by itself");

        // Everything under a directory, at once.
        fs::create_dir_all(a.join("x/m/n")).unwrap();
        fs::write(a.join("x/m/n/o"), vec![6u8; 10_000]).unwrap();
        fs::write(a.join("x/m/p"), vec![6u8; 10_000]).unwrap();
        c.update(&dirty(&[(&a.join("x/m"), true)]), &mut w);
        check(&c, &roots, "a subtree walked again");

        // A root removed, then made again.
        fs::remove_dir_all(&b).unwrap();
        c.update(&dirty(&[(&b, false)]), &mut w);
        check(&c, &roots, "a root removed");
        fs::create_dir_all(b.join("again")).unwrap();
        fs::write(b.join("again/v"), vec![7u8; 60_000]).unwrap();
        c.update(&dirty(&[(&b, true)]), &mut w);
        check(&c, &roots, "a root made again");

        // Unchanged, an update changes nothing; and a path in no root,
        // which can't be placed, has everything walked again.
        let before = c.total();
        c.update(&dirty(&[(&a.join("x"), false)]), &mut w);
        assert_eq!(c.total(), before);
        fs::write(a.join("x/unseen"), vec![8u8; 30_000]).unwrap();
        c.update(&dirty(&[(&tmp.path().join("elsewhere"), true)]), &mut w);
        check(&c, &roots, "a path in no root");
    }

    #[test]
    fn hard_links_count_once_as_they_come_and_go() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        fs::create_dir_all(a.join("one")).unwrap();
        fs::create_dir_all(a.join("two")).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("one/f"), vec![1u8; 500_000]).unwrap();
        let roots = vec![a.clone(), b.clone()];
        let mut w = Walker::new();
        let mut c = Cache::new(roots.clone());
        c.build(&mut w);
        let alone = c.total();
        check(&c, &roots, "a build");

        fs::hard_link(a.join("one/f"), a.join("two/f")).unwrap();
        fs::hard_link(a.join("one/f"), b.join("f")).unwrap();
        c.update(
            &dirty(&[
                (&a.join("two"), false),
                (&b, false),
                (&a.join("one"), false),
            ]),
            &mut w,
        );
        check(&c, &roots, "two more links");
        // Directories' sizes aside, it's still one file.
        assert!(c.total() < alone + (16 << 10), "{alone} {}", c.total());

        // It grows, seen from one of its directories.
        fs::write(b.join("f"), vec![1u8; 2_000_000]).unwrap();
        c.update(&dirty(&[(&b, false)]), &mut w);
        check(&c, &roots, "it grew");

        // Its links go one by one.
        fs::remove_file(a.join("one/f")).unwrap();
        c.update(&dirty(&[(&a.join("one"), false)]), &mut w);
        check(&c, &roots, "one link gone");
        fs::remove_dir_all(a.join("two")).unwrap();
        c.update(&dirty(&[(&a, false)]), &mut w);
        check(&c, &roots, "a directory with a link gone");
        fs::remove_file(b.join("f")).unwrap();
        c.update(&dirty(&[(&b, false)]), &mut w);
        check(&c, &roots, "the last link gone");
        assert!(c.total() < alone, "{alone} {}", c.total());
        assert!(c.links.is_empty());
    }

    #[test]
    fn a_second_link_from_another_directory_isnt_counted_twice() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        fs::create_dir_all(a.join("old")).unwrap();
        fs::create_dir_all(a.join("new")).unwrap();
        fs::write(a.join("old/f"), vec![1u8; 1 << 20]).unwrap();
        let roots = vec![a.clone()];
        let mut w = Walker::new();
        let mut c = Cache::new(roots.clone());
        c.build(&mut w);
        // Only the directory of the new link changed: the file was counted
        // alone in the other.
        fs::hard_link(a.join("old/f"), a.join("new/g")).unwrap();
        c.update(&dirty(&[(&a.join("new"), false)]), &mut w);
        check(&c, &roots, "a link from another directory");
        // A copy of a whole directory as links (`cp -al`), told by its
        // parent.
        fs::write(a.join("old/h"), vec![2u8; 300_000]).unwrap();
        c.update(&dirty(&[(&a.join("old"), false)]), &mut w);
        fs::create_dir(a.join("copy")).unwrap();
        for f in ["f", "h"] {
            fs::hard_link(a.join("old").join(f), a.join("copy").join(f)).unwrap();
        }
        c.update(&dirty(&[(&a, false)]), &mut w);
        check(&c, &roots, "a directory copied as links");
    }

    #[test]
    fn symbolic_links_arent_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, out) = (tmp.path().join("a"), tmp.path().join("out"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("big"), vec![1u8; 4 << 20]).unwrap();
        let roots = vec![a.clone()];
        let mut w = Walker::new();
        let mut c = Cache::new(roots.clone());
        c.build(&mut w);
        std::os::unix::fs::symlink(&out, a.join("out")).unwrap();
        c.update(&dirty(&[(&a, false)]), &mut w);
        check(&c, &roots, "a link to a directory outside");
        assert!(c.total() < 1 << 20);
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

    #[test]
    fn random_changes_told_as_fsevents_tells_them_keep_it_exact() {
        for seed in [
            0x9e37_79b9_7f4a_7c15,
            0x2545_f491_4f6c_dd1d,
            0xdead_beef_cafe_f00d,
        ] {
            random_changes(seed);
        }
    }

    fn random_changes(seed: u64) {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let roots = vec![a.clone(), b.clone()];
        let mut w = Walker::new();
        let mut c = Cache::new(roots.clone());
        c.build(&mut w);
        let mut rng = Rng(seed);
        let mut dirs: Vec<PathBuf> = roots.clone();
        let mut files: Vec<PathBuf> = Vec::new();
        for round in 0..60 {
            let mut d = Dirty::default();
            for _ in 0..8 {
                match rng.below(6) {
                    0 => {
                        let parent = dirs[rng.below(dirs.len())].clone();
                        let new = parent.join(format!("d{}", rng.next() % 1000));
                        if fs::create_dir(&new).is_ok() {
                            dirs.push(new);
                            d.mark(&parent, false);
                        }
                    }
                    1 | 2 => {
                        let parent = dirs[rng.below(dirs.len())].clone();
                        let f = parent.join(format!("f{}", rng.next() % 1000));
                        let len = rng.below(200_000);
                        if fs::write(&f, vec![1u8; len]).is_ok() {
                            files.push(f);
                            d.mark(&parent, false);
                        }
                    }
                    3 if !files.is_empty() => {
                        let f = files[rng.below(files.len())].clone();
                        let parent = dirs[rng.below(dirs.len())].clone();
                        let l = parent.join(format!("l{}", rng.next() % 1000));
                        if fs::hard_link(&f, &l).is_ok() {
                            files.push(l);
                            d.mark(&parent, false);
                        }
                    }
                    4 if !files.is_empty() => {
                        let i = rng.below(files.len());
                        let f = files.swap_remove(i);
                        if fs::remove_file(&f).is_ok() {
                            d.mark(f.parent().unwrap(), false);
                        }
                    }
                    5 if dirs.len() > 2 => {
                        let i = 2 + rng.below(dirs.len() - 2);
                        let gone = dirs[i].clone();
                        if fs::remove_dir_all(&gone).is_ok() {
                            dirs.retain(|x| !x.starts_with(&gone));
                            files.retain(|x| !x.starts_with(&gone));
                            d.mark(gone.parent().unwrap(), false);
                        }
                    }
                    _ => {}
                }
            }
            c.update(&d, &mut w);
            check(&c, &roots, &format!("round {round} of {seed:#x}"));
        }
        // And built afresh, it's the same.
        let kept = c.total();
        c.build(&mut w);
        assert_eq!(c.total(), kept);
    }

    #[test]
    fn too_many_changes_are_everything() {
        let mut d = Dirty::default();
        for i in 0..MAX_DIRTY {
            d.mark(Path::new(&format!("/x/{i}")), false);
        }
        assert!(!d.all);
        d.mark(Path::new("/x/0"), true);
        assert!(!d.all && d.dirs[Path::new("/x/0")]);
        d.mark(Path::new("/y"), false);
        assert!(d.all && d.dirs.is_empty());
        let mut e = Dirty::default();
        e.mark(Path::new("/z"), false);
        e.merge(d);
        assert!(e.all);
        e.clear();
        assert!(e.is_empty());
    }
}
