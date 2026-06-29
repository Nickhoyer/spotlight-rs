//! macOS-specific glue: application discovery, launching, and (later) the
//! NSPanel/global-hotkey/vibrancy work. Kept in one crate so the AppKit FFI is
//! contained in a single place.

// The `objc` 0.2 `msg_send!`/`sel!` macros expand to `cfg`s rustc doesn't know.
#![allow(unexpected_cfgs)]

pub mod apps;
pub mod capture;
pub mod window;
