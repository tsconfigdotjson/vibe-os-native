//! Boot, as PID 1: the kernel's filesystems, the agent's disks stacked into
//! the root, the shares at their paths, and the user `vibe` (PRD: What's
//! inside `linux`, Mounts).
//!
//! The root is overlayfs, never on virtio-fs: the read-only layer disks
//! named by `vibe.layers`, top first, under the agent's writable disk
//! (`vibe.upper`). A layer disk is either a whole root filesystem (the base
//! image) or the writable disk of an earlier VM, whose changes are in its
//! `upper/` directory; that's how the toolbox saves the template and the
//! Linux home. In the toolbox, `vibe.home` names a second writable disk,
//! which takes the writes to `vibe`'s home in its own `upper/home/vibe`
//! through an overlay of its own, so the Linux home and the template come
//! out as two layers. A blank writable disk is formatted ext4 on the first
//! boot, with the `mke2fs` the initrd carries. With no `vibe.layers` the
//! guest runs from its initrd, as the test guest does.
//!
//! `vibe.sudo` lets `vibe` use `sudo` in the toolbox, through a file in
//! `/etc/sudoers.d` that the seal takes away again; any other boot removes
//! it, should one ever be left in a layer.
//!
//! Nothing here stops the boot. What fails is reported as a problem in the
//! guest's status, so the host can say why.

use std::fs;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use vibe_guest::boot::Boot;
use vibe_guest::proto::ShareMount;

use crate::init::log;
use crate::init::net;
use crate::init::sys::{self, check, mkdir_p, mkdir_p_owned, mount};

/// `vibe`'s home.
pub(crate) const HOME: &str = "/home/vibe";

/// Where the managed layer (Claude Code) puts its commands: first on
/// `PATH`, ahead of anything the template installed.
pub(crate) const MANAGED_BIN: &str = "/opt/vibeos/bin";

/// The cgroup the agent runs in.
const CGROUP: &str = "/sys/fs/cgroup/agent";

/// Where the layer disks are mounted before the switch, in the initrd.
const LAYERS: &str = "/layers";

const NEW_ROOT: &str = "/newroot";

/// How long a disk named on the command line may take to show up.
const DISK_TIMEOUT: Duration = Duration::from_secs(10);

const NOSUID_NODEV: libc::c_ulong = libc::MS_NOSUID | libc::MS_NODEV;

/// The rule `vibe.sudo` puts in place, so the toolbox shell can install
/// with `sudo apt install …`. Its name has no `.`, which `sudo` would skip.
pub(crate) const SUDOERS: &str = "/etc/sudoers.d/vibe-toolbox";

/// A writable disk's filesystem, held open by its mount directory so it can
/// be sealed after the switch hides that directory.
#[derive(Debug)]
pub(crate) struct Writable {
    pub(crate) id: String,
    pub(crate) dir: fs::File,
}

/// What the boot set up.
#[derive(Debug)]
pub(crate) struct Setup {
    pub(crate) layers: Vec<String>,
    pub(crate) writable: Vec<Writable>,
    pub(crate) sudo: bool,
    pub(crate) shares: Vec<ShareMount>,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) cgroup: Option<PathBuf>,
    pub(crate) problems: Vec<String>,
}

