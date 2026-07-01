//! Synthetic keyboard input — used to "paste" (⌘V) a chosen clipboard entry
//! into whatever app was frontmost before the launcher appeared.
//!
//! Posting keystrokes into another application requires the **Accessibility**
//! permission (System Settings → Privacy & Security → Accessibility). [`paste`]
//! checks for it and, if missing, triggers the one-time system prompt and does
//! nothing else that run — the copy still happened, so the user can ⌘V manually
//! until they grant it.

use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

/// `kVK_ANSI_V`.
const KEY_V: u16 = 0x09;

extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: core_foundation::dictionary::CFDictionaryRef) -> bool;
}

/// Whether this process may post events to other apps.
fn is_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// Trigger the system "grant Accessibility access" prompt (shown once).
fn prompt_for_trust() {
    // The documented value of `kAXTrustedCheckOptionPrompt`.
    let key = CFString::from_static_string("AXTrustedCheckOptionPrompt");
    let opts = CFDictionary::from_CFType_pairs(&[(
        key.as_CFType(),
        CFBoolean::true_value().as_CFType(),
    )]);
    unsafe {
        AXIsProcessTrustedWithOptions(opts.as_concrete_TypeRef());
    }
}

/// Synthesize a ⌘V keystroke into the frontmost app. No-ops (and prompts once)
/// if Accessibility permission hasn't been granted yet.
pub fn paste() {
    if !is_trusted() {
        prompt_for_trust();
        return;
    }
    let Ok(source) = CGEventSource::new(CGEventSourceStateID::CombinedSessionState) else {
        return;
    };
    for down in [true, false] {
        if let Ok(event) = CGEvent::new_keyboard_event(source.clone(), KEY_V, down) {
            event.set_flags(CGEventFlags::CGEventFlagCommand);
            event.post(CGEventTapLocation::HID);
        }
    }
}
