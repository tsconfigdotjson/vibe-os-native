//! The guest side: a vsock server in PID 1.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use crate::PORT;

static COUNT: AtomicU64 = AtomicU64::new(0);

/// Serves [`PORT`] for ever. As PID 1 it must never return: if it can't
/// serve, it says why on the console and waits, so the test that boots it
/// times out with the reason in the console log.
pub(crate) fn run() -> ! {
    match listen() {
        Ok(listener) => {
            println!("vibe-testguest: serving vsock port {PORT}");
            loop {
                match accept(&listener) {
                    Ok(conn) => {
                        std::thread::spawn(move || {
                            if let Err(e) = serve(conn) {
                                eprintln!("vibe-testguest: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        eprintln!("vibe-testguest: accept: {e}");
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        }
        Err(e) => eprintln!("vibe-testguest: can't serve vsock port {PORT}: {e}"),
    }
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

fn listen() -> io::Result<OwnedFd> {
    // SAFETY: plain syscalls on a descriptor this function owns.
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let owned = OwnedFd::from_raw_fd(fd);
        let mut addr: libc::sockaddr_vm = std::mem::zeroed();
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_port = PORT;
        addr.svm_cid = libc::VMADDR_CID_ANY;
        let rc = libc::bind(
            fd,
            (&raw const addr).cast(),
            size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        );
        if rc != 0 || libc::listen(fd, 16) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(owned)
    }
}

fn accept(listener: &OwnedFd) -> io::Result<File> {
    use std::os::fd::AsRawFd;
    // SAFETY: accept on a listening socket; the result is owned at once.
    let fd = unsafe {
        libc::accept4(
            listener.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a new descriptor nothing else owns.
    Ok(File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
}

fn serve(conn: File) -> io::Result<()> {
    let mut line = String::new();
    BufReader::new(conn.try_clone()?).read_line(&mut line)?;
    let reply = answer(line.trim_end());
    let mut conn = conn;
    conn.write_all(format!("{reply}\n").as_bytes())
}

fn answer(command: &str) -> Value {
    let (verb, arg) = command.split_once(' ').unwrap_or((command, ""));
    match verb {
        "tick" => json!({ "count": COUNT.fetch_add(1, Ordering::SeqCst) + 1 }),
        "status" => json!({
            "count": COUNT.load(Ordering::SeqCst),
            "boot_id": read_trimmed("/proc/sys/kernel/random/boot_id"),
            "mac": read_trimmed("/sys/class/net/eth0/address"),
            "share": read_trimmed("/mnt/share/probe"),
            "disk": read_disk(),
            "uptime_ms": uptime_ms(),
        }),
        "dirty" => match arg.parse::<usize>() {
            Ok(mib) => {
                dirty(mib);
                json!({ "dirtied_mib": mib })
            }
            Err(_) => json!({ "error": "dirty takes a number of MiB" }),
        },
        "write-disk" => match write_disk(arg) {
            Ok(()) => json!({ "written": arg }),
            Err(e) => json!({ "error": e.to_string() }),
        },
        _ => json!({ "error": format!("unknown command `{command}`") }),
    }
}

fn read_trimmed(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

fn uptime_ms() -> Option<u64> {
    let text = fs::read_to_string("/proc/uptime").ok()?;
    let secs: f64 = text.split_whitespace().next()?.parse().ok()?;
    Some((secs * 1000.0) as u64)
}

/// Touches `mib` MiB, then frees them: the allocation is large enough that
/// musl maps it on its own and unmaps it on free, giving the pages back to
/// the guest kernel.
fn dirty(mib: usize) {
    let mut memory = vec![0u8; mib << 20];
    for page in memory.chunks_mut(4096) {
        page[0] = 0x5a;
    }
    std::hint::black_box(&memory);
}

/// The block device whose virtio serial is `agent`.
fn agent_disk() -> Option<PathBuf> {
    fs::read_dir("/sys/block")
        .ok()?
        .flatten()
        .find_map(|entry| {
            let serial = fs::read_to_string(entry.path().join("serial")).ok()?;
            (serial.trim() == "agent").then(|| PathBuf::from("/dev").join(entry.file_name()))
        })
}

fn read_disk() -> Option<String> {
    let mut buf = [0u8; 512];
    File::open(agent_disk()?).ok()?.read_exact(&mut buf).ok()?;
    let end = buf.iter().position(|&b| b == b'\n' || b == 0).unwrap_or(0);
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}

fn write_disk(text: &str) -> io::Result<()> {
    let disk = agent_disk().ok_or_else(|| io::Error::other("no disk with serial `agent`"))?;
    let mut block = [0u8; 512];
    let line = format!("{text}\n");
    let n = line.len().min(block.len());
    block[..n].copy_from_slice(&line.as_bytes()[..n]);
    // Through the page cache, unsynced: the snapshot must carry it.
    OpenOptions::new()
        .write(true)
        .open(disk)?
        .write_all_at(&block, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_count_up() {
        let first = answer("tick")["count"].as_u64().unwrap();
        assert_eq!(answer("tick")["count"].as_u64(), Some(first + 1));
        assert_eq!(answer("status")["count"].as_u64(), Some(first + 1));
    }

    #[test]
    fn status_has_every_field() {
        let status = answer("status");
        for key in ["count", "boot_id", "mac", "share", "disk", "uptime_ms"] {
            assert!(status.get(key).is_some(), "{key} in {status}");
        }
        // This host has a boot id and an uptime, if no `agent` disk.
        assert!(status["boot_id"].is_string(), "{status}");
        assert!(status["uptime_ms"].as_u64().unwrap() > 0);
        assert!(status["disk"].is_null());
    }

    #[test]
    fn dirty_touches_and_frees() {
        assert_eq!(answer("dirty 4"), json!({ "dirtied_mib": 4 }));
        assert!(answer("dirty lots")["error"].is_string());
    }

    #[test]
    fn unknown_commands_and_missing_disks_say_so() {
        assert_eq!(
            answer("dance"),
            json!({ "error": "unknown command `dance`" })
        );
        assert_eq!(
            answer("write-disk x"),
            json!({ "error": "no disk with serial `agent`" })
        );
    }
}
