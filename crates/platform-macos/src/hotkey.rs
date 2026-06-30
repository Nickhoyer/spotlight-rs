//! Global hotkey registration via Carbon's `RegisterEventHotKey` API.
//!
//! Carbon's hotkey API is the standard way to receive a key event system-wide,
//! even when the launcher is not frontmost. The handler is delivered on the main
//! run loop, so callbacks may safely touch AppKit / main-thread state — but they
//! should *schedule* any gpui state mutation (see `AsyncApp::spawn`) rather than
//! borrowing the `App` synchronously, since the app may be mid-update when the
//! event arrives.
//!
//! All functions here must be called on the main thread.

use std::ffi::c_void;

use anyhow::{anyhow, Result};

// --- Carbon FFI -----------------------------------------------------------

/// Carbon / HIToolbox types. These are opaque pointer aliases.
type OSStatus = i32;
type EventTargetRef = *mut c_void;
type EventHandlerRef = *mut c_void;
type EventHandlerUPP = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> OSStatus;
type EventHotKeyRef = *mut c_void;
type EventHandlerCallRef = *mut c_void;
type EventRef = *mut c_void;
type OptionBits = u32;
type FourCharCode = u32;
type UInt32 = u32;

/// `EventHotKeyID { UInt32 signature; UInt32 id; }`
#[repr(C)]
#[derive(Clone, Copy)]
struct EventHotKeyID {
    signature: FourCharCode,
    id: UInt32,
}

/// `EventTypeSpec { UInt32 eventClass; UInt32 eventKind; }`
#[repr(C)]
struct EventTypeSpec {
    event_class: FourCharCode,
    event_kind: UInt32,
}

/// `kEventClassKeyboard` = `'keyb'` (big-endian FourCharCode).
const K_EVENT_CLASS_KEYBOARD: FourCharCode = 0x6b65_7962; // b"keyb"
/// `kEventHotKeyPressed` event kind.
const K_EVENT_HOT_KEY_PRESSED: UInt32 = 5;

// Carbon modifier-key bit masks (from `Events.h`). These are the values
// `RegisterEventHotKey` expects in its `inModifiers` argument.
pub const CMD_KEY: u32 = 1 << 8;
pub const SHIFT_KEY: u32 = 1 << 9;
pub const OPTION_KEY: u32 = 1 << 11;
pub const CONTROL_KEY: u32 = 1 << 12;

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn GetApplicationEventTarget() -> EventTargetRef;
    fn InstallEventHandler(
        target: EventTargetRef,
        handler: EventHandlerUPP,
        num_types: UInt32,
        types: *const EventTypeSpec,
        user_data: *mut c_void,
        out_ref: *mut EventHandlerRef,
    ) -> OSStatus;
    fn RegisterEventHotKey(
        key_code: UInt32,
        modifiers: UInt32,
        hot_key_id: EventHotKeyID,
        target: EventTargetRef,
        options: OptionBits,
        out_ref: *mut EventHotKeyRef,
    ) -> OSStatus;
    fn UnregisterEventHotKey(hot_key: EventHotKeyRef) -> OSStatus;
    fn RemoveEventHandler(handler: EventHandlerRef) -> OSStatus;
}

/// noErr.
const NO_ERR: OSStatus = 0;

// --- The C event handler --------------------------------------------------

/// Trampoline invoked by Carbon on the main thread when the hotkey fires.
/// It dereferences `user_data` to a boxed closure and calls it.
extern "C" fn hot_key_handler(
    _call_ref: EventHandlerCallRef,
    _event: EventRef,
    user_data: *mut c_void,
) -> OSStatus {
    if user_data.is_null() {
        return NO_ERR;
    }
    // SAFETY: `user_data` points to a `Box<Box<dyn FnMut()>>` owned by the
    // `GlobalHotkey` that installed this handler; it stays alive until Drop.
    let cb = unsafe { &mut *(user_data as *mut Box<dyn FnMut()>) };
    cb();
    NO_ERR
}

// --- Public handle --------------------------------------------------------

/// A registered system-wide hotkey. Unregisters on drop.
///
/// Must be constructed on the main thread and kept alive for as long as the
/// hotkey should remain active. For an app-lifetime hotkey, leak the box:
/// `Box::leak(Box::new(GlobalHotkey::register(...)?))`.
pub struct GlobalHotkey {
    event_handler: EventHandlerRef,
    hot_key_ref: EventHotKeyRef,
    /// Owns the user callback; reclaimed in Drop so the raw pointer stays valid
    /// until we unregister (Carbon must not call into freed memory).
    _callback: Box<Box<dyn FnMut()>>,
}

