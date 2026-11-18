//! What an agent's worktree and temp dir take on disk, for `disk_soft` and
//! `disk_hard` (PRD: How macos is governed; How agents run on a Linux
//! host), measured so an agent that can't have changed them costs nothing.
//!
//! - **Only while it runs.** A frozen agent can't write, so an agent asleep
//!   or paused isn't measured: it keeps what it last measured, and a wake
//!   measures it afresh.
//! - **One worker per backend.** A [`Meter`] has one thread, while any
//!   agent is tracked, that measures each agent in turn when it's due. Its
//!   [`Walker`] keeps its buffers from walk to walk, and allocates nothing
//!   per file.
//! - **When something may have changed, and only where.** Each agent's
//!   [`Source`] says whether its trees may have changed since it last
//!   asked. A [`Cached`] source keeps what they take a directory at a time
//!   ([`Cache`]), and is told which directories changed: by FSEvents on a
//!   Mac, by inotify on Linux ([`inotify`], a watch for each directory). A
//!   change is measured by listing again only the directories it touched.
//!   A tree too big to keep that way ([`cache::MAX_DIRS`]), or one inotify
//!   runs out of watches for, is walked instead, when its [`Signal`] says
//!   it may have changed: on a Linux host, whether any of the agent's
//!   processes wrote anything (its cgroup's `io.stat` where the `io`
//!   controller is delegated, and each process's `/proc/<pid>/io`); on a
//!   Mac, FSEvents still. Without either, every measurement may have
//!   changed.
//! - **As often as it's due**, by [`Policy`]: at most every
//!   [`Policy::interval`] (a minute) while something may have changed; a
//!   full walk every [`RECONCILE`] intervals whatever the source says, for
//!   what no source sees (a process outside the agent writing into its
//!   tree, say); and, while the filesystem has less than
//!   [`vibe_core::limits::MIN_FREE_DISK`] free, [`TIGHT`] times an
//!   interval (every 15 s), change or not. The full walk of a cache checks
//!   it: a cache that agrees is kept as it is, and only one that doesn't is
//!   built afresh.
//! - **Holding little.** A frozen agent's cache, and its watches, are let
//!   go of, and its wake builds them afresh. A cache holds at most
//!   [`cache::MAX_DIRS`] directories. The worker lets go of its walker's
//!   buffers and waits without a timer while no agent runs, and a look that
//!   finds nothing changed allocates nothing.

pub mod cache;
#[cfg(target_os = "macos")]
pub mod fsevents;
#[cfg(target_os = "linux")]
pub mod inotify;
pub mod walk;

use std::ffi::{CStr, CString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

pub use cache::{Cache, Dirty, NoWatch, Watcher};
pub use walk::{Walker, usage};

/// A full walk is due after this many intervals, whatever the source says.
pub const RECONCILE: u32 = 10;

/// While space is short, a measurement is due this many times an interval.
pub const TIGHT: u32 = 4;

/// The worker looks at each agent at least this often, to ask its source
/// what changed.
const LOOK: Duration = Duration::from_secs(2);

/// What a measurement does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Brings the last one up to date with what changed, where the source
    /// can; otherwise measures afresh.
    Update,
    /// Measures afresh.
    Full,
}

/// When an agent that runs is measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// The least time between measurements while something may have
    /// changed.
    pub interval: Duration,
}

impl Policy {
    pub fn reconcile(&self) -> Duration {
        self.interval * RECONCILE
    }

    /// What's due, if anything, given how long it's been since the last
    /// measurement and the last full one (none since a launch or a wake),
    /// whether the trees may have changed since the last measurement
    /// (`None`: no way to tell), and whether the filesystem is short of
    /// space.
    pub fn due(
        &self,
        since: Option<Duration>,
        since_full: Option<Duration>,
        changed: Option<bool>,
        tight: bool,
    ) -> Option<Kind> {
        let (Some(since), Some(since_full)) = (since, since_full) else {
            return Some(Kind::Full);
        };
        if since_full >= self.reconcile() {
            return Some(Kind::Full);
        }
        let every = if tight {
            self.interval / TIGHT
        } else if changed == Some(false) {
            return None;
        } else {
            self.interval
        };
        (since >= every).then_some(Kind::Update)
    }
}