pub(crate) fn setup() -> Setup {
    let mut problems = Vec::new();
    let mut problem = |what: String| {
        log!("{what}");
        problems.push(what);
    };
    kernel_filesystems(&mut problem);
    let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let (boot, unreadable) = Boot::parse(&cmdline);
    unreadable.into_iter().for_each(&mut problem);
    let mut layers = Vec::new();
    let mut writable = Vec::new();
    if !boot.layers.is_empty() {
        match stack(&boot) {
            Ok(disks) => {
                log!(
                    "the root is {} under {}{}",
                    boot.layers.join(", "),
                    boot.upper.as_deref().unwrap_or("a scratch disk"),
                    boot.home
                        .as_deref()
                        .map(|h| format!(", with {HOME} on {h}"))
                        .unwrap_or_default()
                );
                layers = boot.layers.clone();
                writable = disks;
            }
            Err(e) => problem(format!(
                "can't stack the disks, so running from the initrd: {e}"
            )),
        }
    }
    runtime_filesystems(&mut problem);
    let hostname = "vibe";
    // SAFETY: the pointer and length describe a live buffer.
    if unsafe { libc::sethostname(hostname.as_ptr().cast(), hostname.len()) } != 0 {
        problem(format!(
            "can't set the hostname: {}",
            io::Error::last_os_error()
        ));
    }
    if let Err(e) = net::up("lo") {
        problem(format!("can't bring up loopback: {e}"));
    }
    let (uid, gid) = (boot.uid(), boot.gid());
    let shell = ["/bin/bash", "/bin/sh"]
        .into_iter()
        .find(|s| Path::new(s).exists())
        .unwrap_or("/bin/sh");
    if let Err(e) = add_user(uid, gid, shell) {
        problem(format!("can't set up the user vibe: {e}"));
    }
    let sudo = boot.sudo && !layers.is_empty();
    if let Err(e) = set_sudo(sudo) {
        problem(format!("can't set up sudo for vibe: {e}"));
    }
    let runtime = format!("/run/user/{uid}");
    if let Err(e) = mkdir_p(Path::new(&runtime), 0o755)
        .and_then(|()| sys::chown(Path::new(&runtime), uid, gid))
        .and_then(|()| sys::chmod(Path::new(&runtime), 0o700))
    {
        problem(format!("can't make {runtime}: {e}"));
    }
    let mut shares = Vec::new();
    for share in &boot.shares {
        match mount_share(&share.tag, Path::new(&share.path), uid, gid) {
            Ok(()) => shares.push(ShareMount {
                tag: share.tag.clone(),
                path: share.path.clone(),
            }),
            Err(e) => problem(format!(
                "can't mount share {} at {}: {e}",
                share.tag, share.path
            )),
        }
    }
    let cgroup = match mkdir_p(Path::new(CGROUP), 0o755) {
        Ok(()) => Some(PathBuf::from(CGROUP)),
        Err(e) => {
            problem(format!(
                "no cgroup for the agent, so it can't be frozen: {e}"
            ));
            None
        }
    };
    let env = [
        ("HOME", HOME.to_owned()),
        ("USER", "vibe".into()),
        ("LOGNAME", "vibe".into()),
        ("SHELL", shell.into()),
        ("PATH", path()),
        ("TERM", "xterm-256color".into()),
        ("LANG", "C.UTF-8".into()),
        // Claude needs a private one for its peer socket (PRD: Spike
        // results).
        ("XDG_RUNTIME_DIR", runtime),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    Setup {
        layers,
        writable,
        sudo,
        shares,
        uid,
        gid,
        env,
        cgroup,
        problems,
    }
}

/// `PATH` for the agent: the managed layer first, then `vibe`'s own, then
/// the system's.
pub(crate) fn path() -> String {
    format!(
        "{MANAGED_BIN}:{HOME}/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    )
}

/// `/proc`, `/sys` and `/dev`, unless the initrd's own `/init` mounted them
/// already, and the console as stdin, stdout and stderr.
fn kernel_filesystems(problem: &mut impl FnMut(String)) {
    let proc_mounted = Path::new("/proc/self").exists();
    let wanted = [
        ("proc", "/proc", "proc", NOSUID_NODEV | libc::MS_NOEXEC, ""),
        ("sysfs", "/sys", "sysfs", NOSUID_NODEV | libc::MS_NOEXEC, ""),
        ("devtmpfs", "/dev", "devtmpfs", libc::MS_NOSUID, "mode=0755"),
    ];
    for (source, target, fstype, flags, data) in wanted {
        let target = Path::new(target);
        let already = if fstype == "proc" {
            proc_mounted
        } else {
            sys::mounted(target)
        };
        if already {
            continue;
        }
        if let Err(e) =
            mkdir_p(target, 0o755).and_then(|()| mount(source, target, fstype, flags, data))
        {
            problem(format!("can't mount {fstype} at {}: {e}", target.display()));
        }
    }
    if let Ok(console) = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/console")
    {
        for fd in 0..3 {
            // SAFETY: dup2 onto the standard descriptors from one we own.
            unsafe { libc::dup2(console.as_raw_fd(), fd) };
        }
    }
}

/// What every root needs once it's in place: terminals, shared memory, a
/// runtime directory, a temp directory and the cgroup filesystem.
fn runtime_filesystems(problem: &mut impl FnMut(String)) {
    let wanted = [
        (
            "devpts",
            "/dev/pts",
            "devpts",
            libc::MS_NOSUID | libc::MS_NOEXEC,
            "ptmxmode=0666,mode=0620",
        ),
        ("tmpfs", "/dev/shm", "tmpfs", NOSUID_NODEV, "mode=1777"),
        ("tmpfs", "/run", "tmpfs", NOSUID_NODEV, "mode=0755"),
        (
            "cgroup2",
            "/sys/fs/cgroup",
            "cgroup2",
            NOSUID_NODEV | libc::MS_NOEXEC,
            "",
        ),
    ];
    for (source, target, fstype, flags, data) in wanted {
        let target = Path::new(target);
        if sys::mounted(target) {
            continue;
        }
        if let Err(e) =
            mkdir_p(target, 0o755).and_then(|()| mount(source, target, fstype, flags, data))
        {
            problem(format!("can't mount {fstype} at {}: {e}", target.display()));
        }
    }
    // devtmpfs makes device nodes, not these links, which shells use for
    // process substitution.
    for (link, to) in [
        ("/dev/fd", "/proc/self/fd"),
        ("/dev/stdin", "/proc/self/fd/0"),
        ("/dev/stdout", "/proc/self/fd/1"),
        ("/dev/stderr", "/proc/self/fd/2"),
    ] {
        if fs::symlink_metadata(link).is_err() {
            let _ = symlink(to, link);
        }
    }
    // `vibe` must be able to reach everything; an initrd's root may come
    // `0700`, as Alpine's does.
    if fs::metadata("/").is_ok_and(|m| m.permissions().mode() & 0o001 == 0)
        && let Err(e) = sys::chmod(Path::new("/"), 0o755)
    {
        problem(format!("can't open up the root to vibe: {e}"));
    }
    // /tmp stays on disk, the agent's: a build that fills it mustn't fill
    // the VM's memory.
    if let Err(e) =
        mkdir_p(Path::new("/tmp"), 0o755).and_then(|()| sys::chmod(Path::new("/tmp"), 0o1777))
    {
        problem(format!("can't make /tmp: {e}"));
    }
}

/// Mounts the layers and the writable disks, stacks them with overlayfs,
/// and makes that the root. Returns the writable disks' filesystems.
fn stack(boot: &Boot) -> io::Result<Vec<Writable>> {
    let mut lowers = Vec::new();
    for id in &boot.layers {
        let device = find_disk(id)?;
        let dir = Path::new(LAYERS).join(id);
        mkdir_p(&dir, 0o755)?;
        mount_disk(&device, &dir, true)?;
        let upper = dir.join("upper");
        lowers.push(if upper.is_dir() { upper } else { dir });
    }
    let mut writable = Vec::new();
    let top = match &boot.upper {
        Some(id) => {
            let dir = writable_disk(id)?;
            writable.push(Writable {
                id: id.clone(),
                dir: fs::File::open(&dir)?,
            });
            dir
        }
        // Without a disk, writes go to memory and are lost on a reboot.
        None => {
            let dir = Path::new(LAYERS).join(".scratch");
            mkdir_p(&dir, 0o755)?;
            // Like a disk's, not `nosuid`: setuid is the root overlay's call.
            mount("tmpfs", &dir, "tmpfs", libc::MS_NODEV, "mode=0755")?;
            dir
        }
    };
    let (upper, work) = (top.join("upper"), top.join("work"));
    mkdir_p(&upper, 0o755)?;
    mkdir_p(&work, 0o755)?;
    let lowerdir: Vec<String> = lowers
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let options = format!(
        "lowerdir={},upperdir={},workdir={}",
        lowerdir.join(":"),
        upper.display(),
        work.display()
    );
    mkdir_p(Path::new(NEW_ROOT), 0o755)?;
    mount("overlay", Path::new(NEW_ROOT), "overlay", 0, &options)
        .map_err(|e| io::Error::new(e.kind(), format!("overlay {options}: {e}")))?;
    if let Some(id) = &boot.home {
        let dir = writable_disk(id)?;
        stack_home(&dir, boot.uid(), boot.gid())?;
        writable.push(Writable {
            id: id.clone(),
            dir: fs::File::open(&dir)?,
        });
    }
    switch_root(Path::new(NEW_ROOT))?;
    Ok(writable)
}

/// Finds writable disk `id`, formats it if it's blank, and mounts it at
/// `/layers/<id>`.
fn writable_disk(id: &str) -> io::Result<PathBuf> {
    let device = find_disk(id)?;
    if detect(&device)?.is_none() {
        // Only a disk that has never been written: anything else may be
        // the agent's work, and isn't the guest's to wipe.
        if !blank(&device)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} ({id}) has data but no filesystem vibe-guest knows; not formatting it",
                    device.display()
                ),
            ));
        }
        format(&device, id)?;
    }
    let dir = Path::new(LAYERS).join(id);
    mkdir_p(&dir, 0o755)?;
    mount_disk(&device, &dir, false)?;
    Ok(dir)
}