impl GlobalHotkey {
    /// Register a global hotkey.
    ///
    /// `key_code` is a macOS virtual key code; `modifiers` is a bitmask of the
    /// `*_KEY` constants in this module. `on_fire` runs on the main thread.
    pub fn register(
        key_code: u32,
        modifiers: u32,
        on_fire: Box<dyn FnMut()>,
    ) -> Result<Self> {
        unsafe {
            let target = GetApplicationEventTarget();
            if target.is_null() {
                return Err(anyhow!("GetApplicationEventTarget returned null"));
            }

            // Box the closure twice: the inner `Box<dyn FnMut()>` is what the
            // handler derefs, the outer `Box` gives us a stable address to pass
            // as `user_data`.
            let mut callback: Box<Box<dyn FnMut()>> = Box::new(Box::new(on_fire));
            let user_data = &mut *callback as *mut Box<dyn FnMut()> as *mut c_void;

            let event_type = EventTypeSpec {
                event_class: K_EVENT_CLASS_KEYBOARD,
                event_kind: K_EVENT_HOT_KEY_PRESSED,
            };
            let mut event_handler: EventHandlerRef = std::ptr::null_mut();
            let status = InstallEventHandler(
                target,
                hot_key_handler,
                1,
                &event_type,
                user_data,
                &mut event_handler,
            );
            if status != NO_ERR {
                return Err(anyhow!("InstallEventHandler failed: {status}"));
            }

            // A throwaway signature/id; we don't read it back from the event.
            let hot_key_id = EventHotKeyID {
                signature: u32::from_be_bytes(*b"spot"),
                id: 1,
            };
            let mut hot_key_ref: EventHotKeyRef = std::ptr::null_mut();
            let status = RegisterEventHotKey(
                key_code,
                modifiers,
                hot_key_id,
                target,
                0,
                &mut hot_key_ref,
            );
            if status != NO_ERR {
                // Best-effort cleanup of the installed handler.
                RemoveEventHandler(event_handler);
                return Err(anyhow!(
                    "RegisterEventHotKey failed: {status} (another app may own this combo)"
                ));
            }

            Ok(GlobalHotkey {
                event_handler,
                hot_key_ref,
                _callback: callback,
            })
        }
    }
}

impl Drop for GlobalHotkey {
    fn drop(&mut self) {
        // SAFETY: we own these refs and only drop once.
        unsafe {
            UnregisterEventHotKey(self.hot_key_ref);
            RemoveEventHandler(self.event_handler);
        }
    }
}

// --- Spec parser ----------------------------------------------------------

/// Parse a hotkey spec like `"alt+space"`, `"cmd+shift+space"`, `"ctrl+j"`.
///
/// Modifier tokens (case-insensitive): `cmd`/`super`, `shift`, `alt`/`option`/
/// `opt`, `ctrl`/`control`. The final token is the key: a single letter, a
/// digit, or one of `space`, `return`/`enter`, `esc`/`escape`, `tab`, `f1`..`f12`.
///
/// Returns `(virtual_keycode, carbon_modifier_bits)`. At least one modifier is
/// required (system-wide hotkeys without modifiers are disallowed by macOS).
pub fn parse(spec: &str) -> Result<(u32, u32)> {
    let parts: Vec<&str> = spec.split('+').map(str::trim).collect();
    if parts.len() < 2 {
        return Err(anyhow!(
            "hotkey spec `{spec}` must be `mod[+mod...]+key`, e.g. `alt+space`"
        ));
    }

    let mut modifiers = 0u32;
    for tok in &parts[..parts.len() - 1] {
        modifiers |= match tok.to_ascii_lowercase().as_str() {
            "cmd" | "super" | "command" => CMD_KEY,
            "shift" => SHIFT_KEY,
            "alt" | "option" | "opt" => OPTION_KEY,
            "ctrl" | "control" => CONTROL_KEY,
            other => return Err(anyhow!("unknown modifier `{other}`")),
        };
    }
    if modifiers == 0 {
        return Err(anyhow!("at least one modifier is required"));
    }

    let key_tok = parts[parts.len() - 1].to_ascii_lowercase();
    let key_code = keycode(&key_tok)
        .ok_or_else(|| anyhow!("unknown key `{key_tok}` (use a-z, 0-9, space, return, esc, tab, f1-f12)"))?;

    Ok((key_code, modifiers))
}

/// Map a key token (already lowercased) to a macOS virtual keycode (US layout).
fn keycode(tok: &str) -> Option<u32> {
    if tok.len() == 1 {
        if let Some(ch) = tok.chars().next() {
            return match ch {
                'a' => Some(0),
                's' => Some(1),
                'd' => Some(2),
                'f' => Some(3),
                'h' => Some(4),
                'g' => Some(5),
                'z' => Some(6),
                'x' => Some(7),
                'c' => Some(8),
                'v' => Some(9),
                'b' => Some(11),
                'q' => Some(12),
                'w' => Some(13),
                'e' => Some(14),
                'r' => Some(15),
                'y' => Some(16),
                't' => Some(17),
                '1' => Some(18),
                '2' => Some(19),
                '3' => Some(20),
                '4' => Some(21),
                '6' => Some(22),
                '5' => Some(23),
                '=' | '+' => Some(24),
                '9' => Some(25),
                '7' => Some(26),
                '-' | '_' => Some(27),
                '8' => Some(28),
                '0' => Some(29),
                'o' => Some(31),
                'u' => Some(32),
                'i' => Some(34),
                'p' => Some(35),
                'l' => Some(37),
                'j' => Some(38),
                'k' => Some(40),
                'n' => Some(45),
                'm' => Some(46),
                _ => None,
            };
        }
    }
    Some(match tok {
        "space" => 49,
        "return" | "enter" => 36,
        "esc" | "escape" => 53,
        "tab" => 48,
        "f1" => 122,
        "f2" => 120,
        "f3" => 99,
        "f4" => 118,
        "f5" => 96,
        "f6" => 97,
        "f7" => 98,
        "f8" => 100,
        "f9" => 101,
        "f10" => 109,
        "f11" => 103,
        "f12" => 111,
        _ => return None,
    })
}