/// Whether the filesystem `path` is on has less than
/// [`vibe_core::limits::MIN_FREE_DISK`] free.
pub fn tight(path: &Path) -> bool {
    c_path(path).is_some_and(|c| tight_at(&c))
}

fn tight_at(path: &CStr) -> bool {
    free_space(path).is_some_and(|free| free < vibe_core::limits::MIN_FREE_DISK)
}

fn c_path(path: &Path) -> Option<CString> {
    CString::new(path.as_os_str().as_encoded_bytes()).ok()
}

fn free_space(path: &CStr) -> Option<u64> {
    // SAFETY: a NUL-terminated path, and plain data for statvfs to fill.
    unsafe {
        let mut st = std::mem::zeroed::<libc::statvfs>();
        if libc::statvfs(path.as_ptr(), &mut st) != 0 {
            return None;
        }
        #[allow(clippy::useless_conversion)]
        Some(u64::from(st.f_bavail).saturating_mul(u64::from(st.f_frsize)))
    }
}

/// How an agent's trees are measured, and how it's told they changed.
pub trait Source: Send + fmt::Debug {
    /// Whether they may have changed since it was last asked; `None` if it
    /// can't tell.
    fn changed(&mut self) -> Option<bool>;

    /// What they take now.
    fn measure(&mut self, kind: Kind, walker: &mut Walker) -> u64;

    /// The agent is frozen: let go of what it won't need until a wake.
    fn rest(&mut self) {}

    /// A launch or a wake: nothing measured before counts.
    fn reset(&mut self) {}
}

/// Says whether an agent wrote anything.
pub trait Signal: Send + fmt::Debug {
    /// Whether it wrote anything since it was last asked; `None` if it
    /// can't tell. The first answer after a [`Signal::reset`] is the
    /// baseline.
    fn wrote(&mut self) -> Option<bool>;

    fn reset(&mut self) {}
}

/// Says which directories under a [`Cached`] source's trees changed.
pub trait Changes: Send + fmt::Debug {
    /// Adds to `dirty` what changed since it was last asked, placed by
    /// `cache`, the one kept from it, if any. Returns whether it can tell:
    /// not once it's let go of what it needs to.
    fn take(&mut self, cache: Option<&Cache>, dirty: &mut Dirty) -> bool;

    /// What sees each directory the cache holds.
    fn watcher(&mut self) -> &mut dyn Watcher;

    /// Whether it can't see every directory the cache holds (it ran out of
    /// watches), so the cache can't be kept.
    fn failed(&self) -> bool {
        false
    }

    /// The cache is let go of: so is whatever only it needed.
    fn release(&mut self) {}
}

/// Measures from a [`Cache`], told what changed by its [`Changes`], or by
/// walking, when its [`Signal`] says to, once the cache can't be kept.
#[derive(Debug)]
pub struct Cached {
    roots: Vec<PathBuf>,
    changes: Option<Box<dyn Changes>>,
    /// What says when to walk, while there's no cache.
    signal: Option<Box<dyn Signal>>,
    cache: Option<Cache>,
    dirty: Dirty,
    /// Whether the cache was let go of until the next reset, as the tree
    /// is more than it holds, or can't be watched whole.
    walking: bool,
    max: (usize, usize),
    /// How many times it built its cache.
    builds: u64,
}

impl Cached {
    /// Measures `roots` from a cache that `changes` keeps up to date, or
    /// with none (`changes` is `None`, or the cache can't be kept) by a walk
    /// when `signal` says.
    pub fn new(
        roots: Vec<PathBuf>,
        changes: Option<Box<dyn Changes>>,
        signal: Option<Box<dyn Signal>>,
    ) -> Cached {
        Cached {
            roots,
            changes,
            signal,
            cache: None,
            dirty: Dirty::default(),
            walking: false,
            max: (cache::MAX_DIRS, cache::MAX_LINKS),
            builds: 0,
        }
    }

    /// Keeps a cache of no more than `dirs` directories and `links` files
    /// with several links.
    pub fn with_max(mut self, dirs: usize, links: usize) -> Cached {
        self.max = (dirs, links);
        self
    }

    /// Whether it measures from a cache now.
    pub fn caching(&self) -> bool {
        self.cache.is_some()
    }

    /// Whether it gave up its cache until the next reset.
    pub fn walking(&self) -> bool {
        self.walking
    }