/// Lays `vibe`'s home in the new root over with the writable home disk
/// mounted at `disk`: its writes land in the disk's `upper/home/vibe`,
/// where they're the Linux home layer's when it's stacked under an agent.
fn stack_home(disk: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let (upper, work) = home_dirs(disk);
    mkdir_p(&disk.join("upper/home"), 0o755)?;
    mkdir_p_owned(&upper, 0o755, uid, gid)?;
    mkdir_p(&work, 0o755)?;
    let at = Path::new(NEW_ROOT).join(&HOME[1..]);
    mkdir_p(&at, 0o755)?;
    let options = format!(
        "lowerdir={},upperdir={},workdir={}",
        at.display(),
        upper.display(),
        work.display()
    );
    mount("overlay", &at, "overlay", 0, &options)
        .map_err(|e| io::Error::new(e.kind(), format!("overlay {options}: {e}")))
}

/// Where a writable home disk keeps `vibe`'s home, and overlayfs's work
/// directory beside it.
fn home_dirs(disk: &Path) -> (PathBuf, PathBuf) {
    (disk.join("upper").join(&HOME[1..]), disk.join("work"))
}

/// Puts the toolbox's `sudo` rule in place, or takes it away.
pub(crate) fn set_sudo(on: bool) -> io::Result<()> {
    let path = Path::new(SUDOERS);
    if !on {
        return match fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    mkdir_p(path.parent().expect("a parent"), 0o755)?;
    // A name with a `.`, which sudo skips, until it's whole.
    let tmp = path.with_extension("vibe-guest");
    fs::write(&tmp, "vibe ALL=(ALL:ALL) NOPASSWD: ALL\n")?;
    sys::chmod(&tmp, 0o440)?;
    fs::rename(&tmp, path)
}

/// Freezes the filesystem open at `dir`: the kernel flushes it, empties its
/// journal and marks it clean, so the disk under it can be kept as it is.
pub(crate) fn freeze_filesystem(dir: &fs::File) -> io::Result<()> {
    /// `_IOWR('X', 119, int)`.
    const FIFREEZE: u32 = 0xC004_5877;
    // SAFETY: an ioctl on a descriptor we hold, with an integer argument.
    check(unsafe { libc::ioctl(dir.as_raw_fd(), FIFREEZE as _, 0) }).map(drop)
}

/// Moves the kernel's filesystems into `new` and makes it `/`, as
/// `switch_root` does. The initrd stays underneath, hidden. If it fails,
/// what it moved goes back, so the initrd can carry on.
fn switch_root(new: &Path) -> io::Result<()> {
    let mut moved: Vec<(&str, PathBuf)> = Vec::new();
    let undo = |moved: &[(&str, PathBuf)]| {
        for (dir, to) in moved.iter().rev() {
            let _ = sys::move_mount(to, Path::new(dir));
        }
    };
    for dir in ["/proc", "/sys", "/dev"] {
        let to = new.join(&dir[1..]);
        if let Err(e) = mkdir_p(&to, 0o755).and_then(|()| sys::move_mount(Path::new(dir), &to)) {
            undo(&moved);
            return Err(e);
        }
        moved.push((dir, to));
    }
    if let Err(e) = std::env::set_current_dir(new)
        .and_then(|()| sys::move_mount(Path::new("."), Path::new("/")))
    {
        undo(&moved);
        let _ = std::env::set_current_dir("/");
        return Err(e);
    }
    let dot = c".";
    // SAFETY: chroot and chdir with NUL-terminated strings.
    unsafe {
        check(libc::chroot(dot.as_ptr()))?;
    }
    std::env::set_current_dir("/")
}

/// The block device whose virtio serial is `id`, waiting for it a while.
fn find_disk(id: &str) -> io::Result<PathBuf> {
    let deadline = Instant::now() + DISK_TIMEOUT;
    loop {
        let found = fs::read_dir("/sys/block")?.flatten().find_map(|entry| {
            let serial = fs::read_to_string(entry.path().join("serial")).ok()?;
            (serial.trim() == id).then(|| Path::new("/dev").join(entry.file_name()))
        });
        if let Some(device) = found
            && fs::metadata(&device).is_ok_and(|m| m.file_type().is_block_device())
        {
            return Ok(device);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no disk with serial `{id}`"),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The filesystem on a disk, by its magic number: what the layer images
/// may be.
fn detect(device: &Path) -> io::Result<Option<&'static str>> {
    let mut head = vec![0u8; 4096];
    let mut f = fs::File::open(device)?;
    let mut got = 0;
    while got < head.len() {
        match f.read(&mut head[got..])? {
            0 => break,
            n => got += n,
        }
    }
    Ok(magic(&head[..got]))
}

/// Whether the first 1 MiB of a disk is all zeros, as a new sparse image
/// is: where any filesystem would have left its mark.
fn blank(device: &Path) -> io::Result<bool> {
    let mut f = fs::File::open(device)?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut left = 1 << 20;
    while left > 0 {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if buf[..n].iter().any(|&b| b != 0) {
            return Ok(false);
        }
        left -= n.min(left);
    }
    Ok(true)
}

fn magic(head: &[u8]) -> Option<&'static str> {
    let at = |offset: usize, bytes: &[u8]| head.get(offset..offset + bytes.len()) == Some(bytes);
    if at(1080, &[0x53, 0xef]) {
        Some("ext4")
    } else if at(0, b"hsqs") {
        Some("squashfs")
    } else if at(1024, &[0xe2, 0xe1, 0xf5, 0xe0]) {
        Some("erofs")
    } else {
        None
    }
}

fn mount_disk(device: &Path, dir: &Path, read_only: bool) -> io::Result<()> {
    let source = device.to_string_lossy();
    let fstype = detect(device)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{source} has no filesystem vibe-guest knows"),
        )
    })?;
    let flags = disk_flags(read_only, fstype);
    // The writable disk is sparse on the host, so the kernel mustn't zero
    // its inode tables in the background; mke2fs marked them unused.
    let data = if fstype == "ext4" && !read_only {
        "noinit_itable"
    } else {
        ""
    };
    match mount(&source, dir, fstype, flags, data) {
        // A read-only image that wasn't unmounted cleanly can't replay its
        // journal; it can still be read without.
        Err(_) if fstype == "ext4" && read_only => mount(&source, dir, fstype, flags, "noload"),
        other => other,
    }
    .map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("mount {source} ({fstype}) at {}: {e}", dir.display()),
        )
    })
}

