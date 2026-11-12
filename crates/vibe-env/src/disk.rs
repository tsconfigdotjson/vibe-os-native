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
//! - **When something may have changed.** Each agent's [`Source`] says
//!   whether its trees may have changed since it last asked. On a Linux
//!   host that's whether any of the agent's processes wrote anything (its
//!   cgroup's `io.stat` where the `io` controller is delegated, and each
//!   process's `/proc/<pid>/io`), and a change is measured by a walk. On a
//!   Mac it's FSEvents, and a change is measured by listing again only the
//!   directories it touched, from a [`Cache`] kept a directory at a time.
//!   Without either, every measurement may have changed.
//! - **As often as it's due**, by [`Policy`]: at most every
//!   [`Policy::interval`] (a minute) while something may have changed; a
//!   full walk every [`RECONCILE`] intervals whatever the source says, for
//!   what no source sees (a process outside the agent writing into its
//!   tree, say); and, while the filesystem has less than
//!   [`vibe_core::limits::MIN_FREE_DISK`] free, [`TIGHT`] times an
//!   interval (every 15 s), change or not.

pub mod cache;
#[cfg(target_os = "macos")]
pub mod fsevents;
pub mod walk;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

pub use cache::{Cache, Dirty};
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
    free_space(path).is_some_and(|free| free < vibe_core::limits::MIN_FREE_DISK)
}

fn free_space(path: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: a NUL-terminated path, and plain data for statvfs to fill.
    unsafe {
        let mut st = std::mem::zeroed::<libc::statvfs>();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
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

/// Measures by walking every tree, when a [`Signal`] (if any) says
/// something may have changed.
#[derive(Debug)]
pub struct Walk {
    roots: Vec<PathBuf>,
    signal: Option<Box<dyn Signal>>,
}

impl Walk {
    pub fn new(roots: Vec<PathBuf>, signal: Option<Box<dyn Signal>>) -> Walk {
        Walk { roots, signal }
    }
}

impl Source for Walk {
    fn changed(&mut self) -> Option<bool> {
        self.signal.as_mut()?.wrote()
    }

    fn measure(&mut self, _: Kind, walker: &mut Walker) -> u64 {
        walker.usage(&self.roots)
    }

    fn reset(&mut self) {
        if let Some(s) = &mut self.signal {
            s.reset();
        }
    }
}

/// Measures from a [`Cache`], told what changed by FSEvents.
#[cfg(target_os = "macos")]
#[derive(Debug)]
pub struct Cached {
    roots: Vec<PathBuf>,
    watch: fsevents::Watch,
    cache: Option<Cache>,
    dirty: Dirty,
}

#[cfg(target_os = "macos")]
impl Cached {
    /// Watches `roots` with FSEvents, gathering changes for up to
    /// `latency`.
    pub fn new(roots: &[PathBuf], latency: Duration) -> std::io::Result<Cached> {
        let roots: Vec<PathBuf> = roots.iter().map(|r| real(r)).collect();
        let watch = fsevents::Watch::new(&roots, latency)?;
        Ok(Cached {
            roots,
            watch,
            cache: None,
            dirty: Dirty::default(),
        })
    }
}

#[cfg(target_os = "macos")]
impl Source for Cached {
    fn changed(&mut self) -> Option<bool> {
        self.dirty.merge(self.watch.take());
        Some(!self.dirty.is_empty())
    }

    fn measure(&mut self, kind: Kind, walker: &mut Walker) -> u64 {
        self.dirty.merge(self.watch.take());
        let dirty = std::mem::take(&mut self.dirty);
        let cache = match (&mut self.cache, kind) {
            (Some(c), Kind::Update) => {
                c.update(&dirty, walker);
                c
            }
            (cache, _) => {
                let c = cache.insert(Cache::new(self.roots.clone()));
                c.build(walker);
                c
            }
        };
        cache.total()
    }

    fn rest(&mut self) {
        self.cache = None;
        self.watch.take();
        self.dirty.clear();
    }

    fn reset(&mut self) {
        self.rest();
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
    space: PathBuf,
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
            space,
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
    fn step(&self, policy: &Policy, walker: &mut Walker) {
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
            return;
        }
        let changed = inner.source.changed();
        inner.knows = changed.is_some();
        inner.changed |= changed == Some(true);
        let now = Instant::now();
        let kind = policy.due(
            inner.last.map(|t| now - t),
            inner.last_full.map(|t| now - t),
            inner.knows.then_some(inner.changed),
            tight(&self.space),
        );
        let Some(kind) = kind else {
            return;
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
    }
}

/// The worker: each agent in turn, then a rest until the next look or a
/// nudge, for as long as any agent is tracked.
fn work(shared: &Shared) {
    let mut walker = Walker::new();
    let policy = shared.policy;
    let look = LOOK
        .min(policy.interval / TIGHT)
        .max(Duration::from_millis(10));
    loop {
        let live: Vec<Arc<Track>> = {
            let mut tracks = lock(&shared.tracks);
            tracks.list.retain(|t| t.strong_count() > 0);
            if tracks.list.is_empty() {
                tracks.working = false;
                return;
            }
            tracks.nudged = false;
            tracks.list.iter().filter_map(Weak::upgrade).collect()
        };
        for t in &live {
            t.step(&policy, &mut walker);
        }
        drop(live);
        let tracks = lock(&shared.tracks);
        if !tracks.nudged {
            let _ = shared
                .nudge
                .wait_timeout(tracks, look)
                .unwrap_or_else(|e| e.into_inner());
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
        assert!(free_space(tmp.path()).is_some_and(|f| f > 0));
        assert!(free_space(Path::new("/no/such/place")).is_none());
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

    fn until(what: &str, ok: impl Fn() -> bool) {
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
    fn a_walk_source_measures_what_a_walk_does() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f"), vec![1u8; 100_000]).unwrap();
        let roots = vec![tmp.path().to_owned()];
        let mut w = Walk::new(roots.clone(), None);
        assert_eq!(w.changed(), None);
        assert_eq!(w.measure(Kind::Update, &mut Walker::new()), usage(&roots));
    }
}
