//! What the host's memory is doing (#47). VibeOS never admits or refuses an
//! agent by memory: each agent's memory is a ceiling it's held to (a VM's
//! size, a cgroup's `memory.max`, the governor's limit), and the host swaps
//! if its agents use more than it has. This is what it reports instead: the
//! host's memory used and total, and its swap, for `hosts.list`, MCP and
//! the menu's header; and, while the kernel says memory is short, a
//! warning that blocks nothing.

use std::sync::Arc;

use vibe_core::rpc::MemoryUse;

use crate::agents::size;
use crate::core::Core;
use crate::pressure::{Level, Reading};

/// Measures the host's memory: [`measure`], by default. Its `pressure` is
/// left for [`Core::memory_use`] to fill in.
pub type MemoryGauge = Arc<dyn Fn() -> Option<MemoryUse> + Send + Sync>;

/// Below this share of its memory available, a host without pressure
/// stall information counts as short of memory.
const LOW_AVAILABLE: f64 = 0.05;

impl Core {
    /// The host's memory now, with a warning while it's short: the kernel's
    /// pressure (macOS's at warn or critical, Linux's stall information
    /// past its marks), or, where the kernel doesn't say, under 5% left.
    pub fn memory_use(&self) -> Option<MemoryUse> {
        let mut m = (self.memory_gauge)()?;
        m.pressure = match self.pressure.read() {
            Some(r) => warns(&r).then(|| r.to_string()),
            None => {
                let available = m.total.saturating_sub(m.used);
                (m.total > 0 && (available as f64) < m.total as f64 * LOW_AVAILABLE)
                    .then(|| format!("only {} of its {} is free", size(available), size(m.total)))
            }
        };
        Some(m)
    }
}

/// Whether `r` is worth a warning: more than [`Reading::high`], which is
/// what acts on agents, as macOS's warn level is worth knowing of too.
fn warns(r: &Reading) -> bool {
    match r {
        Reading::Psi(p) => p.high(),
        Reading::Level(l) => *l != Level::Normal,
    }
}

/// This machine's memory, or `None` where it can't be read.
pub fn measure() -> Option<MemoryUse> {
    platform::measure()
}

/// From `/proc/meminfo`: in use is all but `MemAvailable`, which the kernel
/// counts as what a new program could have without swapping.
pub fn parse_meminfo(text: &str) -> Option<MemoryUse> {
    let field = |name: &str| -> Option<u64> {
        let rest = text
            .lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))?;
        let mut words = rest.split_whitespace();
        let n: u64 = words.next()?.parse().ok()?;
        Some(match words.next() {
            Some("kB") => n.saturating_mul(1024),
            _ => n,
        })
    };
    let total = field("MemTotal")?;
    let available = field("MemAvailable")?;
    let swap_total = field("SwapTotal").unwrap_or(0);
    let swap_free = field("SwapFree").unwrap_or(swap_total);
    Some(MemoryUse {
        total,
        used: total.saturating_sub(available),
        swap_total,
        swap_used: swap_total.saturating_sub(swap_free),
        pressure: None,
    })
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use vibe_core::rpc::MemoryUse;

    pub(super) fn measure() -> Option<MemoryUse> {
        super::parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use vibe_core::rpc::MemoryUse;

    unsafe extern "C" {
        // libc has it, deprecated in favour of the mach2 crate.
        fn mach_host_self() -> libc::mach_port_t;
    }

    /// The host port, got once: each `mach_host_self` adds a reference to
    /// it that's never given back.
    fn host_port() -> libc::mach_port_t {
        static PORT: std::sync::OnceLock<libc::mach_port_t> = std::sync::OnceLock::new();
        // SAFETY: mach_host_self has no preconditions.
        *PORT.get_or_init(|| unsafe { mach_host_self() })
    }

    /// As Activity Monitor counts it, and as the menu did before vibed
    /// reported it: app memory (internal pages, less purgeable ones), wired
    /// and compressed.
    pub(super) fn measure() -> Option<MemoryUse> {
        let total = sysctl::<u64>(c"hw.memsize")?;
        let mut stats: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        // SAFETY: a vm_statistics64 to fill, with its size in natural_t's.
        let rc = unsafe {
            libc::host_statistics64(
                host_port(),
                libc::HOST_VM_INFO64,
                (&raw mut stats).cast(),
                &mut count,
            )
        };
        if rc != libc::KERN_SUCCESS {
            return None;
        }
        // SAFETY: sysconf has no preconditions.
        let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
        let app =
            u64::from(stats.internal_page_count).saturating_sub(u64::from(stats.purgeable_count));
        let used = (app + u64::from(stats.wire_count) + u64::from(stats.compressor_page_count))
            .saturating_mul(page);
        let swap = sysctl::<libc::xsw_usage>(c"vm.swapusage");
        Some(MemoryUse {
            total,
            used: used.min(total),
            swap_total: swap.map_or(0, |s| s.xsu_total),
            swap_used: swap.map_or(0, |s| s.xsu_used),
            pressure: None,
        })
    }

    /// A sysctl whose value is a `T`.
    fn sysctl<T: Copy>(name: &std::ffi::CStr) -> Option<T> {
        let mut value = std::mem::MaybeUninit::<T>::zeroed();
        let mut len = std::mem::size_of::<T>();
        // SAFETY: a T for sysctl to fill, and its size; it's read only if
        // sysctl filled all of it.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0 && len == std::mem::size_of::<T>()).then(|| unsafe { value.assume_init() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vibe_core::units::{GIB, MIB};

    #[test]
    fn meminfo_counts_all_but_what_is_available() {
        let text = "MemTotal:       12288000 kB\nMemFree:          512000 kB\n\
                    MemAvailable:    4096000 kB\nBuffers: 1 kB\n\
                    SwapTotal:       2097152 kB\nSwapFree:        1048576 kB\n";
        let m = parse_meminfo(text).unwrap();
        assert_eq!(m.total, 12_288_000 * 1024);
        assert_eq!(m.used, (12_288_000 - 4_096_000) * 1024);
        assert_eq!((m.swap_total, m.swap_used), (2 * GIB, GIB));
        assert_eq!(m.pressure, None);
        // No swap, and a kernel too old for MemAvailable says nothing.
        let none = parse_meminfo("MemTotal: 1024 kB\nMemAvailable: 512 kB\n").unwrap();
        assert_eq!((none.swap_total, none.swap_used), (0, 0));
        assert_eq!(parse_meminfo("MemTotal: 1024 kB\nMemFree: 512 kB\n"), None);
    }

    #[test]
    fn this_machine_measures() {
        let m = measure().expect("this machine's memory");
        assert!(m.total >= 256 * MIB, "{m:?}");
        assert!(m.used > 0 && m.used <= m.total, "{m:?}");
        assert!(m.swap_used <= m.swap_total, "{m:?}");
    }

    #[test]
    fn macos_warn_is_worth_a_warning_and_normal_is_not() {
        assert!(warns(&Reading::Level(Level::Warn)));
        assert!(warns(&Reading::Level(Level::Critical)));
        assert!(!warns(&Reading::Level(Level::Normal)));
    }
}
