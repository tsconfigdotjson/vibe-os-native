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

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;

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
}