/// How a disk is mounted under `/layers`: never `nosuid`. Nothing runs
/// from those mounts, which the switch hides; programs run through the
/// root overlay, which decides setuid, and a layer's own `nosuid` would
/// only risk taking it away from the template's `sudo`.
fn disk_flags(read_only: bool, fstype: &str) -> libc::c_ulong {
    let mut flags = libc::MS_NODEV;
    if read_only || fstype != "ext4" {
        flags |= libc::MS_RDONLY;
    }
    flags
}

/// Formats a blank writable disk.
fn format(device: &Path, label: &str) -> io::Result<()> {
    let mke2fs = ["mke2fs", "mkfs.ext4"]
        .iter()
        .flat_map(|name| {
            ["/sbin", "/usr/sbin", "/bin", "/usr/bin"].map(|d| Path::new(d).join(name))
        })
        .find(|p| p.exists())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no mke2fs in the initrd"))?;
    log!("formatting {} as ext4", device.display());
    let out = Command::new(&mke2fs)
        .args(["-q", "-F", "-t", "ext4", "-L", label])
        .args(["-E", "lazy_itable_init=1,lazy_journal_init=1"])
        .arg(device)
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{} failed: {}",
            mke2fs.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

/// Mounts a virtio-fs share at `path`, making the directories on the way.
/// Those in `vibe`'s home are `vibe`'s, so a share at `~/.cache/pip`
/// doesn't leave `~/.cache` to root.
fn mount_share(tag: &str, path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    if path.starts_with(HOME) {
        mkdir_p_owned(path, 0o755, uid, gid)?;
    } else {
        mkdir_p(path, 0o755)?;
    }
    mount(tag, path, "virtiofs", NOSUID_NODEV, "")
}

/// Makes `vibe` the user and group with these ids, in the root's own
/// `/etc` (the agent's copy), with a home of its own.
fn add_user(uid: u32, gid: u32, shell: &str) -> io::Result<()> {
    let passwd = Path::new("/etc/passwd");
    let group = Path::new("/etc/group");
    mkdir_p(Path::new("/etc"), 0o755)?;
    let entry = format!("vibe:x:{uid}:{gid}:vibe:{HOME}:{shell}");
    replace_atomically(passwd, &upsert(&read_or_empty(passwd)?, "vibe", &entry))?;
    let entry = format!("vibe:x:{gid}:");
    replace_atomically(group, &upsert(&read_or_empty(group)?, "vibe", &entry))?;
    mkdir_p(Path::new("/home"), 0o755)?;
    mkdir_p(Path::new(HOME), 0o755)?;
    // Only the top: the Linux home layer's files are already the user's,
    // and a recursive chown would copy every one of them up.
    sys::chown(Path::new(HOME), uid, gid)
}

fn read_or_empty(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}

fn replace_atomically(path: &Path, text: &str) -> io::Result<()> {
    let tmp = path.with_extension("vibe-guest");
    fs::write(&tmp, text)?;
    sys::chmod(&tmp, 0o644)?;
    fs::rename(&tmp, path)
}

/// `text`, a `/etc/passwd` or `/etc/group`, with the entry for `name` set
/// to `line`, added at the end if there was none.
fn upsert(text: &str, name: &str, line: &str) -> String {
    let prefix = format!("{name}:");
    let mut found = false;
    let mut out: Vec<&str> = Vec::new();
    for l in text.lines() {
        if l.starts_with(&prefix) {
            if !found {
                out.push(line);
            }
            found = true;
        } else {
            out.push(l);
        }
    }
    if !found {
        out.push(line);
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn users_are_added_or_replaced() {
        let base = "root:x:0:0:root:/root:/bin/bash\nvibe:x:1000:1000::/home/vibe:/bin/sh\n";
        let line = "vibe:x:501:20:vibe:/home/vibe:/bin/bash";
        assert_eq!(
            upsert(base, "vibe", line),
            format!("root:x:0:0:root:/root:/bin/bash\n{line}\n")
        );
        assert_eq!(upsert("", "vibe", line), format!("{line}\n"));
        assert_eq!(
            upsert("root:x:0:0::/:/bin/sh", "vibe", line),
            format!("root:x:0:0::/:/bin/sh\n{line}\n")
        );
        // A user whose name starts the same is someone else.
        assert_eq!(
            upsert("viber:x:7:7::/:/bin/sh\n", "vibe", line),
            format!("viber:x:7:7::/:/bin/sh\n{line}\n")
        );
        // Duplicates collapse into one.
        assert_eq!(
            upsert("vibe:a\nvibe:b\n", "vibe", line),
            format!("{line}\n")
        );
    }

    #[test]
    fn filesystems_are_told_apart_by_magic() {
        let mut ext4 = vec![0u8; 2048];
        ext4[1080..1082].copy_from_slice(&[0x53, 0xef]);
        assert_eq!(magic(&ext4), Some("ext4"));
        assert_eq!(magic(b"hsqs and the rest"), Some("squashfs"));
        let mut erofs = vec![0u8; 2048];
        erofs[1024..1028].copy_from_slice(&[0xe2, 0xe1, 0xf5, 0xe0]);
        assert_eq!(magic(&erofs), Some("erofs"));
        assert_eq!(magic(&[0u8; 4096]), None);
        assert_eq!(magic(&[]), None);
    }

    #[test]
    fn only_a_disk_never_written_is_blank() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("disk.img");
        fs::File::create(&disk).unwrap().set_len(4 << 20).unwrap();
        assert!(blank(&disk).unwrap());
        // A byte deep in the first MiB, say a damaged superblock's backup.
        std::os::unix::fs::FileExt::write_at(
            &fs::OpenOptions::new().write(true).open(&disk).unwrap(),
            b"x",
            900_000,
        )
        .unwrap();
        assert!(!blank(&disk).unwrap());
        let empty = dir.path().join("empty.img");
        fs::File::create(&empty).unwrap();
        assert!(blank(&empty).unwrap());
    }

    #[test]
    fn layers_keep_setuid() {
        for (read_only, fstype) in [(true, "ext4"), (false, "ext4"), (true, "squashfs")] {
            assert_eq!(disk_flags(read_only, fstype) & libc::MS_NOSUID, 0);
            assert_ne!(disk_flags(read_only, fstype) & libc::MS_NODEV, 0);
        }
        assert_ne!(disk_flags(true, "ext4") & libc::MS_RDONLY, 0);
        assert_eq!(disk_flags(false, "ext4") & libc::MS_RDONLY, 0);
        assert_ne!(disk_flags(false, "erofs") & libc::MS_RDONLY, 0);
    }

    #[test]
    fn the_home_disk_holds_the_home_where_a_layer_has_it() {
        let (upper, work) = home_dirs(Path::new("/layers/home"));
        assert_eq!(upper, Path::new("/layers/home/upper/home/vibe"));
        assert_eq!(work, Path::new("/layers/home/work"));
        // Stacked as a layer, the disk's `upper/` is a root, and its
        // `home/vibe` lands on vibe's home.
        assert_eq!(
            Path::new("/").join(upper.strip_prefix("/layers/home/upper").unwrap()),
            Path::new(HOME)
        );
        assert!(!SUDOERS.rsplit('/').next().unwrap().contains('.'));
    }

    #[test]
    fn the_managed_layer_comes_first_on_path() {
        let path = path();
        assert!(
            path.starts_with("/opt/vibeos/bin:/home/vibe/.local/bin:"),
            "{path}"
        );
        assert!(path.ends_with(":/bin"));
    }
}
