//! Debug-only self-capture: grab our own window's rendered pixels and write a
//! PNG, from inside our process. Unlike the `screencapture` CLI or an external
//! screenshot tool, this works headlessly / over SSH because the capture runs
//! in the GUI-session process that owns the window. Captures the real composited
//! frame (including Metal-rendered content), which `cacheDisplayInRect` cannot.

use std::ffi::c_void;
use std::path::Path;

use anyhow::{anyhow, Result};
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use core_graphics::window::{
    create_image, kCGWindowImageBoundsIgnoreFraming, kCGWindowListOptionIncludingWindow,
};
use objc::runtime::Object;
use objc::{msg_send, sel, sel_impl};

/// Resolve the CoreGraphics window number for the NSWindow owning `ns_view`.
/// Call on the main thread (it sends an Obj-C message). Returns `None` if the
/// view has no window yet.
pub fn window_number(ns_view: *mut c_void) -> Option<u32> {
    if ns_view.is_null() {
        return None;
    }
    // SAFETY: `ns_view` is a live NSView from gpui's window handle.
    unsafe {
        let view = ns_view as *mut Object;
        let window: *mut Object = msg_send![view, window];
        if window.is_null() {
            return None;
        }
        let num: i64 = msg_send![window, windowNumber];
        (num > 0).then_some(num as u32)
    }
}

/// Capture window `window_id`'s on-screen pixels and write them to `out` as PNG.
/// Thread-safe (no Obj-C main-thread requirement). The window must be on screen.
pub fn capture_window_png(window_id: u32, out: &Path) -> Result<()> {
    // `CGRectNull` tells CG to use the window's own bounds.
    let null_rect = CGRect::new(
        &CGPoint::new(f64::INFINITY, f64::INFINITY),
        &CGSize::new(0.0, 0.0),
    );
    let cg_image = create_image(
        null_rect,
        kCGWindowListOptionIncludingWindow,
        window_id,
        kCGWindowImageBoundsIgnoreFraming,
    )
    .ok_or_else(|| {
        anyhow!("CGWindowListCreateImage returned None (screen-recording permission or off-screen window)")
    })?;

    let width = cg_image.width();
    let height = cg_image.height();
    let bytes_per_row = cg_image.bytes_per_row();
    let data = cg_image.data();
    let src = data.bytes();

    if src.len() < height * bytes_per_row {
        return Err(anyhow!(
            "unexpected pixel buffer size: {} < {}x{}",
            src.len(),
            height,
            bytes_per_row
        ));
    }

    // CGWindowListCreateImage returns 32-bit little-endian BGRA; convert to RGBA
    // and drop any per-row padding.
    let mut rgba = vec![0u8; width * height * 4];
    for y in 0..height {
        let row = y * bytes_per_row;
        for x in 0..width {
            let s = row + x * 4;
            let d = (y * width + x) * 4;
            rgba[d] = src[s + 2];
            rgba[d + 1] = src[s + 1];
            rgba[d + 2] = src[s];
            rgba[d + 3] = src[s + 3];
        }
    }

    let img = image::RgbaImage::from_raw(width as u32, height as u32, rgba)
        .ok_or_else(|| anyhow!("failed to build image buffer"))?;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    img.save(out)?;
    Ok(())
}
