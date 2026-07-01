//! The menu-bar (`NSStatusItem`) control for the launcher: a template glyph in
//! the system menu bar whose click opens a menu of actions (Open, Settings, any
//! extension items, Launch at Login, Quit).
//!
//! This module is deliberately gpui-free — it takes plain `Box<dyn Fn()>`
//! callbacks. Like [`crate::hotkey`], those callbacks run on the main thread when
//! the user clicks, so the UI layer bridges them into gpui via `AsyncApp::spawn`.
//!
//! Everything here must be created and used on the main thread, after
//! `NSApplication` exists (i.e. inside gpui's run closure).

use std::ffi::{c_void, CString};
use std::sync::Once;

use objc::declare::ClassDecl;
use objc::runtime::{Class, Object, Sel};
use objc::{class, msg_send, sel, sel_impl};

/// One entry in the status-bar menu.
pub enum MenuItem {
    /// A divider line.
    Separator,
    /// A clickable command.
    Action {
        title: String,
        /// Live checkbox state, queried each time the menu opens (`None` = a
        /// plain item, no checkmark).
        checked: Option<Box<dyn Fn() -> bool>>,
        /// Runs on the main thread when the item is clicked.
        on_click: Box<dyn Fn()>,
    },
}

/// Backing store for one clickable menu item, indexed by the `NSMenuItem`'s tag.
struct Slot {
    checked: Option<Box<dyn Fn() -> bool>>,
    on_click: Box<dyn Fn()>,
}

/// The dispatch table pointed at by the Objective-C target's ivar. Boxed and its
/// raw pointer stashed in the target so the action/delegate methods can reach it.
struct Callbacks {
    slots: Vec<Slot>,
}

/// A live `NSStatusItem`. Drop removes it from the menu bar, so callers that want
/// it to last the whole session should `Box::leak` it (as `run()` does).
pub struct StatusBar {
    status_item: *mut Object,
    /// Kept alive because the Objective-C target holds a raw pointer into it.
    _callbacks: Box<Callbacks>,
    /// The target/delegate object (retained); released on drop.
    target: *mut Object,
}

const NS_CONTROL_STATE_ON: i64 = 1;
const NS_CONTROL_STATE_OFF: i64 = 0;
/// `NSVariableStatusItemLength` — size the item to its content.
const NS_VARIABLE_STATUS_ITEM_LENGTH: f64 = -1.0;

impl StatusBar {
    /// Install a status-bar item showing `icon_rgba` (`w`×`h` premultiplied RGBA,
    /// treated as a template image) with the given menu.
    pub fn new(icon_rgba: &[u8], w: u32, h: u32, items: Vec<MenuItem>) -> Self {
        unsafe {
            // SAFETY: all AppKit calls below are on the main thread against the
            // process-wide NSStatusBar / freshly-allocated objects.
            let status_bar: *mut Object = msg_send![class!(NSStatusBar), systemStatusBar];
            let status_item: *mut Object =
                msg_send![status_bar, statusItemWithLength: NS_VARIABLE_STATUS_ITEM_LENGTH];
            let status_item: *mut Object = msg_send![status_item, retain];

            if let Some(image) = template_image(icon_rgba, w, h) {
                let button: *mut Object = msg_send![status_item, button];
                if !button.is_null() {
                    let _: () = msg_send![button, setImage: image];
                }
            }

            // Build the menu and the dispatch table in lock-step: each `Action`
            // gets a tag equal to its index in `slots`; separators get -1.
            let target: *mut Object = msg_send![target_class(), new];
            let menu: *mut Object = msg_send![class!(NSMenu), new];
            let _: () = msg_send![menu, setAutoenablesItems: false as i8];

            let mut slots: Vec<Slot> = Vec::new();
            for item in items {
                match item {
                    MenuItem::Separator => {
                        let sep: *mut Object = msg_send![class!(NSMenuItem), separatorItem];
                        let _: () = msg_send![sep, setTag: -1i64];
                        let _: () = msg_send![menu, addItem: sep];
                    }
                    MenuItem::Action {
                        title,
                        checked,
                        on_click,
                    } => {
                        let tag = slots.len() as i64;
                        let ns_title = ns_string(&title);
                        let empty = ns_string("");
                        let mi: *mut Object = msg_send![class!(NSMenuItem), alloc];
                        let mi: *mut Object = msg_send![
                            mi,
                            initWithTitle: ns_title
                            action: sel!(handleMenuAction:)
                            keyEquivalent: empty
                        ];
                        let _: () = msg_send![mi, setTarget: target];
                        let _: () = msg_send![mi, setTag: tag];
                        let _: () = msg_send![menu, addItem: mi];
                        slots.push(Slot { checked, on_click });
                    }
                }
            }

            // Hand the target a raw pointer to the (boxed) dispatch table, and let
            // the menu's delegate refresh checkmarks each time it opens.
            let callbacks = Box::new(Callbacks { slots });
            let raw = &*callbacks as *const Callbacks as *mut c_void;
            (*target).set_ivar::<*mut c_void>(IVAR, raw);
            let _: () = msg_send![menu, setDelegate: target];
            let _: () = msg_send![status_item, setMenu: menu];

            StatusBar {
                status_item,
                _callbacks: callbacks,
                target,
            }
        }
    }
}

