//! FSEvents on macOS: which directories under an agent's trees changed,
//! for the [cache](super::cache) to list again.
//!
//! The stream reports directories, not files (no
//! `kFSEventStreamCreateFlagFileEvents`): the cache is kept a directory at a
//! time, and a build writing ten thousand files into fifty directories is
//! fifty paths a batch rather than ten thousand. A batch FSEvents had to
//! coalesce (`MustScanSubDirs`) marks everything under its path; events
//! dropped, IDs wrapped, a volume mounted or unmounted, or a root moved
//! (`WatchRoot`) mark everything. Callbacks come on one serial dispatch
//! queue that every stream shares, so watching costs no thread of its own.

use std::ffi::{CStr, CString, c_char, c_void};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use super::cache::Dirty;

type CFIndex = isize;
type CFAllocatorRef = *const c_void;
type CFArrayRef = *const c_void;
type CFStringRef = *const c_void;
type FSEventStreamRef = *mut c_void;
type DispatchQueue = *mut c_void;

#[repr(C)]
struct CFArrayCallBacks {
    version: CFIndex,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
}

#[repr(C)]
struct FSEventStreamContext {
    version: CFIndex,
    info: *mut c_void,
    retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<extern "C" fn(*const c_void)>,
    copy_description: Option<extern "C" fn(*const c_void) -> CFStringRef>,
}

type Callback =
    extern "C" fn(FSEventStreamRef, *mut c_void, usize, *mut c_void, *const u32, *const u64);

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: CFArrayCallBacks;
    fn CFArrayCreate(
        allocator: CFAllocatorRef,
        values: *const *const c_void,
        count: CFIndex,
        callbacks: *const CFArrayCallBacks,
    ) -> CFArrayRef;
    fn CFStringCreateWithFileSystemRepresentation(
        allocator: CFAllocatorRef,
        buffer: *const c_char,
    ) -> CFStringRef;
    fn CFRelease(cf: *const c_void);
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        allocator: CFAllocatorRef,
        callback: Callback,
        context: *const FSEventStreamContext,
        paths: CFArrayRef,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> FSEventStreamRef;
    fn FSEventStreamSetDispatchQueue(stream: FSEventStreamRef, queue: DispatchQueue);
    fn FSEventStreamStart(stream: FSEventStreamRef) -> u8;
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);
}

// libdispatch is in libSystem, which everything links.
unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueue;
    fn dispatch_sync_f(
        queue: DispatchQueue,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
}

const SINCE_NOW: u64 = u64::MAX;
const CREATE_WATCH_ROOT: u32 = 0x04;

const MUST_SCAN_SUBDIRS: u32 = 0x01;
const USER_DROPPED: u32 = 0x02;
const KERNEL_DROPPED: u32 = 0x04;
const IDS_WRAPPED: u32 = 0x08;
const ROOT_CHANGED: u32 = 0x20;
const MOUNT: u32 = 0x40;
const UNMOUNT: u32 = 0x80;

/// The one serial queue every stream calls back on.
fn queue() -> DispatchQueue {
    static QUEUE: OnceLock<usize> = OnceLock::new();
    *QUEUE.get_or_init(|| {
        // SAFETY: a static label, and a null attribute for a serial queue,
        // which lives as long as the process.
        unsafe { dispatch_queue_create(c"vibe-env disk".as_ptr(), std::ptr::null()) as usize }
    }) as DispatchQueue
}

/// What the stream has said changed, shared with its callbacks.
type Shared = Mutex<Dirty>;

/// Directories changed under some paths, as FSEvents reports them, until
/// it's dropped.
#[derive(Debug)]
pub struct Watch {
    stream: usize,
    shared: Arc<Shared>,
}

