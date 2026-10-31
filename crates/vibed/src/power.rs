//! Keeping the Mac from sleeping while any agent is awake (PRD: Failures,
//! System sleep): an IOPM assertion against idle system sleep, which still
//! lets the display sleep. Elsewhere there's nothing to hold.

use std::fmt;
use std::sync::Mutex;

use crate::core::lock;

/// What holds the host awake.
pub trait Power: Send + Sync + fmt::Debug {
    /// Holds the host awake, or lets it go. Called on every look at the
    /// agents, so it does nothing when nothing changes.
    fn keep_awake(&self, on: bool);
}

/// Nothing to hold: a Linux host, and tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoPower;

impl Power for NoPower {
    fn keep_awake(&self, _on: bool) {}
}

/// This host's: an IOPM assertion on macOS, nothing elsewhere.
#[derive(Debug, Default)]
pub struct HostPower {
    /// The assertion held, by its IOPM id.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    held: Mutex<Option<u32>>,
    /// The last error taking it gave, so it's said once.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    failed: Mutex<Option<i32>>,
}

impl Power for HostPower {
    #[cfg(target_os = "macos")]
    fn keep_awake(&self, on: bool) {
        let mut held = lock(&self.held);
        match (on, *held) {
            (true, None) => match iopm::hold() {
                Ok(id) => {
                    *held = Some(id);
                    *lock(&self.failed) = None;
                    crate::log!("vibed: agents are awake, so this Mac won't sleep while idle");
                }
                Err(code) => {
                    if lock(&self.failed).replace(code) != Some(code) {
                        crate::log!("vibed: can't keep this Mac awake: IOPM error {code:#x}");
                    }
                }
            },
            (false, Some(id)) => {
                iopm::release(id);
                *held = None;
                crate::log!("vibed: no agent is awake, so this Mac may sleep again");
            }
            _ => {}
        }
    }

    /// Nothing to hold here: a Linux host has no idle sleep to stop.
    #[cfg(not(target_os = "macos"))]
    fn keep_awake(&self, _on: bool) {}
}

impl Drop for HostPower {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(id) = lock(&self.held).take() {
            iopm::release(id);
        }
    }
}

/// A record of what was asked, for tests.
#[derive(Debug, Default)]
pub struct RecordedPower(pub Mutex<Vec<bool>>);

impl RecordedPower {
    /// Whether the host is held awake now: the last call's say.
    pub fn held(&self) -> bool {
        lock(&self.0).last().copied().unwrap_or(false)
    }
}

impl Power for RecordedPower {
    fn keep_awake(&self, on: bool) {
        lock(&self.0).push(on);
    }
}

#[cfg(target_os = "macos")]
mod iopm {
    use std::ffi::{c_char, c_void};

    type CFStringRef = *const c_void;

    const UTF8: u32 = 0x0800_0100;
    const LEVEL_ON: u32 = 255;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            alloc: *const c_void,
            string: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFRelease(cf: *const c_void);
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            kind: CFStringRef,
            level: u32,
            name: CFStringRef,
            id: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(id: u32) -> i32;
    }

    /// A CFString of `s`, released when dropped.
    struct CfString(CFStringRef);

    impl CfString {
        fn new(s: &std::ffi::CStr) -> CfString {
            // SAFETY: a NUL-terminated UTF-8 string, copied.
            CfString(unsafe { CFStringCreateWithCString(std::ptr::null(), s.as_ptr(), UTF8) })
        }
    }

    impl Drop for CfString {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: made by CFStringCreateWithCString, released once.
                unsafe { CFRelease(self.0) };
            }
        }
    }

    /// Holds an assertion against idle system sleep; display sleep is
    /// still allowed. Returns its id, or IOKit's error.
    pub(super) fn hold() -> Result<u32, i32> {
        let kind = CfString::new(c"PreventUserIdleSystemSleep");
        let name = CfString::new(c"VibeOS: agents are awake");
        if kind.0.is_null() || name.0.is_null() {
            return Err(-1);
        }
        let mut id = 0u32;
        // SAFETY: valid CFStrings, and a place for the id.
        let rc = unsafe { IOPMAssertionCreateWithName(kind.0, LEVEL_ON, name.0, &mut id) };
        if rc == 0 { Ok(id) } else { Err(rc) }
    }

    pub(super) fn release(id: u32) {
        // SAFETY: an id IOPMAssertionCreateWithName gave.
        unsafe { IOPMAssertionRelease(id) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_says_the_last_call() {
        let p = RecordedPower::default();
        assert!(!p.held());
        p.keep_awake(true);
        assert!(p.held());
        p.keep_awake(false);
        assert!(!p.held());
    }

    /// On a Mac this really takes and lets go of the assertion.
    #[test]
    fn the_host_holds_and_lets_go() {
        let p = HostPower::default();
        p.keep_awake(true);
        p.keep_awake(true);
        #[cfg(target_os = "macos")]
        assert!(lock(&p.held).is_some(), "the assertion is held");
        p.keep_awake(false);
        assert!(lock(&p.held).is_none());
    }
}
