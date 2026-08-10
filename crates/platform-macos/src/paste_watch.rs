//! Noticing when the user pastes, so a follow-up can be delivered into the app
//! they pasted into.
//!
//! The ⌘V is a *readiness* signal: it says a text field is focused and the
//! target app is listening, which nothing else on macOS will tell us. That is
//! what makes "copy, then paste, and the attachments follow" possible without
//! guessing where the caret is.
//!
//! Watching keys in another app means a [`CGEventTap`], which by nature sees
//! every keystroke on the system, so this one is kept deliberately narrow:
//!
//! - **listen-only**, so it can never modify or swallow what the user typed;
//! - **armed on demand** for a few seconds after an explicit user action,
//!   never at rest;
//! - **fires once**, then tears the tap down immediately;
//! - it reads only the keycode and modifier flags of the event, and keeps
//!   nothing.
//!
//! Requires the Accessibility permission, which the app already holds for
//! [`crate::input::paste`]; without it the tap simply never fires.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// core-graphics 0.24 is built on core-foundation 0.10, while the rest of this
// crate uses 0.9 — the two `CFRunLoopSource` types are not interchangeable, so
// the tap's run loop is driven through the C API directly rather than through
// either wrapper.
use cf_modern::base::TCFType;
use core_graphics::event::{
    CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, EventField,
};

use std::os::raw::c_void;

extern "C" {
    /// Whether this process may observe keyboard events. Does **not** prompt.
    fn CGPreflightListenEventAccess() -> bool;
    /// Trigger the one-time system prompt for that access.
    fn CGRequestListenEventAccess() -> bool;
    fn CFRunLoopGetCurrent() -> *const c_void;
    fn CFRunLoopAddSource(rl: *const c_void, source: *const c_void, mode: *const c_void);
    fn CFRunLoopRemoveSource(rl: *const c_void, source: *const c_void, mode: *const c_void);
    fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after_handled: u8) -> i32;
    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: *const c_void,
        order: isize,
    ) -> *const c_void;
    fn CFRelease(cf: *const c_void);
    /// Valid for *adding* a source (it stands for the set of common modes)…
    static kCFRunLoopCommonModes: *const c_void;
    /// …but only a concrete mode may be *run*; passing the common-modes marker
    /// to `CFRunLoopRunInMode` is rejected and the loop never runs.
    static kCFRunLoopDefaultMode: *const c_void;
}

/// `kVK_ANSI_V`.
const KEY_V: i64 = 0x09;

/// How often the watch thread wakes to check for cancellation/expiry.
const TICK: Duration = Duration::from_millis(50);

/// Whether the user has granted this app the right to observe keystrokes
/// (System Settings → Privacy & Security → **Input Monitoring**).
///
/// This is a different grant from the Accessibility one that [`crate::input::paste`]
/// needs: posting events and observing them are separate permissions. Checking
/// does not prompt.
pub fn can_watch() -> bool {
    unsafe { CGPreflightListenEventAccess() }
}

/// Ask for that access, showing the system prompt once.
pub fn request_access() {
    unsafe {
        CGRequestListenEventAccess();
    }
}

/// A running watch. Dropping it stops the watch (the callback will not fire
/// afterwards) and lets the tap tear down.
pub struct PasteWatch {
    cancelled: Arc<AtomicBool>,
}

impl Drop for PasteWatch {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

impl PasteWatch {
    /// Stop watching without waiting for the guard to drop.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

/// Call `on_paste` the first time the user presses ⌘V within `timeout`.
///
/// The callback runs on the watch thread once the tap has been torn down, so
/// it is free to post its own keystrokes without re-entering the tap.
pub fn watch_for_paste<F>(timeout: Duration, on_paste: F) -> PasteWatch
where
    F: FnOnce() + Send + 'static,
{
    let cancelled = Arc::new(AtomicBool::new(false));
    let watch = PasteWatch {
        cancelled: cancelled.clone(),
    };
    // Without Input Monitoring the tap would be created but never deliver an
    // event, so ask once and leave this window unarmed rather than looking
    // like it's watching when it isn't.
    if !can_watch() {
        request_access();
        return watch;
    }
    let fired = Arc::new(AtomicBool::new(false));

    std::thread::Builder::new()
        .name("paste-watch".to_string())
        .spawn({
            let fired = fired.clone();
            move || {
                {
                    // Scope the tap so it is released before `on_paste` runs.
                    let seen = fired.clone();
                    let Ok(tap) = CGEventTap::new(
                        CGEventTapLocation::Session,
                        CGEventTapPlacement::HeadInsertEventTap,
                        // Listen-only: this tap cannot alter or drop events.
                        CGEventTapOptions::ListenOnly,
                        vec![CGEventType::KeyDown],
                        move |_proxy, _type, event| {
                            let key = event.get_integer_value_field(
                                EventField::KEYBOARD_EVENT_KEYCODE,
                            );
                            let flags = event.get_flags();
                            // ⌘V, with or without Shift (paste-and-match-style).
                            if key == KEY_V
                                && flags.contains(CGEventFlags::CGEventFlagCommand)
                                && !flags.contains(CGEventFlags::CGEventFlagControl)
                            {
                                seen.store(true, Ordering::SeqCst);
                            }
                            None
                        },
                    ) else {
                        // No Accessibility permission (or the tap was refused):
                        // nothing to do — the caller's copy already happened.
                        return;
                    };

                    unsafe {
                        let port = tap.mach_port.as_concrete_TypeRef() as *const c_void;
                        let source =
                            CFMachPortCreateRunLoopSource(std::ptr::null(), port, 0);
                        if source.is_null() {
                            return;
                        }
                        let run_loop = CFRunLoopGetCurrent();
                        CFRunLoopAddSource(run_loop, source, kCFRunLoopCommonModes);
                        tap.enable();

                        let deadline = Instant::now() + timeout;
                        while Instant::now() < deadline
                            && !fired.load(Ordering::SeqCst)
                            && !cancelled.load(Ordering::SeqCst)
                        {
                            CFRunLoopRunInMode(
                                kCFRunLoopDefaultMode,
                                TICK.as_secs_f64(),
                                1,
                            );
                        }
                        CFRunLoopRemoveSource(run_loop, source, kCFRunLoopCommonModes);
                        CFRelease(source);
                    }
                }

                if fired.load(Ordering::SeqCst) && !cancelled.load(Ordering::SeqCst) {
                    on_paste();
                }
            }
        })
        .ok();

    watch
}