impl Watch {
    /// Watches `paths`, which should be real paths (no symbolic links), as
    /// FSEvents reports those. `latency` is how long it gathers changes
    /// before it calls back with them.
    pub fn new(paths: &[PathBuf], latency: Duration) -> io::Result<Watch> {
        let shared: Arc<Shared> = Arc::new(Mutex::new(Dirty::default()));
        let c_paths: Vec<CString> = paths
            .iter()
            .map(|p| CString::new(p.as_os_str().as_bytes()))
            .collect::<Result<_, _>>()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL"))?;
        // SAFETY: each string is NUL-terminated and outlives its CFString's
        // creation; the array retains the strings, the stream copies the
        // array, and each is released once here.
        let stream = unsafe {
            let strings: Vec<CFStringRef> = c_paths
                .iter()
                .map(|p| CFStringCreateWithFileSystemRepresentation(std::ptr::null(), p.as_ptr()))
                .collect();
            if strings.iter().any(|s| s.is_null()) {
                for s in strings.iter().filter(|s| !s.is_null()) {
                    CFRelease(*s);
                }
                return Err(io::Error::other("can't make the paths to watch"));
            }
            let array = CFArrayCreate(
                std::ptr::null(),
                strings.as_ptr(),
                strings.len() as CFIndex,
                &kCFTypeArrayCallBacks,
            );
            for s in &strings {
                CFRelease(*s);
            }
            if array.is_null() {
                return Err(io::Error::other("can't make the paths to watch"));
            }
            let context = FSEventStreamContext {
                version: 0,
                info: Arc::as_ptr(&shared).cast_mut().cast(),
                retain: Some(retain),
                release: Some(release),
                copy_description: None,
            };
            let stream = FSEventStreamCreate(
                std::ptr::null(),
                callback,
                &context,
                array,
                SINCE_NOW,
                latency.as_secs_f64(),
                CREATE_WATCH_ROOT,
            );
            CFRelease(array);
            stream
        };
        if stream.is_null() {
            return Err(io::Error::other("FSEvents didn't make a stream"));
        }
        let watch = Watch {
            stream: stream as usize,
            shared,
        };
        // SAFETY: a stream just made, scheduled on a queue that lives as
        // long as the process; it's let go of in `drop`.
        let started = unsafe {
            FSEventStreamSetDispatchQueue(stream, queue());
            FSEventStreamStart(stream) != 0
        };
        if !started {
            return Err(io::Error::other("FSEvents didn't start a stream"));
        }
        Ok(watch)
    }

    /// What's changed since the last time it was asked.
    pub fn take(&self) -> Dirty {
        std::mem::take(&mut *self.shared.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        extern "C" fn end(stream: *mut c_void) {
            // SAFETY: called once, on the stream's own queue, so no callback
            // runs meanwhile; the stream was started and isn't used after.
            unsafe {
                FSEventStreamStop(stream);
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
            }
        }
        // SAFETY: the queue is serial and a callback never drops a `Watch`,
        // so this can't wait on itself.
        unsafe { dispatch_sync_f(queue(), self.stream as *mut c_void, end) };
    }
}

extern "C" fn retain(info: *const c_void) -> *const c_void {
    // SAFETY: `info` is an `Arc<Shared>`'s pointer, kept alive by the
    // `Watch` until FSEvents first retains it.
    unsafe { Arc::increment_strong_count(info.cast::<Shared>()) };
    info
}

extern "C" fn release(info: *const c_void) {
    // SAFETY: matched with a `retain` above.
    unsafe { Arc::decrement_strong_count(info.cast::<Shared>()) };
}

extern "C" fn callback(
    _stream: FSEventStreamRef,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    _ids: *const u64,
) {
    // SAFETY: FSEvents passes back the `info` it retained, and `count`
    // C strings and flags (no `UseCFTypes`), valid for the call.
    let shared = unsafe { &*info.cast::<Shared>() };
    let paths = paths.cast::<*const c_char>();
    let mut dirty = shared.lock().unwrap_or_else(|e| e.into_inner());
    for i in 0..count {
        let (path, flag) = unsafe { (CStr::from_ptr(*paths.add(i)), *flags.add(i)) };
        if flag & (USER_DROPPED | KERNEL_DROPPED | IDS_WRAPPED | ROOT_CHANGED | MOUNT | UNMOUNT)
            != 0
        {
            dirty.everything();
            continue;
        }
        let mut bytes = path.to_bytes();
        while bytes.len() > 1 && bytes.ends_with(b"/") {
            bytes = &bytes[..bytes.len() - 1];
        }
        let path = Path::new(std::ffi::OsStr::from_bytes(bytes));
        dirty.mark(path, flag & MUST_SCAN_SUBDIRS != 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn until(what: &str, mut ok: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ok() {
            assert!(Instant::now() < deadline, "no {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn it_says_which_directories_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("root");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        let w = Watch::new(std::slice::from_ref(&root), Duration::from_millis(50)).unwrap();
        // FSEvents starts from now: give it a moment to.
        std::thread::sleep(Duration::from_millis(500));
        w.take();
        std::fs::write(root.join("a/b/f"), b"hello").unwrap();
        let mut seen = Dirty::default();
        until("change in a/b", || {
            seen.merge(w.take());
            seen.all || seen.dirs.contains_key(&root.join("a/b"))
        });
        // Nothing outside it.
        std::fs::write(tmp.path().join("outside"), b"x").unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let d = w.take();
        assert!(d.dirs.keys().all(|p| p.starts_with(&root)), "{:?}", d.dirs);
        drop(w);
    }
}
