//! Native NSWindow tweaks for the launcher panel, applied via the raw NSView
//! pointer obtained from gpui's `raw-window-handle` implementation.

use std::ffi::c_void;

use objc::runtime::Object;
use objc::{class, msg_send, sel, sel_impl};

/// Configure the launcher's window for a clean, borderless translucent look.
///
/// gpui's `WindowKind::PopUp` already gives us a non-activating panel that
/// floats over fullscreen Spaces, but it leaves two things we have to undo:
///
/// * **Style mask.** For `titlebar: None` gpui builds the window titled
///   (`NSTitledWindowMask | NSFullSizeContentViewWindowMask`), so `NSThemeFrame`
///   still exists and still paints a frame along the window's top edge. Our
///   window is much larger than the visible panel (it carries transparent slack
///   for the open spring and the exit drop), so that frame shows up as a stray
///   beam floating well above the panel. We drop down to borderless, keeping
///   only the non-activating-panel bit that the paste-back flow depends on (see
///   [`show_panel`]). Geometry is unchanged: with `FullSizeContentView` the
///   content rect already equalled the frame.
/// * **Background color.** gpui paints non-opaque windows at sRGB alpha 0.0001
///   rather than `clearColor`, deliberately, "to avoid broken shadow". That
///   near-zero alpha still counts as *covered* for the window server's hit test,
///   so the transparent margin around the panel swallows every click instead of
///   letting it through to the app underneath — which also meant clicking there
///   never resigned key, and so never dismissed the launcher. We have no native
///   shadow to keep well-formed (we disable it below and draw our own rounded
///   one inside the panel), so `clearColor` is safe here and makes the alpha-0
///   region a genuine hole.
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
        // NSWindowStyleMaskBorderless (0) | NSWindowStyleMaskNonactivatingPanel.
        const BORDERLESS_NONACTIVATING_PANEL: u64 = 1 << 7;
        let _: () = msg_send![window, setStyleMask: BORDERLESS_NONACTIVATING_PANEL];
        // Changing the style mask rebuilds the window frame, which can drop the
        // first responder and the movable flag gpui set at creation time.
        let _: () = msg_send![window, makeFirstResponder: view];
        let _: () = msg_send![window, setMovable: NO];

        let clear: *mut Object = msg_send![class!(NSColor), clearColor];
        let _: () = msg_send![window, setBackgroundColor: clear];

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

/// Show the panel and make it key so it receives keystrokes. Safe to call
/// repeatedly or when already visible. Must be called on the main thread.
///
/// We deliberately do NOT call `activateIgnoringOtherApps:` — that would make our
/// app the active app and steal focus from whatever the user was typing in.
/// gpui's `WindowKind::PopUp` is a non-activating `NSPanel`, which can become the
/// key window (and take keyboard input) while the previous app stays active, so
/// hiding the panel returns focus to that app — the behavior a paste-back needs.
pub fn show_panel(ns_view: *mut c_void) {
    if ns_view.is_null() {
        return;
    }
    // SAFETY: well-known AppKit messages on the window.
    unsafe {
        let view = ns_view as *mut Object;
        let window: *mut Object = msg_send![view, window];
        if window.is_null() {
            return;
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
