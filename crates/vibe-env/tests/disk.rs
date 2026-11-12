//! The disk walk allocates nothing per file (#167): a walker's buffers are
//! kept from walk to walk, so once warm a walk of ten thousand files
//! allocates no more than one of a thousand, which is nothing. The old walk,
//! a path per entry, is counted beside it to show the count would see it.
//!
//! Its own test binary, as it counts every allocation this process makes,
//! per thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};

use vibe_env::disk::{Walker, usage};

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

fn count() {
    ALLOCATIONS.with(|c| c.set(c.get() + 1));
}

// SAFETY: every call goes straight to the system allocator.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// What `f` returns, and how many allocations it made on this thread.
fn allocations<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let before = ALLOCATIONS.with(Cell::get);
    let out = f();
    (out, ALLOCATIONS.with(Cell::get) - before)
}

/// `dirs` directories two deep, of `files` small files each, and a file
/// with links in every directory.
fn tree(root: &Path, dirs: usize, files: usize) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("shared"), vec![9u8; 50_000]).unwrap();
    for d in 0..dirs {
        let dir = root.join(format!("d{}/e{d}", d % 4));
        fs::create_dir_all(&dir).unwrap();
        for f in 0..files {
            fs::write(dir.join(format!("f{f}")), [1u8; 100]).unwrap();
        }
        fs::hard_link(root.join("shared"), dir.join("shared")).unwrap();
    }
}

/// The walk before #167: a path for every entry.
fn old(paths: &[PathBuf]) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let mut seen = std::collections::HashSet::new();
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

#[test]
fn a_walk_allocates_nothing_per_file() {
    let tmp = tempfile::tempdir().unwrap();
    let (small, big) = (tmp.path().join("small"), tmp.path().join("big"));
    tree(&small, 20, 50);
    tree(&big, 20, 500);

    let mut w = Walker::new();
    // A first walk grows the walker's buffers, a few times over, however
    // many files there are.
    let (bytes, cold) = allocations(|| w.usage(&[&big]));
    assert_eq!(bytes, old(std::slice::from_ref(&big)));
    assert!(
        cold < 64,
        "a first walk of 10,000 files allocated {cold} times"
    );

    // Warm, a walk allocates nothing at all, small or big.
    let (_, small_warm) = allocations(|| w.usage(&[&small]));
    let (bytes, big_warm) = allocations(|| w.usage(&[&big]));
    assert_eq!(bytes, usage(std::slice::from_ref(&big)));
    assert_eq!((small_warm, big_warm), (0, 0));

    // The old walk allocated for every entry, and more.
    let (_, before) = allocations(|| old(std::slice::from_ref(&big)));
    assert!(before > 10_000, "the old walk allocated {before} times");
    eprintln!(
        "allocations walking 10,000 files: old {before}, new {cold} cold and {big_warm} warm"
    );
}
