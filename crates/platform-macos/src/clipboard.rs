//! Read/write access to the general `NSPasteboard`, used by the clipboard-history
//! extension to monitor copies and to paste items back.
//!
//! `change_count` is a cheap monotonically-increasing counter the pasteboard
//! bumps on every write, so the monitor can poll it and only read contents when
//! it actually changed. Reads honor the `org.nspasteboard.ConcealedType` marker
//! that password managers set, so secrets are never captured into history.

use std::ffi::{c_void, CStr, CString};

use objc::rc::autoreleasepool;
use objc::runtime::Object;
use objc::{class, msg_send, sel, sel_impl};

/// UTI for plain UTF-8 text (`NSPasteboardTypeString`).
const TYPE_STRING: &str = "public.utf8-plain-text";
/// UTI for PNG image data (`NSPasteboardTypePNG`).
const TYPE_PNG: &str = "public.png";
/// UTI for TIFF image data (`NSPasteboardTypeTIFF`) — the fallback image form.
const TYPE_TIFF: &str = "public.tiff";
/// Marker type set by password managers to keep content out of history.
const TYPE_CONCEALED: &str = "org.nspasteboard.ConcealedType";
/// `NSBitmapImageFileType.png`.
const NS_BITMAP_FILE_TYPE_PNG: usize = 4;

/// The general pasteboard's change counter. Increases by one on every write to
/// the pasteboard (by any app), so a change is detected by a value change alone.
/// Returns 0 if the pasteboard can't be reached.
pub fn change_count() -> i64 {
    autoreleasepool(|| unsafe {
        let pb: *mut Object = msg_send![class!(NSPasteboard), generalPasteboard];
        if pb.is_null() {
            return 0;
        }
        msg_send![pb, changeCount]
    })
}

/// The current plain-text contents of the pasteboard, or `None` if there's no
/// text or the content is marked concealed (e.g. a password-manager copy).
pub fn read_text() -> Option<String> {
    autoreleasepool(|| unsafe {
        let pb: *mut Object = msg_send![class!(NSPasteboard), generalPasteboard];
        if pb.is_null() || has_type(pb, TYPE_CONCEALED) {
            return None;
        }
        let ty = nsstring(TYPE_STRING)?;
        let s: *mut Object = msg_send![pb, stringForType: ty];
        nsstring_to_string(s)
    })
}

/// The current image on the pasteboard as PNG bytes (converting from TIFF when
/// that's all the source provided), or `None` if there's no image / it's
/// concealed.
pub fn read_image_png() -> Option<Vec<u8>> {
    autoreleasepool(|| unsafe {
        let pb: *mut Object = msg_send![class!(NSPasteboard), generalPasteboard];
        if pb.is_null() || has_type(pb, TYPE_CONCEALED) {
            return None;
        }
        // Prefer PNG directly.
        if let Some(ty) = nsstring(TYPE_PNG) {
            let data: *mut Object = msg_send![pb, dataForType: ty];
            if let Some(bytes) = nsdata_bytes(data) {
                return Some(bytes);
            }
        }
        // Fall back to TIFF and transcode to PNG via NSBitmapImageRep.
        let tiff_ty = nsstring(TYPE_TIFF)?;
        let tiff: *mut Object = msg_send![pb, dataForType: tiff_ty];
        if tiff.is_null() {
            return None;
        }
        let rep: *mut Object = msg_send![class!(NSBitmapImageRep), imageRepWithData: tiff];
        if rep.is_null() {
            return None;
        }
        let props: *mut Object = msg_send![class!(NSDictionary), dictionary];
        let png: *mut Object =
            msg_send![rep, representationUsingType: NS_BITMAP_FILE_TYPE_PNG properties: props];
        nsdata_bytes(png)
    })
}

/// Replace the pasteboard contents with `text`.
pub fn write_text(text: &str) {
    autoreleasepool(|| unsafe {
        let pb: *mut Object = msg_send![class!(NSPasteboard), generalPasteboard];
        if pb.is_null() {
            return;
        }
        let _: i64 = msg_send![pb, clearContents];
        let (Some(s), Some(ty)) = (nsstring(text), nsstring(TYPE_STRING)) else {
            return;
        };
        let _: i8 = msg_send![pb, setString: s forType: ty];
    })
}

/// Replace the pasteboard contents with a PNG image.
pub fn write_image_png(png: &[u8]) {
    autoreleasepool(|| unsafe {
        let pb: *mut Object = msg_send![class!(NSPasteboard), generalPasteboard];
        if pb.is_null() || png.is_empty() {
            return;
        }
        let _: i64 = msg_send![pb, clearContents];
        let data: *mut Object = msg_send![
            class!(NSData),
            dataWithBytes: png.as_ptr() as *const c_void
            length: png.len()
        ];
        let (false, Some(ty)) = (data.is_null(), nsstring(TYPE_PNG)) else {
            return;
        };
        let _: i8 = msg_send![pb, setData: data forType: ty];
    })
}

// --- helpers ---------------------------------------------------------------

/// Whether the pasteboard currently advertises `ty` among its types.
unsafe fn has_type(pb: *mut Object, ty: &str) -> bool {
    let types: *mut Object = msg_send![pb, types];
    if types.is_null() {
        return false;
    }
    let Some(needle) = nsstring(ty) else {
        return false;
    };
    let contains: i8 = msg_send![types, containsObject: needle];
    contains != 0
}

/// Build an autoreleased `NSString` from a Rust string (interior NULs stripped).
unsafe fn nsstring(s: &str) -> Option<*mut Object> {
    let c = CString::new(s.replace('\0', "")).ok()?;
    let ptr: *mut Object = msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()];
    (!ptr.is_null()).then_some(ptr)
}

/// Copy an `NSString`'s UTF-8 contents into an owned `String`.
unsafe fn nsstring_to_string(s: *mut Object) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let utf8: *const std::os::raw::c_char = msg_send![s, UTF8String];
    if utf8.is_null() {
        return None;
    }
    CStr::from_ptr(utf8).to_str().ok().map(|s| s.to_owned())
}

/// Copy an `NSData`'s bytes into an owned `Vec`.
unsafe fn nsdata_bytes(data: *mut Object) -> Option<Vec<u8>> {
    if data.is_null() {
        return None;
    }
    let len: usize = msg_send![data, length];
    if len == 0 {
        return None;
    }
    let ptr: *const u8 = msg_send![data, bytes];
    if ptr.is_null() {
        return None;
    }
    Some(std::slice::from_raw_parts(ptr, len).to_vec())
}
