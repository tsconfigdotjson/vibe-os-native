//! Keeping disk images sparse on the host, so a 20 GB image that holds
//! 2 GB takes 2 GB.
//!
//! Skipping the zeros when writing isn't enough everywhere. Linux's
//! filesystems leave a block that's never written as a hole, but APFS
//! writes zeros on disk for a gap a later write leaves behind it. So once
//! an image is written, [`punch_zeros`] goes through the parts of it that
//! are on disk and gives back every run of whole blocks that holds only
//! zeros: `F_PUNCHHOLE` on macOS, `fallocate` on Linux. It flushes the
//! file first, as APFS writes those zeros out only then. Reads of a hole
//! return zeros, so the file's contents don't change.
//!
//! What it can't give back: blocks APFS can hold in a file that was extended as it
//! was written, a write at a time. They count in `st_blocks` but read as a hole, and
//! `F_PUNCHHOLE` returns success without freeing them. So an image's end stays past
//! what's being written to it.
//!
//! [`clone_file`] makes a new image from an old one without copying it: an
//! APFS clone on a Mac (`clonefile`), a reflink where Linux has one, and
//! otherwise a copy of only the parts on disk.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;

/// What's read at a time while looking for zeros.
const CHUNK: u64 = 1 << 20;

/// The filesystem's block size: holes are whole blocks.
fn block_size(file: &File) -> u64 {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: statfs is plain data, filled in by fstatfs.
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatfs(file.as_raw_fd(), &mut st) } == 0 {
            let b = u64::from(st.f_bsize);
            if b.is_power_of_two() && (512..=CHUNK).contains(&b) {
                return b;
            }
        }
    }
    let _ = file;
    4096
}

/// Makes `[offset, offset + len)` of `file` a hole. False if the filesystem
/// can't punch holes.
fn punch(file: &File, offset: u64, len: u64) -> io::Result<bool> {
    let fd = file.as_raw_fd();
    let (Ok(off), Ok(n)) = (libc::off_t::try_from(offset), libc::off_t::try_from(len)) else {
        return Ok(false);
    };
    #[cfg(target_os = "macos")]
    // SAFETY: the struct is passed by pointer and outlives the call.
    let rc = unsafe {
        let hole = libc::fpunchhole_t {
            fp_flags: 0,
            reserved: 0,
            fp_offset: off,
            fp_length: n,
        };
        libc::fcntl(fd, libc::F_PUNCHHOLE, &hole)
    };
    #[cfg(target_os = "linux")]
    // SAFETY: fallocate takes plain integers.
    let rc = unsafe {
        libc::fallocate(
            fd,
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            off,
            n,
        )
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let rc = {
        let _ = (fd, off, n);
        return Ok(false);
    };
    if rc == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        // The same number on Linux, two on macOS.
        Some(c) if [libc::EOPNOTSUPP, libc::ENOTSUP, libc::EINVAL, libc::ENOSYS].contains(&c) => {
            Ok(false)
        }
        _ => Err(e),
    }
}

/// Where the next data at or after `from` starts, and where it ends; `None`
/// past the last. Without `SEEK_DATA`, everything is data.
fn next_data(file: &File, from: u64, len: u64) -> Option<(u64, u64)> {
    if from >= len {
        return None;
    }
    let fd = file.as_raw_fd();
    let seek = |pos: u64, whence| {
        // SAFETY: lseek on a descriptor we own; the file's offset isn't used
        // for anything else here, as every read is positioned.
        let r = unsafe { libc::lseek(fd, pos as libc::off_t, whence) };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(r as u64)
        }
    };
    let start = match seek(from, libc::SEEK_DATA) {
        Ok(s) => s,
        Err(e) if e.raw_os_error() == Some(libc::ENXIO) => return None,
        Err(_) => return Some((from, len)),
    };
    let end = seek(start, libc::SEEK_HOLE).unwrap_or(len).min(len);
    (start < end).then_some((start, end))
}

