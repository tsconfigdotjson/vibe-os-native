//! The guest side: a vsock server in PID 1.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use crate::PORT;

static COUNT: AtomicU64 = AtomicU64::new(0);

/// Memory `hold` keeps in use until `release`.
static HELD: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// Serves [`PORT`] for ever. As PID 1 it must never return: if it can't
/// serve, it says why on the console and waits, so the test that boots it
/// times out with the reason in the console log.
pub(crate) fn run() -> ! {
    match listen() {
        Ok(listener) => {
            println!("vibe-testguest: serving vsock port {PORT}");
            // For the console log: when DHCP has given eth0 an address.
            std::thread::spawn(|| {
                for _ in 0..300 {
                    if let Some(ip) = ipv4("eth0") {
                        println!("vibe-testguest: eth0 has {ip}");
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                println!("vibe-testguest: eth0 has no address after 30s");
            });
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
            "cpus": std::thread::available_parallelism().map(|n| n.get()).ok(),
            "rng": read_trimmed("/sys/class/misc/hw_random/rng_current"),
            "ip": ipv4("eth0"),
            "mem_free_kib": meminfo("MemFree"),
            "held_mib": HELD.lock().unwrap().iter().map(|m| m.len() >> 20).sum::<usize>(),
            "held_sum": HELD.lock().unwrap().iter().fold(0u64, |sum, m| sum.wrapping_add(checksum(m))),
            "balloon": balloon(),
        }),
        "sync" => {
            // SAFETY: sync takes no arguments and can't fail.
            unsafe { libc::sync() };
            json!({ "synced": true })
        }
        "dirty" => match arg.parse::<usize>() {
            Ok(mib) => {
                let sum = checksum(&filled(mib));
                json!({ "dirtied_mib": mib, "sum": sum })
            }
            Err(_) => json!({ "error": "dirty takes a number of MiB" }),
        },
        "hold" => match arg.parse::<usize>() {
            Ok(mib) => {
                let memory = filled(mib);
                let sum = checksum(&memory);
                HELD.lock().unwrap().push(memory);
                json!({ "held_mib": mib, "sum": sum })
            }
            Err(_) => json!({ "error": "hold takes a number of MiB" }),
        },
        "release" => {
            HELD.lock().unwrap().clear();
            json!({ "held_mib": 0 })
        }
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

/// The IPv4 address DHCP gave `interface`, if any.
fn ipv4(interface: &str) -> Option<String> {
    let mut addrs: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills a list this function frees before returning,
    // and every pointer read is checked for null first.
    unsafe {
        if libc::getifaddrs(&mut addrs) != 0 {
            return None;
        }
        let mut found = None;
        let mut cur = addrs;
        while !cur.is_null() {
            let ifa = &*cur;
            let inet =
                !ifa.ifa_addr.is_null() && i32::from((*ifa.ifa_addr).sa_family) == libc::AF_INET;
            if inet && std::ffi::CStr::from_ptr(ifa.ifa_name).to_bytes() == interface.as_bytes() {
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                found =
                    Some(std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)).to_string());
                break;
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(addrs);
        found
    }
}

fn uptime_ms() -> Option<u64> {
    let text = fs::read_to_string("/proc/uptime").ok()?;
    let secs: f64 = text.split_whitespace().next()?.parse().ok()?;
    Some((secs * 1000.0) as u64)
}

/// `mib` MiB, every byte of it written with pseudo-random data. A page
/// that is mostly zeros costs a snapshot next to nothing, so it would prove
/// nothing about what a snapshot holds; these pages don't compress. `dirty`
/// frees them again at once: the allocation is large enough that musl maps
/// it on its own and unmaps it on free, giving the pages back to the guest
/// kernel. `hold` keeps them.
fn filled(mib: usize) -> Vec<u8> {
    let mut memory = vec![0u8; mib << 20];
    // xorshift64*, seeded afresh for every fill, so no two fills match and
    // none could be shared between them.
    static FILLS: AtomicU64 = AtomicU64::new(1);
    let mut x = FILLS
        .fetch_add(1, Ordering::SeqCst)
        .wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let (words, _) = memory.as_chunks_mut::<8>();
    for word in words {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        *word = x.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes();
    }
    memory
}

/// Reads every byte, so none of the writes can be optimised away.
fn checksum(memory: &[u8]) -> u64 {
    let (words, _) = memory.as_chunks::<8>();
    words
        .iter()
        .fold(0u64, |sum, w| sum.wrapping_add(u64::from_le_bytes(*w)))
}

/// A field of /proc/meminfo, in KiB.
fn meminfo(field: &str) -> Option<u64> {
    let text = fs::read_to_string("/proc/meminfo").ok()?;
    let line = text.lines().find(|l| l.starts_with(&format!("{field}:")))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// The virtio balloon's negotiated feature bits, and whether they include
/// free page reporting (bit 5), through which the guest hands freed pages to
/// the host on its own, a couple of seconds after freeing them.
fn balloon() -> Value {
    let Ok(devices) = fs::read_dir("/sys/bus/virtio/devices") else {
        return Value::Null;
    };
    for device in devices.flatten() {
        let id = fs::read_to_string(device.path().join("device")).unwrap_or_default();
        if id.trim() == "0x0005" {
            let features = read_trimmed(&device.path().join("features").to_string_lossy());
            let bit = |n: usize| features.as_deref().and_then(|f| f.chars().nth(n)) == Some('1');
            return json!({
                "features": features,
                "deflate_on_oom": bit(2),
                "free_page_hint": bit(3),
                "free_page_reporting": bit(5),
            });
        }
    }
    Value::Null
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
        for key in [
            "count",
            "boot_id",
            "mac",
            "share",
            "disk",
            "uptime_ms",
            "cpus",
            "rng",
            "ip",
            "mem_free_kib",
            "held_mib",
            "held_sum",
            "balloon",
        ] {
            assert!(status.get(key).is_some(), "{key} in {status}");
        }
        // This host has a boot id and an uptime, if no `agent` disk.
        assert!(status["boot_id"].is_string(), "{status}");
        assert!(status["uptime_ms"].as_u64().unwrap() > 0);
        assert!(status["disk"].is_null());
        assert!(status["cpus"].as_u64().unwrap() >= 1);
        assert_eq!(answer("sync"), json!({ "synced": true }));
    }

    #[test]
    fn dirty_hold_and_release() {
        let dirty = answer("dirty 4");
        assert_eq!(dirty["dirtied_mib"], 4);
        assert!(dirty["sum"].is_u64());
        assert!(answer("dirty lots")["error"].is_string());
        let two = answer("hold 2")["sum"].as_u64().unwrap();
        let three = answer("hold 3")["sum"].as_u64().unwrap();
        let status = answer("status");
        assert_eq!(status["held_mib"], 5);
        assert_eq!(status["held_sum"].as_u64(), Some(two.wrapping_add(three)));
        assert_eq!(answer("release"), json!({ "held_mib": 0 }));
        assert_eq!(answer("status")["held_mib"], 0);
        assert!(answer("hold x")["error"].is_string());
    }

    #[test]
    fn fills_are_random_enough_not_to_compress() {
        let memory = filled(1);
        // No byte value takes more than a sliver of the MiB, and no two
        // fills are the same.
        let mut counts = [0usize; 256];
        memory.iter().for_each(|&b| counts[b as usize] += 1);
        assert!(counts.iter().all(|&c| c < 8192), "{:?}", &counts[..4]);
        assert_ne!(filled(1), memory);
        assert!(meminfo("MemFree").unwrap() > 0);
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