impl Drop for StatusBar {
    fn drop(&mut self) {
        // SAFETY: we own these; remove the item from the bar and release.
        unsafe {
            let status_bar: *mut Object = msg_send![class!(NSStatusBar), systemStatusBar];
            let _: () = msg_send![status_bar, removeStatusItem: self.status_item];
            let _: () = msg_send![self.status_item, release];
            let _: () = msg_send![self.target, release];
        }
    }
}

// --- Objective-C target class ---------------------------------------------

const IVAR: &str = "spotlightCallbacks";

/// The action method: read the sender's tag and fire the matching callback.
extern "C" fn handle_menu_action(this: &Object, _sel: Sel, sender: *mut Object) {
    unsafe {
        let raw = *this.get_ivar::<*mut c_void>(IVAR);
        if raw.is_null() {
            return;
        }
        let callbacks = &*(raw as *const Callbacks);
        let tag: i64 = msg_send![sender, tag];
        if let Some(slot) = usize::try_from(tag).ok().and_then(|i| callbacks.slots.get(i)) {
            (slot.on_click)();
        }
    }
}

/// `NSMenuDelegate` hook: refresh every checkbox item's state before display.
extern "C" fn menu_needs_update(this: &Object, _sel: Sel, menu: *mut Object) {
    unsafe {
        let raw = *this.get_ivar::<*mut c_void>(IVAR);
        if raw.is_null() {
            return;
        }
        let callbacks = &*(raw as *const Callbacks);
        let count: usize = msg_send![menu, numberOfItems];
        for i in 0..count {
            let item: *mut Object = msg_send![menu, itemAtIndex: i as i64];
            let tag: i64 = msg_send![item, tag];
            let Some(slot) = usize::try_from(tag).ok().and_then(|i| callbacks.slots.get(i)) else {
                continue;
            };
            if let Some(check) = &slot.checked {
                let state = if check() {
                    NS_CONTROL_STATE_ON
                } else {
                    NS_CONTROL_STATE_OFF
                };
                let _: () = msg_send![item, setState: state];
            }
        }
    }
}

/// Register (once) and return the target/delegate class.
fn target_class() -> &'static Class {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        let mut decl = ClassDecl::new("SpotlightStatusTarget", class!(NSObject))
            .expect("SpotlightStatusTarget already registered");
        decl.add_ivar::<*mut c_void>(IVAR);
        unsafe {
            decl.add_method(
                sel!(handleMenuAction:),
                handle_menu_action as extern "C" fn(&Object, Sel, *mut Object),
            );
            decl.add_method(
                sel!(menuNeedsUpdate:),
                menu_needs_update as extern "C" fn(&Object, Sel, *mut Object),
            );
        }
        decl.register();
    });
    Class::get("SpotlightStatusTarget").expect("SpotlightStatusTarget registered above")
}

// --- helpers --------------------------------------------------------------

/// Build a template `NSImage` (autoreleased) from premultiplied RGBA pixels, or
/// `None` on failure. Template images are tinted by macOS for the menu bar.
unsafe fn template_image(rgba: &[u8], w: u32, h: u32) -> Option<*mut Object> {
    if rgba.len() < (w * h * 4) as usize {
        return None;
    }
    let cs_name = ns_string("NSDeviceRGBColorSpace");
    let rep: *mut Object = msg_send![class!(NSBitmapImageRep), alloc];
    // `bitmapFormat: 0` == premultiplied-alpha, RGBA byte order (matches resvg).
    let rep: *mut Object = msg_send![
        rep,
        initWithBitmapDataPlanes: std::ptr::null_mut::<*mut u8>()
        pixelsWide: w as usize
        pixelsHigh: h as usize
        bitsPerSample: 8usize
        samplesPerPixel: 4usize
        hasAlpha: 1i8
        isPlanar: 0i8
        colorSpaceName: cs_name
        bitmapFormat: 0usize
        bytesPerRow: (w * 4) as usize
        bitsPerPixel: 32usize
    ];
    if rep.is_null() {
        return None;
    }
    // Copy our pixels into the rep's own buffer (so we needn't keep `rgba`).
    let dst: *mut u8 = msg_send![rep, bitmapData];
    if dst.is_null() {
        let _: () = msg_send![rep, release];
        return None;
    }
    std::ptr::copy_nonoverlapping(rgba.as_ptr(), dst, (w * h * 4) as usize);

    let size = NSSize {
        // Point size = pixels / 2 so the 2× rep renders crisply at menu-bar scale.
        width: w as f64 / 2.0,
        height: h as f64 / 2.0,
    };
    let image: *mut Object = msg_send![class!(NSImage), alloc];
    let image: *mut Object = msg_send![image, initWithSize: size];
    let _: () = msg_send![image, addRepresentation: rep];
    let _: () = msg_send![rep, release];
    let _: () = msg_send![image, setTemplate: true as i8];
    let _: () = msg_send![image, autorelease];
    Some(image)
}

/// An autoreleased `NSString` from a Rust `&str`.
unsafe fn ns_string(s: &str) -> *mut Object {
    let c = CString::new(s).unwrap_or_default();
    msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()]
}

#[repr(C)]
struct NSSize {
    width: f64,
    height: f64,
}