/// Turns every run of whole blocks of `file` that holds only zeros into a
/// hole. Returns how many bytes were given back. A filesystem that can't
/// punch holes is left as it is, and that's no error.
pub fn punch_zeros(file: &File) -> io::Result<u64> {
    // APFS fills a gap with zeros only when the file is flushed, and until
    // then reports the gap as a hole, which the walk below would skip. So
    // the file is flushed first, and every block it will keep is on disk
    // and found.
    file.sync_all()?;
    let len = file.metadata()?.len();
    let block = block_size(file);
    let mut buf = vec![0u8; CHUNK as usize];
    let mut punched = 0;
    // The run of zero blocks so far: where it starts.
    let mut run: Option<u64> = None;
    let mut pos = 0;
    let flush = |start: u64, end: u64, punched: &mut u64| -> io::Result<bool> {
        if end > start {
            if !punch(file, start, end - start)? {
                return Ok(false);
            }
            *punched += end - start;
        }
        Ok(true)
    };
    while let Some((data, end)) = next_data(file, pos, len) {
        // A hole skipped is zeros too: a run of zeros goes on across it,
        // and punching a hole again is harmless.
        let mut at = (data / block * block).max(pos);
        while at < end {
            let want = (end - at).min(CHUNK).div_ceil(block) * block;
            let want = want.min(len - at) as usize;
            file.read_exact_at(&mut buf[..want], at)?;
            for b in buf[..want].chunks(block as usize) {
                let whole = b.len() as u64 == block;
                if whole && b.iter().all(|&x| x == 0) {
                    run.get_or_insert(at);
                } else if let Some(start) = run.take()
                    && !flush(start, at, &mut punched)?
                {
                    return Ok(punched);
                }
                at += b.len() as u64;
            }
        }
        pos = at;
    }
    if let Some(start) = run {
        flush(start, len / block * block, &mut punched)?;
    }
    Ok(punched)
}

/// Makes `dest`, which mustn't exist, a copy of `src` that shares its
/// blocks where the filesystem can: an APFS clone on macOS, a reflink
/// (`FICLONE`) on Linux, and failing those a sparse copy, of only the parts
/// of `src` on disk. `dest` is `0600`, whatever `src` is: it's to be
/// written.
pub fn clone_file(src: &Path, dest: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = |p: &Path| {
            std::ffi::CString::new(p.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in a path"))
        };
        let (from, to) = (c(src)?, c(dest)?);
        // SAFETY: two NUL-terminated paths that outlive the call.
        if unsafe { libc::clonefile(from.as_ptr(), to.as_ptr(), 0) } == 0 {
            return std::fs::set_permissions(
                dest,
                std::os::unix::fs::PermissionsExt::from_mode(0o600),
            );
        }
        let e = io::Error::last_os_error();
        // Only where cloning isn't possible: another volume, or not APFS.
        if ![libc::EXDEV, libc::ENOTSUP, libc::EOPNOTSUPP].contains(&e.raw_os_error().unwrap_or(0))
        {
            return Err(e);
        }
    }
    let from = File::open(src)?;
    let to = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dest)?;
    let copied = copy_into(&from, &to);
    if copied.is_err() {
        drop(to);
        let _ = std::fs::remove_file(dest);
    }
    copied
}

