//! Native NSWindow tweaks for the launcher panel, applied via the raw NSView
//! pointer obtained from gpui's `raw-window-handle` implementation.

use std::ffi::c_void;

use objc::runtime::Object;
use objc::{class, msg_send, sel, sel_impl};

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

/// Run as an "accessory" app: no Dock icon, no app menu, but still able to show
/// floating panels and receive keyboard input. This is the activation policy
/// Spotlight itself uses. Must be called on the main thread after NSApplication
/// exists (i.e. inside gpui's run closure).
pub fn set_accessory_activation_policy() {
    // SAFETY: `sharedApplication` returns the singleton NSApplication that gpui
    // already created. `setActivationPolicy:` takes an NSInteger enum value;
    // `NSApplicationActivationPolicyAccessory` == 1.
    unsafe {
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        if app.is_null() {
            return;
        }
        let policy: i64 = 1; // NSApplicationActivationPolicyAccessory
        let _: () = msg_send![app, setActivationPolicy: policy];
    }
}

/// Whether the panel owning `ns_view` is currently on screen.
pub fn panel_visible(ns_view: *mut c_void) -> bool {
    if ns_view.is_null() {
        return false;
    }
    // SAFETY: `ns_view` is a live NSView; `[window isVisible]` returns a BOOL.
    unsafe {
        let view = ns_view as *mut Object;
        let window: *mut Object = msg_send![view, window];
        if window.is_null() {
            return false;
        }
        let visible: i8 = msg_send![window, isVisible];
        visible != 0
    }
}

/// Show the panel: bring the app forward and make the window key. Safe to call
/// repeatedly or when already visible. Must be called on the main thread.
pub fn show_panel(ns_view: *mut c_void) {
    if ns_view.is_null() {
        return;
    }
    // SAFETY: well-known AppKit messages on the window and NSApp singleton.
    unsafe {
        let view = ns_view as *mut Object;
        let window: *mut Object = msg_send![view, window];
        if window.is_null() {
            return;
        }
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        if !app.is_null() {
            const YES: i8 = 1;
            // With accessory policy there's no Dock click to activate us, so we
            // explicitly activate to ensure the panel can take keyboard focus.
            let _: () = msg_send![app, activateIgnoringOtherApps: YES];
        }
        let nil: *mut Object = std::ptr::null_mut();
        let _: () = msg_send![window, makeKeyAndOrderFront: nil];
        // Re-composite the shadow now that we're back on screen.
        let _: () = msg_send![window, invalidateShadow];
    }
}

/// Hide the panel without closing it (so gpui keeps the window alive). Must be
/// called on the main thread.
pub fn hide_panel(ns_view: *mut c_void) {
    if ns_view.is_null() {
        return;
    }
    // SAFETY: `orderOut:` removes the window from screen without destroying it.
    unsafe {
        let view = ns_view as *mut Object;
        let window: *mut Object = msg_send![view, window];
        if window.is_null() {
            return;
        }
        let nil: *mut Object = std::ptr::null_mut();
        let _: () = msg_send![window, orderOut: nil];
    }
}
