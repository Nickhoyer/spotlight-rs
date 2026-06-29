//! Native NSWindow tweaks for the launcher panel, applied via the raw NSView
//! pointer obtained from gpui's `raw-window-handle` implementation.

use std::ffi::c_void;

use objc::runtime::Object;
use objc::{msg_send, sel, sel_impl};

/// Configure the launcher's window for a clean, borderless translucent look.
///
/// gpui's `WindowKind::PopUp` already gives us a non-activating panel that
/// floats over fullscreen Spaces. Here we disable the *native* drop shadow so
/// the system doesn't draw a rectangular shadow around our transparent window —
/// we render our own rounded shadow inside the panel instead.
///
/// `ns_view` is the `AppKitWindowHandle::ns_view` pointer; safe to pass null.
pub fn configure_panel(ns_view: *mut c_void) {
    if ns_view.is_null() {
        return;
    }
    // SAFETY: `ns_view` is a live NSView from gpui's window handle; `window` is
    // its owning NSWindow. We only send well-known, side-effecting messages.
    unsafe {
        let view = ns_view as *mut Object;
        let window: *mut Object = msg_send![view, window];
        if window.is_null() {
            return;
        }
        const NO: i8 = 0;
        let _: () = msg_send![window, setHasShadow: NO];
        let _: () = msg_send![window, invalidateShadow];
    }
}