    /// How many times it built its cache from a walk.
    pub fn builds(&self) -> u64 {
        self.builds
    }

    /// Lets go of the cache until the next reset, and walks.
    fn give_up(&mut self, walker: &mut Walker) -> u64 {
        let why = match &self.cache {
            Some(c) if c.over() => "more than a cache holds",
            _ => "it can't be watched whole",
        };
        eprintln!(
            "vibe-env: {} is walked when it changes rather than kept a directory at a time: {why}",
            self.roots
                .first()
                .map(|r| r.display().to_string())
                .unwrap_or_default()
        );
        self.cache = None;
        self.dirty.clear();
        self.walking = true;
        if let Some(c) = &mut self.changes {
            c.release();
        }
        if let Some(s) = &mut self.signal {
            s.reset();
            // The baseline, from before this walk.
            s.wrote();
        }
        walker.usage(&self.roots)
    }
}

impl Source for Cached {
    fn changed(&mut self) -> Option<bool> {
        let cache = if self.walking {
            None
        } else {
            self.cache.as_ref()
        };
        if let Some(c) = &mut self.changes
            && c.take(cache, &mut self.dirty)
        {
            return Some(!self.dirty.is_empty());
        }
        self.signal.as_mut()?.wrote()
    }

    fn measure(&mut self, kind: Kind, walker: &mut Walker) -> u64 {
        let Some(changes) = self.changes.as_mut().filter(|_| !self.walking) else {
            self.dirty.clear();
            return walker.usage(&self.roots);
        };
        changes.take(self.cache.as_ref(), &mut self.dirty);
        let dirty = std::mem::take(&mut self.dirty);
        let mut walked = None;
        let cache = match (&mut self.cache, kind) {
            (Some(c), Kind::Update) => {
                c.update_watched(&dirty, walker, changes.watcher());
                c
            }
            (Some(c), Kind::Full) if !c.sparse() => {
                // Checked by a walk, which allocates nothing: kept as it is
                // if it agrees, built afresh if not.
                c.update_watched(&dirty, walker, changes.watcher());
                if !c.over() && !changes.failed() {
                    let w = walker.usage(&self.roots);
                    if w == c.total() {
                        walked = Some(w);
                    } else {
                        c.build_watched(walker, changes.watcher());
                        self.builds += 1;
                    }
                }
                c
            }
            (cache, _) => {
                let (roots, (dirs, links)) = (self.roots.clone(), self.max);
                let c = cache.insert(Cache::new(roots).with_max(dirs, links));
                c.build_watched(walker, changes.watcher());
                self.builds += 1;
                c
            }
        };
        if cache.over() || changes.failed() {
            return self.give_up(walker);
        }
        walked.unwrap_or(cache.total())
    }

    fn rest(&mut self) {
        self.cache = None;
        self.dirty.clear();
        if let Some(c) = &mut self.changes {
            c.release();
        }
    }

    fn reset(&mut self) {
        self.rest();
        self.walking = false;
        if let Some(s) = &mut self.signal {
            s.reset();
        }
    }
}

/// FSEvents, as a [`Cached`] source's [`Changes`].
#[cfg(target_os = "macos")]
#[derive(Debug)]
pub struct FsEvents {
    watch: fsevents::Watch,
    none: NoWatch,
}

#[cfg(target_os = "macos")]
impl Changes for FsEvents {
    fn take(&mut self, _: Option<&Cache>, dirty: &mut Dirty) -> bool {
        dirty.merge(self.watch.take());
        true
    }

    fn watcher(&mut self) -> &mut dyn Watcher {
        &mut self.none
    }

    fn release(&mut self) {
        self.watch.take();
    }
}

