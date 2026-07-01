//! "Launch at Login" via `SMAppService` (ServiceManagement, macOS 13+).
//!
//! Only meaningful for the installed `.app` bundle — when run as a loose binary
//! (e.g. `cargo run`) macOS has nothing to register, so these degrade gracefully:
//! [`is_enabled`] returns `false` and [`set_enabled`] returns an error the caller
//! can surface or ignore. All calls must be on the main thread.

use anyhow::{anyhow, Result};
use objc::runtime::{Class, Object};
use objc::{msg_send, sel, sel_impl};

// Force the framework to load so the `SMAppService` class is registered.
#[link(name = "ServiceManagement", kind = "framework")]
extern "C" {}

/// `SMAppServiceStatusEnabled`.
const STATUS_ENABLED: i64 = 1;

fn main_app_service() -> Option<*mut Object> {
    // SAFETY: `mainAppService` is a class method returning a shared instance;
    // the class is absent on macOS < 13, in which case we bail.
    unsafe {
        let cls = Class::get("SMAppService")?;
        let svc: *mut Object = msg_send![cls, mainAppService];
        (!svc.is_null()).then_some(svc)
    }
}

/// Whether the app is currently registered to launch at login.
pub fn is_enabled() -> bool {
    let Some(svc) = main_app_service() else {
        return false;
    };
    // SAFETY: `status` returns the `SMAppServiceStatus` enum (NSInteger).
    let status: i64 = unsafe { msg_send![svc, status] };
    status == STATUS_ENABLED
}

/// Register (`on = true`) or unregister the app as a login item.
pub fn set_enabled(on: bool) -> Result<()> {
    let svc = main_app_service().ok_or_else(|| anyhow!("SMAppService unavailable (macOS < 13)"))?;
    // SAFETY: both selectors take an `NSError **` out-param and return BOOL.
    unsafe {
        let mut error: *mut Object = std::ptr::null_mut();
        let ok: i8 = if on {
            msg_send![svc, registerAndReturnError: &mut error]
        } else {
            msg_send![svc, unregisterAndReturnError: &mut error]
        };
        if ok != 0 {
            return Ok(());
        }
        let verb = if on { "register" } else { "unregister" };
        Err(anyhow!("SMAppService failed to {verb} login item"))
    }
}
