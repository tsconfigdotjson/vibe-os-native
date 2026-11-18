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
//! (pnpm, say), and they're measured as a walk would measure them. Likewise
//! a file with several links written through one of them and that link
//! then removed: only that link's directory is reported, and with it gone
//! nothing says what the file takes now, so an update that lets go of a
//! link to a file it still holds, without seeing what that file takes in
//! the same update, walks everything afresh too. The total is then always
//! what a walk would find. Nothing here depends on how
//! changes are found, so it's tested on every OS.
//!
//! What finds them may need to know each directory the cache holds, as
//! inotify does (a watch per directory): a [`Watcher`] is told of each one
//! before it's listed, and of each one let go of.
//!
//! It holds at most [`MAX_DIRS`] directories and [`MAX_LINKS`] files with
//! several links. A tree bigger than that is more than it's worth keeping a
//! directory at a time: the cache says it's [over](Cache::over), and is let
//! go of for a walk.

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

/// The most directories a cache holds, about 9 MiB of them.
pub const MAX_DIRS: usize = 1 << 16;

/// The most files with several links a cache holds.
pub const MAX_LINKS: usize = 1 << 17;

/// What has to see each directory the cache holds, to say when it changes.
pub trait Watcher {
    /// The directory at `path` is about to be listed, to be held: start
    /// seeing what changes in it. Returns its tag, `0` for none.
    fn watch(&mut self, path: &Path) -> i32;

    /// The directory tagged `tag` is held as `node`.
    fn added(&mut self, tag: i32, node: u32);

    /// The directory tagged `tag`, held as `node`, is let go of.
    fn removed(&mut self, tag: i32, node: u32);

    /// Every directory is let go of at once.
    fn cleared(&mut self);
}

/// Sees nothing, for changes found some other way (FSEvents watches whole
/// trees).
#[derive(Debug, Default)]
pub struct NoWatch;

impl Watcher for NoWatch {
    fn watch(&mut self, _: &Path) -> i32 {
        0
    }
    fn added(&mut self, _: i32, _: u32) {}
    fn removed(&mut self, _: i32, _: u32) {}
    fn cleared(&mut self) {}
}

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

/// A file with several links.
#[derive(Debug, Default, Clone, Copy)]
struct Linked {
    /// How many of its links the cache holds.
    count: u32,
    /// What it takes.
    bytes: u64,
    /// The update that last saw what it takes.
    seen: u32,
}