fn copy_into(from: &File, to: &File) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: an ioctl between two descriptors we hold.
        if unsafe { libc::ioctl(to.as_raw_fd(), libc::FICLONE as _, from.as_raw_fd()) } == 0 {
            return Ok(());
        }
    }
    let len = from.metadata()?.len();
    to.set_len(len)?;
    let mut buf = vec![0u8; CHUNK as usize];
    let mut at = 0;
    while let Some((start, end)) = next_data(from, at, len) {
        let mut pos = start;
        while pos < end {
            let n = ((end - pos) as usize).min(buf.len());
            from.read_exact_at(&mut buf[..n], pos)?;
            // A block of zeros stays a hole.
            if buf[..n].iter().any(|&b| b != 0) {
                to.write_all_at(&buf[..n], pos)?;
            }
            pos += n as u64;
        }
        at = end;
    }
    punch_zeros(to)?;
    to.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::os::unix::fs::MetadataExt;

    fn on_disk(file: &File) -> u64 {
        file.metadata().unwrap().blocks() * 512
    }

    #[test]
    fn zeros_written_out_become_holes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("img");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        // 8 MiB of real zeros, with data in a few places: what APFS leaves
        // when an image is written in order.
        let mut bytes = vec![0u8; 8 << 20];
        bytes[..5].copy_from_slice(b"start");
        bytes[3 << 20..(3 << 20) + 4].copy_from_slice(b"mid!");
        let tail = bytes.len() - 3;
        bytes[tail..].copy_from_slice(b"end");
        file.write_all_at(&bytes, 0).unwrap();
        file.sync_all().unwrap();
        let before = on_disk(&file);
        assert!(before >= 8 << 20, "the zeros are on disk: {before}");

        let punched = punch_zeros(&file).unwrap();
        file.sync_all().unwrap();
        let after = on_disk(&file);
        assert!(punched >= 7 << 20, "{punched}");
        assert!(after < 1 << 20, "still {after} bytes on disk");
        let mut back = vec![0u8; bytes.len()];
        file.read_exact_at(&mut back, 0).unwrap();
        assert!(back == bytes, "the contents changed");
        // Again, there's nothing more to give back.
        assert_eq!(punch_zeros(&file).unwrap(), 0);
    }

    #[test]
    fn gaps_left_by_writes_not_yet_flushed_become_holes() {
        // As an image is written: data here and there, in order, with
        // nothing flushed until the punching. APFS writes the gaps out as
        // zeros at the flush.
        let tmp = tempfile::tempdir().unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(tmp.path().join("img"))
            .unwrap();
        let mut at = 0u64;
        for i in 0..8u8 {
            file.write_all_at(&[i + 1; 4096], at).unwrap();
            at += 1 << 20;
        }
        file.set_len(8 << 20).unwrap();
        punch_zeros(&file).unwrap();
        file.sync_all().unwrap();
        let after = on_disk(&file);
        assert!(after < 1 << 20, "still {after} bytes on disk");
        for i in 0..8u8 {
            let mut b = [0u8; 4097];
            file.read_exact_at(&mut b, u64::from(i) << 20).unwrap();
            assert!(b[..4096].iter().all(|&x| x == i + 1) && b[4096] == 0);
        }
    }

    #[test]
    fn an_empty_or_all_data_file_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let empty = File::create(tmp.path().join("e")).unwrap();
        assert_eq!(punch_zeros(&empty).unwrap(), 0);
        let full = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(tmp.path().join("f"))
            .unwrap();
        full.write_all_at(&vec![7u8; 3 << 20], 0).unwrap();
        assert_eq!(punch_zeros(&full).unwrap(), 0);
        // A last part-block of zeros isn't a whole block: it stays.
        let odd = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(tmp.path().join("o"))
            .unwrap();
        odd.write_all_at(&[1u8; 4096], 0).unwrap();
        odd.write_all_at(&[0u8; 100], 4096).unwrap();
        assert_eq!(punch_zeros(&odd).unwrap(), 0);
        assert_eq!(odd.metadata().unwrap().len(), 4196);
    }

    #[test]
    fn a_clone_is_the_same_bytes_and_stays_sparse() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.img");
        let f = File::create(&src).unwrap();
        f.set_len(64 << 20).unwrap();
        f.write_all_at(b"superblock", 1024).unwrap();
        f.write_all_at(b"tail", (64 << 20) - 4).unwrap();
        drop(f);
        std::fs::set_permissions(&src, std::os::unix::fs::PermissionsExt::from_mode(0o444))
            .unwrap();
        let dest = tmp.path().join("dest.img");
        clone_file(&src, &dest).unwrap();
        let (a, b) = (std::fs::read(&src).unwrap(), std::fs::read(&dest).unwrap());
        assert!(a == b, "the clone differs");
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = std::fs::metadata(&dest).unwrap();
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o600,
            "the clone is to be written"
        );
        assert!(
            meta.blocks() * 512 < 4 << 20,
            "{} bytes on disk",
            meta.blocks() * 512
        );
        // Writing the clone leaves the original as it was.
        OpenOptions::new()
            .write(true)
            .open(&dest)
            .unwrap()
            .write_all_at(b"changed", 1024)
            .unwrap();
        assert_eq!(std::fs::read(&src).unwrap(), a);
        // It never replaces a file.
        assert!(clone_file(&src, &dest).is_err());
        assert_eq!(&std::fs::read(&dest).unwrap()[1024..1031], b"changed");
    }
}