/// The source that measures `roots` best here: from a cache that FSEvents
/// (on a Mac, gathering changes for up to `latency`) or inotify (on Linux)
/// keeps up to date, walking when `signal` says if that can't be done.
pub fn watched(roots: Vec<PathBuf>, latency: Duration, signal: Option<Box<dyn Signal>>) -> Cached {
    #[cfg(target_os = "macos")]
    {
        let roots: Vec<PathBuf> = roots.iter().map(|r| real(r)).collect();
        match fsevents::Watch::new(&roots, latency) {
            Ok(watch) => {
                let changes = FsEvents {
                    watch,
                    none: NoWatch,
                };
                Cached::new(roots, Some(Box::new(changes)), signal)
            }
            Err(e) => {
                eprintln!(
                    "vibe-env: can't watch {} with FSEvents, so it's walked instead: {e}",
                    roots
                        .first()
                        .map(|r| r.display().to_string())
                        .unwrap_or_default()
                );
                Cached::new(roots, None, signal)
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        let _ = latency;
        Cached::new(roots, Some(Box::new(inotify::Inotify::new())), signal)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = latency;
        Cached::new(roots, None, signal)
    }
}

/// `path` with its parent's symbolic links resolved, as FSEvents reports
/// paths, but not its own: a root that's a link is measured as a link.
#[cfg(target_os = "macos")]
fn real(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => parent
            .canonicalize()
            .map(|p| p.join(name))
            .unwrap_or_else(|_| path.to_owned()),
        _ => path.to_owned(),
    }
}

/// What a backend measures its agents with: one worker, started with the
/// first agent tracked and ended once none is.
#[derive(Debug, Clone)]
pub struct Meter {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    policy: Policy,
    tracks: Mutex<Tracks>,
    /// Woken when a track is new or reset, to measure it now.
    nudge: Condvar,
}

#[derive(Debug, Default)]
struct Tracks {
    list: Vec<Weak<Track>>,
    working: bool,
    nudged: bool,
}

/// One agent's measurements, while it's tracked: its backend says whether
/// it runs and when it's woken, and reads what was last measured. Dropped,
/// the worker lets go of it.
#[derive(Debug)]
pub struct Track {
    /// Where to ask how much space there is.
    space: Option<CString>,
    public: Mutex<Public>,
    inner: Mutex<Inner>,
    meter: Weak<Shared>,
}

#[derive(Debug, Default)]
struct Public {
    running: bool,
    /// Bumped by a reset: a measurement begun before one doesn't count.
    generation: u64,
    bytes: Option<u64>,
}

/// What only the worker touches.
#[derive(Debug)]
struct Inner {
    source: Box<dyn Source>,
    generation: u64,
    last: Option<Instant>,
    last_full: Option<Instant>,
    /// Whether the source said its trees changed since the last
    /// measurement began.
    changed: bool,
    /// Whether the source can tell.
    knows: bool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Meter {
    /// Measures each agent at least `interval` apart while it may have
    /// changed (see [`Policy`]).
    pub fn new(interval: Duration) -> Meter {
        Meter {
            shared: Arc::new(Shared {
                policy: Policy { interval },
                tracks: Mutex::new(Tracks::default()),
                nudge: Condvar::new(),
            }),
        }
    }

    /// Tracks an agent whose trees are measured by `source`, on the
    /// filesystem `space` is on. It isn't measured until it's said to run.
    pub fn track(&self, space: PathBuf, source: Box<dyn Source>) -> Arc<Track> {
        let track = Arc::new(Track {
            space: c_path(&space),
            public: Mutex::new(Public::default()),
            inner: Mutex::new(Inner {
                source,
                generation: 0,
                last: None,
                last_full: None,
                changed: false,
                knows: false,
            }),
            meter: Arc::downgrade(&self.shared),
        });
        let mut tracks = lock(&self.shared.tracks);
        tracks.list.push(Arc::downgrade(&track));
        if !tracks.working {
            let shared = Arc::clone(&self.shared);
            let spawned = std::thread::Builder::new()
                .name("disk".into())
                .spawn(move || work(&shared));
            match spawned {
                Ok(_) => tracks.working = true,
                Err(e) => eprintln!("vibe-env: can't start measuring disks: {e}"),
            }
        }
        drop(tracks);
        track
    }
}

impl Track {
    /// Says whether the agent runs now. Only then is it measured.
    pub fn set_running(&self, running: bool) {
        let was = std::mem::replace(&mut lock(&self.public).running, running);
        if running && !was {
            self.nudge();
        }
    }

    /// A launch or a wake: what was measured before doesn't count, and it's
    /// measured again as soon as it runs.
    pub fn reset(&self) {
        {
            let mut p = lock(&self.public);
            p.generation += 1;
            p.bytes = None;
        }
        self.nudge();
    }

    /// What it last measured, since the last reset.
    pub fn bytes(&self) -> Option<u64> {
        lock(&self.public).bytes
    }

    fn nudge(&self) {
        if let Some(shared) = self.meter.upgrade() {
            lock(&shared.tracks).nudged = true;
            shared.nudge.notify_all();
        }
    }

    /// Asks its source what changed, and measures it if that's due.
    /// Returns whether it runs.
    fn step(&self, policy: &Policy, walker: &mut Walker) -> bool {
        let (running, generation) = {
            let p = lock(&self.public);
            (p.running, p.generation)
        };
        let mut inner = lock(&self.inner);
        if inner.generation != generation {
            inner.source.reset();
            inner.generation = generation;
            inner.last = None;
            inner.last_full = None;
            inner.changed = false;
        }
        if !running {
            inner.source.rest();
            return false;
        }
        let changed = inner.source.changed();
        inner.knows = changed.is_some();
        inner.changed |= changed == Some(true);
        let now = Instant::now();
        let kind = policy.due(
            inner.last.map(|t| now - t),
            inner.last_full.map(|t| now - t),
            inner.knows.then_some(inner.changed),
            self.space.as_deref().is_some_and(tight_at),
        );
        let Some(kind) = kind else {
            return true;
        };
        inner.changed = false;
        let bytes = inner.source.measure(kind, walker);
        inner.last = Some(now);
        if kind == Kind::Full {
            inner.last_full = Some(now);
        }
        let mut p = lock(&self.public);
        if p.generation == generation {
            p.bytes = Some(bytes);
        }
        true
    }
}

impl Drop for Track {
    /// Wakes the worker, which may be waiting for an agent to run, to let
    /// go of it, and to end if it was the last.
    fn drop(&mut self) {
        self.nudge();
    }
}

/// The worker: each agent in turn, then a rest until the next look or a
/// nudge, for as long as any agent is tracked. While none runs it waits for
/// a nudge alone, holding no walker's buffers.
fn work(shared: &Shared) {
    let mut walker = Walker::new();
    let policy = shared.policy;
    let look = LOOK
        .min(policy.interval / TIGHT)
        .max(Duration::from_millis(10));
    let mut live: Vec<Arc<Track>> = Vec::new();
    loop {
        {
            let mut tracks = lock(&shared.tracks);
            tracks.list.retain(|t| t.strong_count() > 0);
            if tracks.list.is_empty() {
                tracks.working = false;
                return;
            }
            tracks.nudged = false;
            live.extend(tracks.list.iter().filter_map(Weak::upgrade));
        }
        let mut running = false;
        for t in &live {
            running |= t.step(&policy, &mut walker);
        }
        // Outside the lock, as the last of a track ends here and nudges.
        live.clear();
        if !running {
            walker = Walker::new();
        }
        let tracks = lock(&shared.tracks);
        if !tracks.nudged {
            if running {
                let _ = shared
                    .nudge
                    .wait_timeout(tracks, look)
                    .unwrap_or_else(|e| e.into_inner());
            } else {
                drop(shared.nudge.wait(tracks).unwrap_or_else(|e| e.into_inner()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    const MINUTE: Duration = Duration::from_secs(60);

    #[test]
    fn the_policy() {
        let p = Policy { interval: MINUTE };
        let s = |n: u64| Some(Duration::from_secs(n));
        // Nothing measured since a launch or a wake: a full walk, whatever
        // else.
        assert_eq!(p.due(None, None, Some(false), false), Some(Kind::Full));
        assert_eq!(p.due(s(5), None, None, false), Some(Kind::Full));
        // Changed, or no way to tell: once an interval.
        assert_eq!(p.due(s(59), s(59), Some(true), false), None);
        assert_eq!(p.due(s(60), s(60), Some(true), false), Some(Kind::Update));
        assert_eq!(p.due(s(60), s(200), None, false), Some(Kind::Update));
        // Unchanged: nothing, until a reconcile is due.
        assert_eq!(p.due(s(300), s(300), Some(false), false), None);
        assert_eq!(p.due(s(599), s(599), Some(false), false), None);
        assert_eq!(p.due(s(10), s(600), Some(false), false), Some(Kind::Full));
        // Short of space: four times an interval, changed or not.
        assert_eq!(p.due(s(14), s(14), Some(false), true), None);
        assert_eq!(p.due(s(15), s(15), Some(false), true), Some(Kind::Update));
        assert_eq!(p.due(s(15), s(15), Some(true), true), Some(Kind::Update));
    }

    #[test]
    fn space_is_asked_of_the_filesystem() {
        let tmp = tempfile::tempdir().unwrap();
        let at = |p: &Path| free_space(&c_path(p).unwrap());
        assert!(at(tmp.path()).is_some_and(|f| f > 0));
        assert!(at(Path::new("/no/such/place")).is_none());
        // Unknown isn't short.
        assert!(!tight(Path::new("/no/such/place")));
    }

    /// A source that counts what it's asked, and changes when told to.
    #[derive(Debug, Default)]
    struct Fake {
        measured: AtomicUsize,
        full: AtomicUsize,
        rests: AtomicUsize,
        resets: AtomicUsize,
        changes: AtomicBool,
        knows: AtomicBool,
        bytes: AtomicU64,
    }

    #[derive(Debug)]
    struct FakeSource(Arc<Fake>);

    impl Source for FakeSource {
        fn changed(&mut self) -> Option<bool> {
            self.0
                .knows
                .load(Ordering::SeqCst)
                .then(|| self.0.changes.load(Ordering::SeqCst))
        }
        fn measure(&mut self, kind: Kind, _: &mut Walker) -> u64 {
            self.0.measured.fetch_add(1, Ordering::SeqCst);
            if kind == Kind::Full {
                self.0.full.fetch_add(1, Ordering::SeqCst);
            }
            self.0.bytes.load(Ordering::SeqCst)
        }
        fn rest(&mut self) {
            self.0.rests.fetch_add(1, Ordering::SeqCst);
        }
        fn reset(&mut self) {
            self.0.resets.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn until(what: &str, mut ok: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ok() {
            assert!(Instant::now() < deadline, "no {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn measured(f: &Fake) -> usize {
        f.measured.load(Ordering::SeqCst)
    }

    #[test]
    fn only_a_running_agent_is_measured_and_a_wake_measures_afresh() {
        let interval = Duration::from_millis(40);
        let meter = Meter::new(interval);
        let fake = Arc::new(Fake::default());
        fake.bytes.store(1234, Ordering::SeqCst);
        let tmp = tempfile::tempdir().unwrap();
        let track = meter.track(
            tmp.path().to_owned(),
            Box::new(FakeSource(Arc::clone(&fake))),
        );
        // Not said to run: never measured.
        std::thread::sleep(interval * 5);
        assert_eq!(measured(&fake), 0);
        assert_eq!(track.bytes(), None);
        assert!(fake.rests.load(Ordering::SeqCst) > 0);

        // Running, with no way to tell what changed: measured at once, then
        // once an interval.
        track.set_running(true);
        until("a first measurement", || track.bytes() == Some(1234));
        assert_eq!(fake.full.load(Ordering::SeqCst), 1);
        until("more measurements", || measured(&fake) >= 3);

        // Frozen: it keeps the last, and isn't measured however long.
        track.set_running(false);
        std::thread::sleep(interval * 2);
        let frozen_at = measured(&fake);
        fake.bytes.store(99, Ordering::SeqCst);
        std::thread::sleep(interval * 8);
        assert_eq!(measured(&fake), frozen_at);
        assert_eq!(track.bytes(), Some(1234));

        // Woken: what it had doesn't count, and it's measured afresh at
        // once.
        track.reset();
        assert_eq!(track.bytes(), None);
        track.set_running(true);
        until("a fresh measurement", || track.bytes() == Some(99));
        assert!(fake.resets.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn a_source_that_saw_nothing_change_waits_for_a_reconcile() {
        let interval = Duration::from_millis(30);
        let meter = Meter::new(interval);
        let fake = Arc::new(Fake::default());
        fake.knows.store(true, Ordering::SeqCst);
        let tmp = tempfile::tempdir().unwrap();
        let track = meter.track(
            tmp.path().to_owned(),
            Box::new(FakeSource(Arc::clone(&fake))),
        );
        track.set_running(true);
        until("a first measurement", || measured(&fake) == 1);
        // Unchanged for several intervals: nothing more.
        std::thread::sleep(interval * 5);
        assert_eq!(measured(&fake), 1);
        // A change is measured within an interval or so.
        fake.changes.store(true, Ordering::SeqCst);
        until("a measurement of the change", || measured(&fake) >= 2);
        fake.changes.store(false, Ordering::SeqCst);
        // And unchanged again, a full walk comes with the reconcile.
        let full = fake.full.load(Ordering::SeqCst);
        until("a reconcile", || fake.full.load(Ordering::SeqCst) > full);
    }

    #[test]
    fn the_worker_ends_with_the_last_track() {
        let meter = Meter::new(Duration::from_millis(20));
        let tmp = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let track = meter.track(
            tmp.path().to_owned(),
            Box::new(FakeSource(Arc::clone(&fake))),
        );
        track.set_running(true);
        until("a measurement", || measured(&fake) > 0);
        drop(track);
        until("the worker to end", || !lock(&meter.shared.tracks).working);
        // And starts again with the next.
        let again = meter.track(
            tmp.path().to_owned(),
            Box::new(FakeSource(Arc::clone(&fake))),
        );
        again.set_running(true);
        let before = measured(&fake);
        until("a measurement by a new worker", || measured(&fake) > before);
    }

    #[test]
    fn the_worker_waits_without_a_timer_while_no_agent_runs() {
        let interval = Duration::from_millis(20);
        let meter = Meter::new(interval);
        let fake = Arc::new(Fake::default());
        let tmp = tempfile::tempdir().unwrap();
        let track = meter.track(
            tmp.path().to_owned(),
            Box::new(FakeSource(Arc::clone(&fake))),
        );
        until("a first look", || fake.rests.load(Ordering::SeqCst) > 0);
        // A look every few milliseconds would rest it dozens of times over.
        std::thread::sleep(interval * 10);
        assert_eq!(fake.rests.load(Ordering::SeqCst), 1);
        // Running, it's measured at once.
        track.set_running(true);
        until("a measurement", || measured(&fake) > 0);
        // Frozen again, it waits again; dropped, the worker ends, waiting
        // or not.
        track.set_running(false);
        std::thread::sleep(interval * 4);
        let rests = fake.rests.load(Ordering::SeqCst);
        std::thread::sleep(interval * 10);
        assert_eq!(fake.rests.load(Ordering::SeqCst), rests);
        drop(track);
        until("the worker to end", || !lock(&meter.shared.tracks).working);
    }

    /// Says nothing ever changed, so a cache kept from it goes stale; and,
    /// as inotify, can't tell anything without one.
    #[derive(Debug, Default)]
    struct Deaf(NoWatch);

    impl Changes for Deaf {
        fn take(&mut self, cache: Option<&Cache>, _: &mut Dirty) -> bool {
            cache.is_some()
        }
        fn watcher(&mut self) -> &mut dyn Watcher {
            &mut self.0
        }
    }

    #[test]
    fn a_full_measurement_keeps_a_cache_that_agrees_and_rebuilds_one_that_doesnt() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("r");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/f"), vec![1u8; 100_000]).unwrap();
        let roots = vec![root.clone()];
        let mut w = Walker::new();
        let mut c = Cached::new(roots.clone(), Some(Box::new(Deaf::default())), None);
        assert_eq!(c.measure(Kind::Full, &mut w), usage(&roots));
        assert_eq!(c.builds(), 1);
        // Unchanged, a full measurement is a walk that agrees: the cache is
        // kept as it is.
        assert_eq!(c.measure(Kind::Full, &mut w), usage(&roots));
        assert_eq!(c.builds(), 1);
        // Changed where nothing said, an update can't know, but a full
        // measurement finds it and builds afresh.
        std::fs::write(root.join("a/g"), vec![1u8; 300_000]).unwrap();
        assert!(c.measure(Kind::Update, &mut w) < usage(&roots));
        assert_eq!(c.measure(Kind::Full, &mut w), usage(&roots));
        assert_eq!(c.builds(), 2);
        assert_eq!(c.measure(Kind::Update, &mut w), usage(&roots));
    }

    /// Says when told to.
    #[derive(Debug, Clone, Default)]
    struct Told(Arc<AtomicBool>);

    impl Signal for Told {
        fn wrote(&mut self) -> Option<bool> {
            Some(self.0.swap(false, Ordering::SeqCst))
        }
    }

    #[test]
    fn a_tree_bigger_than_a_cache_is_walked_when_its_signal_says_until_a_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("r");
        for i in 0..30 {
            std::fs::create_dir_all(root.join(format!("d{i}/e"))).unwrap();
            std::fs::write(root.join(format!("d{i}/e/f")), vec![1u8; 10_000]).unwrap();
        }
        let roots = vec![root.clone()];
        let told = Told::default();
        let mut w = Walker::new();
        let mut c = Cached::new(
            roots.clone(),
            Some(Box::new(Deaf::default())),
            Some(Box::new(told.clone())),
        )
        .with_max(20, 100);
        // More directories than it holds: measured by a walk, exactly, and
        // with no cache kept.
        assert_eq!(c.measure(Kind::Full, &mut w), usage(&roots));
        assert!(c.walking() && !c.caching());
        // Its signal says when to walk now.
        assert_eq!(c.changed(), Some(false));
        told.0.store(true, Ordering::SeqCst);
        assert_eq!(c.changed(), Some(true));
        std::fs::write(root.join("new"), vec![1u8; 70_000]).unwrap();
        assert_eq!(c.measure(Kind::Update, &mut w), usage(&roots));
        assert!(!c.caching());
        // A reset tries a cache again: smaller now, it's kept.
        for i in 0..25 {
            std::fs::remove_dir_all(root.join(format!("d{i}"))).unwrap();
        }
        c.reset();
        assert!(!c.walking());
        assert_eq!(c.measure(Kind::Full, &mut w), usage(&roots));
        assert!(c.caching());
        // So many files with several links are more than it holds too.
        for i in 0..150 {
            std::fs::write(root.join(format!("m{i}")), b"x").unwrap();
            std::fs::hard_link(root.join(format!("m{i}")), root.join(format!("n{i}"))).unwrap();
        }
        c.reset();
        assert_eq!(c.measure(Kind::Full, &mut w), usage(&roots));
        assert!(c.walking());
    }

    /// The source a backend gets here, kept up to date by what this OS
    /// reports (FSEvents on a Mac, inotify on Linux): it stays what a walk
    /// finds, and a frozen agent's lets go of its cache.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn the_native_source_keeps_up_with_changes_and_lets_go_when_frozen() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        std::fs::create_dir_all(a.join("x")).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let roots = vec![a.clone(), b.clone()];
        let mut src = watched(roots.clone(), Duration::from_millis(50), None);
        let mut w = Walker::new();
        assert_eq!(src.measure(Kind::Full, &mut w), usage(&roots));
        assert!(src.caching());
        // FSEvents starts from now: give it a moment to.
        std::thread::sleep(Duration::from_millis(300));
        while src.changed() == Some(true) {
            src.measure(Kind::Update, &mut w);
        }
        let builds = src.builds();
        let steps: [&dyn Fn(); 4] = [
            &|| std::fs::write(a.join("x/f"), vec![1u8; 200_000]).unwrap(),
            &|| {
                std::fs::create_dir_all(b.join("p/q")).unwrap();
                std::fs::write(b.join("p/q/g"), vec![1u8; 90_000]).unwrap();
            },
            &|| std::fs::rename(b.join("p"), a.join("x/p")).unwrap(),
            &|| std::fs::remove_dir_all(a.join("x/p")).unwrap(),
        ];
        for (i, step) in steps.iter().enumerate() {
            step();
            let want = usage(&roots);
            until(&format!("step {i} measured"), || {
                src.changed() == Some(true) && src.measure(Kind::Update, &mut w) == want
            });
        }
        // Kept up to date, not built again.
        assert_eq!(src.builds(), builds);
        // Frozen, it holds no cache; woken, it's built afresh.
        src.rest();
        assert!(!src.caching());
        std::fs::write(b.join("while-frozen"), vec![1u8; 40_000]).unwrap();
        src.reset();
        assert_eq!(src.measure(Kind::Full, &mut w), usage(&roots));
        assert!(src.caching());
    }

    #[test]
    fn a_source_with_nothing_to_say_what_changed_walks() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f"), vec![1u8; 100_000]).unwrap();
        let roots = vec![tmp.path().to_owned()];
        let mut w = Cached::new(roots.clone(), None, None);
        assert_eq!(w.changed(), None);
        assert_eq!(w.measure(Kind::Update, &mut Walker::new()), usage(&roots));
        assert!(!w.caching());
    }
}