/// The parent of a node let go of, whose room waits to be used again.
const FREED: u32 = u32::MAX - 1;

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
    /// What its [`Watcher`] tagged it, `0` for nothing.
    tag: i32,
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
    /// Each file with several links, by device and inode.
    links: HashMap<(u64, u64), Linked>,
    /// Counts updates, to say which of them last saw what a file with
    /// several links takes.
    updates: u32,
    /// Files with several links that the update running let go of a link
    /// to, but still holds.
    unsure: Vec<(u64, u64)>,
    total: u64,
    /// While an update runs: whether it must walk everything afresh, as it
    /// found a file with several links that it didn't hold before, or a
    /// change it can't place.
    rebuild: Option<bool>,
    /// The most directories and files with several links it holds.
    max: (usize, usize),
    /// Whether it found more than that.
    over: bool,
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
            updates: 0,
            unsure: Vec::new(),
            total: 0,
            rebuild: None,
            max: (MAX_DIRS, MAX_LINKS),
            over: false,
        }
    }

    /// Holds no more than `dirs` directories and `links` files with several
    /// links.
    pub fn with_max(mut self, dirs: usize, links: usize) -> Cache {
        self.max = (dirs, links);
        self
    }

    /// What the tree takes, unless it's [over](Cache::over).
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Whether the tree has more directories or files with several links
    /// than it holds. Its total then means nothing, and it holds only part
    /// of the tree, until it's built again.
    pub fn over(&self) -> bool {
        self.over
    }

    /// How many directories it holds.
    pub fn dirs(&self) -> usize {
        self.nodes.len() - self.free.len()
    }

    /// Whether most of the room it has for directories is unused, after a
    /// tree that shrank, so that it's worth building afresh to let go of
    /// it.
    pub fn sparse(&self) -> bool {
        self.nodes.len() >= 1024 && self.free.len() * 2 > self.nodes.len()
    }

    /// The path of directory `node`, if it holds one.
    pub fn path(&self, node: u32) -> Option<PathBuf> {
        let mut top = node;
        // No deeper than it holds directories, so a node let go of (whose
        // parent means nothing) can't send it round for ever.
        for _ in 0..=self.nodes.len() {
            let n = self.nodes.get(top as usize)?;
            match n.parent {
                NONE => break,
                FREED => return None,
                p => top = p,
            }
        }
        if self.nodes.get(top as usize)?.parent != NONE {
            return None;
        }
        let root = self.roots.iter().position(|r| r.node == top)?;
        Some(self.path_of(root, node))
    }

    /// The root `node` is, if it's one.
    pub fn root_of(&self, node: u32) -> Option<&Path> {
        self.roots
            .iter()
            .find(|r| r.node == node)
            .map(|r| r.path.as_path())
    }

    /// Walks every root afresh.
    pub fn build(&mut self, walker: &mut Walker) {
        self.build_watched(walker, &mut NoWatch);
    }

    /// Walks every root afresh, `watcher` seeing each directory.
    pub fn build_watched(&mut self, walker: &mut Walker, watcher: &mut dyn Watcher) {
        watcher.cleared();
        // Kept, as a build mostly fills it again.
        self.nodes.clear();
        self.free = Vec::new();
        self.links = HashMap::new();
        self.total = 0;
        self.over = false;
        for r in &mut self.roots {
            r.node = NONE;
            r.file = None;
        }
        for i in 0..self.roots.len() {
            self.walk_root(i, walker, watcher);
        }
        // A tree that shrank lets go of the room it took.
        if self.nodes.capacity() > 2 * self.nodes.len() + 1024 {
            self.nodes.shrink_to_fit();
        }
    }

    /// Brings it up to date with what `dirty` says changed.
    pub fn update(&mut self, dirty: &Dirty, walker: &mut Walker) {
        self.update_watched(dirty, walker, &mut NoWatch);
    }

    /// Each root that isn't there (or can't be looked at), which nothing
    /// watching the trees will say has come back.
    pub fn missing(&self) -> impl Iterator<Item = &Path> {
        self.roots
            .iter()
            .filter(|r| r.node == NONE && r.file.is_none())
            .map(|r| r.path.as_path())
    }

    /// Brings it up to date with what `dirty` says changed, `watcher`
    /// seeing each directory it takes up or lets go of. Returns whether
    /// that took building it afresh.
    pub fn update_watched(
        &mut self,
        dirty: &Dirty,
        walker: &mut Walker,
        watcher: &mut dyn Watcher,
    ) -> bool {
        if dirty.all || self.over {
            self.build_watched(walker, watcher);
            return true;
        }
        self.updates = self.updates.wrapping_add(1);
        self.rebuild = Some(false);
        self.apply(dirty, walker, watcher);
        // A file it let go of a link to, and still holds, takes what was
        // last seen, unless this update saw it again: it may have been
        // written through the link that's gone.
        let updates = self.updates;
        let unsure = std::mem::take(&mut self.unsure);
        if unsure
            .iter()
            .any(|k| self.links.get(k).is_some_and(|e| e.seen != updates))
        {
            self.rebuild = Some(true);
        }
        // Let go of: it's seldom needed, and then briefly.
        drop(unsure);
        if self.rebuild.take() == Some(true) {
            self.build_watched(walker, watcher);
            return true;
        }
        false
    }

    fn apply(&mut self, dirty: &Dirty, walker: &mut Walker, watcher: &mut dyn Watcher) {
        // Shallower first, so a directory listed again has its new and gone
        // directories sorted out before anything under it is looked at.
        let mut todo: Vec<(&PathBuf, bool)> = dirty.dirs.iter().map(|(p, r)| (p, *r)).collect();
        todo.sort_by(|a, b| a.0.as_os_str().as_bytes().cmp(b.0.as_os_str().as_bytes()));
        // What's been walked whole already.
        let mut whole: Vec<PathBuf> = Vec::new();
        for (path, recursive) in todo {
            if self.rebuild == Some(true) || self.over {
                // Everything is walked afresh anyway, or let go of.
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
                self.drop_root(root, watcher);
                self.walk_root(root, walker, watcher);
                continue;
            }
            if recursive && exact {
                self.rewalk(root, node, walker, watcher);
                whole.push(path.clone());
            } else {
                self.relist(root, node, walker, watcher);
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
    fn walk_root(&mut self, i: usize, walker: &mut Walker, watcher: &mut dyn Watcher) {
        let path = self.roots[i].path.clone();
        let mut b = Builder {
            cache: self,
            watcher,
            stack: Vec::new(),
            parent: NONE,
            first: NONE,
            file: None,
            tag: 0,
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
    fn drop_root(&mut self, i: usize, watcher: &mut dyn Watcher) {
        let node = std::mem::replace(&mut self.roots[i].node, NONE);
        if node != NONE {
            self.remove(node, watcher);
        }
        match self.roots[i].file.take() {
            Some(s) if s.links > 1 => self.unlink((s.dev, s.ino)),
            Some(s) => self.total = self.total.saturating_sub(s.bytes),
            None => {}
        }
    }

    /// Walks again everything under `node`.
    fn rewalk(&mut self, root: usize, node: u32, walker: &mut Walker, watcher: &mut dyn Watcher) {
        let parent = self.nodes[node as usize].parent;
        let path = self.path_of(root, node);
        if parent == NONE {
            self.drop_root(root, watcher);
            self.walk_root(root, walker, watcher);
            return;
        }
        self.remove(node, watcher);
        if !self.walk_under(parent, &path, walker, watcher) {
            // Not a directory now: its parent counts what's there.
            self.relist(root, parent, walker, watcher);
        }
    }

    /// Walks the directory at `path`, new under `parent`, whole. Returns
    /// whether there was one.
    fn walk_under(
        &mut self,
        parent: u32,
        path: &Path,
        walker: &mut Walker,
        watcher: &mut dyn Watcher,
    ) -> bool {
        let mut b = Builder {
            cache: self,
            watcher,
            stack: Vec::new(),
            parent,
            first: NONE,
            file: None,
            tag: 0,
        };
        walker.walk(path, usize::MAX, &mut b);
        b.first != NONE
    }

    /// Lists `node` again, walking only what's new in it.
    fn relist(&mut self, root: usize, node: u32, walker: &mut Walker, watcher: &mut dyn Watcher) {
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
                self.drop_root(root, watcher);
                self.walk_root(root, walker, watcher);
            } else {
                self.relist(root, parent, walker, watcher);
            }
            return;
        };
        if listed.ino != self.nodes[node as usize].ino {
            // Another directory by the same name: none of it is known.
            self.rewalk(root, node, walker, watcher);
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
            self.remove(c, watcher);
        }
        for name in new {
            if self.over {
                return;
            }
            if let Some(c) = self.child(node, &name) {
                self.remove(c, watcher);
            }
            self.walk_under(node, &path.join(&name), walker, watcher);
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
        if self.links.len() >= self.max.1 && !self.links.contains_key(&l.key()) {
            self.over = true;
        }
        let e = self.links.entry(l.key()).or_default();
        if e.count == 0
            && let Some(rebuild) = &mut self.rebuild
        {
            *rebuild = true;
        }
        e.count += 1;
        self.total = self.total.saturating_sub(e.bytes).saturating_add(l.bytes);
        e.bytes = l.bytes;
        e.seen = self.updates;
        l.key()
    }

    fn unlink(&mut self, k: (u64, u64)) {
        if let Some(e) = self.links.get_mut(&k) {
            e.count -= 1;
            if e.count == 0 {
                self.total = self.total.saturating_sub(e.bytes);
                self.links.remove(&k);
            } else if self.rebuild.is_some() {
                self.unsure.push(k);
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
    fn remove(&mut self, node: u32, watcher: &mut dyn Watcher) {
        let parent = self.nodes[node as usize].parent;
        if parent != NONE {
            self.nodes[parent as usize].children.retain(|&c| c != node);
        }
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            let gone = std::mem::take(&mut self.nodes[n as usize]);
            if gone.tag != 0 {
                watcher.removed(gone.tag, n);
            }
            self.total = self.total.saturating_sub(gone.own);
            for k in gone.links {
                self.unlink(k);
            }
            stack.extend(gone.children);
            self.nodes[n as usize].parent = FREED;
            self.free.push(n);
        }
    }
}

#[cfg(test)]
impl Cache {
    /// Where it differs from a cache of the same roots built afresh, for a
    /// test that finds its total isn't what a walk finds.
    pub(crate) fn differences(&self) -> String {
        type Sizes = std::collections::BTreeMap<PathBuf, (u64, Vec<(u64, u64)>)>;
        fn sizes(c: &Cache) -> Sizes {
            let mut out = Sizes::new();
            for (i, n) in c.nodes.iter().enumerate() {
                if n.parent == FREED {
                    continue;
                }
                if let Some(p) = c.path(i as u32) {
                    let mut links = n.links.clone();
                    links.sort_unstable();
                    out.insert(p, (n.own, links));
                }
            }
            out
        }
        let mut fresh = Cache::new(self.roots.iter().map(|r| r.path.clone()).collect());
        fresh.build(&mut Walker::new());
        let (mine, theirs) = (sizes(self), sizes(&fresh));
        let mut out = String::new();
        for (p, v) in &theirs {
            if mine.get(p) != Some(v) {
                out += &format!("\n{}: kept {:?}, walked {v:?}", p.display(), mine.get(p));
            }
        }
        for p in mine.keys().filter(|p| !theirs.contains_key(*p)) {
            out += &format!("\n{}: kept, not there", p.display());
        }
        out
    }
}

/// Adds what a walk finds under `parent` to the cache.
struct Builder<'a> {
    cache: &'a mut Cache,
    watcher: &'a mut dyn Watcher,
    /// The directory last listed at each depth of the walk.
    stack: Vec<u32>,
    parent: u32,
    /// The walk's root, once listed.
    first: u32,
    /// The walk's root, when it isn't a directory.
    file: Option<Stat>,
    /// What the watcher tagged the directory being listed.
    tag: i32,
}

impl Visit for Builder<'_> {
    fn opening(&mut self, path: &Path, _: usize) {
        self.tag = if self.cache.over {
            0
        } else {
            self.watcher.watch(path)
        };
    }

    fn dir(&mut self, l: &Listing<'_>) -> bool {
        let tag = std::mem::take(&mut self.tag);
        if self.cache.over || self.cache.dirs() >= self.cache.max.0 {
            // More than it holds: what it has is let go of anyway, so
            // nothing more is walked. A watch already made for this one
            // goes with the watcher's others.
            self.cache.over = true;
            return false;
        }
        self.stack.truncate(l.depth);
        let parent = self.stack.last().copied().unwrap_or(self.parent);
        let node = self.cache.alloc(Node {
            parent,
            name: l.name().as_bytes().into(),
            ino: l.stat.ino,
            tag,
            ..Node::default()
        });
        if tag != 0 {
            self.watcher.added(tag, node);
        }
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
        if cache.total() != usage(roots) {
            panic!(
                "after {what}, kept {} but walked {}:{}",
                cache.total(),
                usage(roots),
                cache.differences()
            );
        }
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
    fn a_file_written_through_a_link_then_gone_isnt_left_as_it_was() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        fs::create_dir_all(a.join("one")).unwrap();
        fs::create_dir_all(a.join("two")).unwrap();
        fs::write(a.join("one/f"), vec![1u8; 50_000]).unwrap();
        fs::hard_link(a.join("one/f"), a.join("two/g")).unwrap();
        let roots = vec![a.clone()];
        let mut w = Walker::new();
        let mut c = Cache::new(roots.clone());
        c.build(&mut w);
        // Grown through one link, which then goes: only its directory is
        // reported, and the other still holds the file.
        {
            use std::io::Write;
            let mut f = fs::OpenOptions::new()
                .append(true)
                .open(a.join("two/g"))
                .unwrap();
            f.write_all(&vec![2u8; 400_000]).unwrap();
        }
        fs::remove_file(a.join("two/g")).unwrap();
        c.update(&dirty(&[(&a.join("two"), false)]), &mut w);
        check(&c, &roots, "a file grown through a link that's gone");
        // A link moved to another directory, both reported, needs nothing
        // walked afresh to stay exact.
        fs::hard_link(a.join("one/f"), a.join("two/h")).unwrap();
        c.update(&dirty(&[(&a.join("two"), false)]), &mut w);
        fs::rename(a.join("two/h"), a.join("h")).unwrap();
        c.update(&dirty(&[(&a.join("two"), false), (&a, false)]), &mut w);
        check(&c, &roots, "a link moved");
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
                match rng.below(7) {
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
                    // Appended to, through whichever link: reported as the
                    // directory of that link.
                    6 if !files.is_empty() => {
                        use std::io::Write;
                        let f = files[rng.below(files.len())].clone();
                        if let Ok(mut h) = fs::OpenOptions::new().append(true).open(&f) {
                            let _ = h.write_all(&vec![2u8; rng.below(300_000)]);
                            d.mark(f.parent().unwrap(), false);
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
