//! Rasterize system file/app icons via `NSWorkspace`. Results are cached by path
//! so repeated renders (which happen every frame) don't re-hit AppKit.
//!
//! NOTE: we rasterize into a premultiplied-alpha **RGBA** rep (see
//! `NS_BITMAP_FORMAT_RGBA_PREMULTIPLIED`). The UI layer un-premultiplies and
//! reorders to the BGRA that gpui's `RenderImage` wants — see `resolve_icon` in
//! the `ui` crate.

use std::collections::HashMap;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use objc::rc::autoreleasepool;
use objc::runtime::Object;
use objc::{class, msg_send, sel, sel_impl};

/// Edge length (px) of a rasterized icon. ~2× the 28px list size so icons stay
/// crisp on retina; the `img` element scales them down at draw time.
const ICON_PX: usize = 64;

/// Write an `IconPixels` buffer to a PNG next to /tmp, for verifying
/// rasterization independent of gpui. Only used when SPOTLIGHT_DUMP_ICONS is set.
fn dump_icon_png(pixels: &IconPixels, source: &Path) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let i = N.fetch_add(1, Ordering::SeqCst);
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("icon")
        .replace('/', "_");
    let out = std::path::Path::new("/tmp").join(format!("spotlight-icon-{i:02}-{stem}.png"));
    if let Some(buf) =
        image::RgbaImage::from_raw(pixels.width, pixels.height, (*pixels.data).clone())
    {
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        if buf.save(&out).is_ok() {
            eprintln!("icons: dumped {} -> {}", source.display(), out.display());
        }
    }
}

/// Alpha value (0-255) below which a pixel counts as transparent margin.
const ALPHA_CUTOFF: u8 = 12;

/// Trim uniform transparent margins and rescale so the visible artwork fills the
/// tile. Native `.app` icons carry wildly varying built-in padding — Slack's mark
/// floats in a large transparent square while Firefox's fills its canvas — which
/// makes a row of them look ragged next to the full-bleed extension logos. Cropping
/// to the opaque bounds and re-fitting equalizes their on-screen size.
///
/// The buffer is premultiplied-alpha RGBA (see the module note), which resamples
/// cleanly here; the UI layer un-premultiplies later.
fn normalize(pixels: IconPixels) -> IconPixels {
    let (w, h) = (pixels.width, pixels.height);
    let Some(mut img) = image::RgbaImage::from_raw(w, h, (*pixels.data).clone()) else {
        return pixels;
    };
    // Opaque bounding box.
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0u32, 0u32);
    for y in 0..h {
        for x in 0..w {
            if img.get_pixel(x, y)[3] > ALPHA_CUTOFF {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    if x1 < x0 || y1 < y0 {
        return pixels; // fully transparent — nothing to normalize
    }
    let (bw, bh) = (x1 - x0 + 1, y1 - y0 + 1);
    // Already fills the tile (within a pixel or two)? Skip the resample.
    if bw >= w.saturating_sub(2) && bh >= h.saturating_sub(2) {
        return pixels;
    }
    let cropped = image::imageops::crop(&mut img, x0, y0, bw, bh).to_image();
    // Scale the cropped art to fill the tile, preserving aspect and centering it
    // on a transparent canvas. `replace` copies pixels verbatim (no compositing),
    // so the premultiplied values survive intact.
    let scale = (w as f32 / bw as f32).min(h as f32 / bh as f32);
    let nw = ((bw as f32 * scale).round() as u32).clamp(1, w);
    let nh = ((bh as f32 * scale).round() as u32).clamp(1, h);
    let resized = image::imageops::resize(&cropped, nw, nh, image::imageops::FilterType::Lanczos3);
    let mut canvas = image::RgbaImage::new(w, h);
    image::imageops::replace(&mut canvas, &resized, ((w - nw) / 2) as i64, ((h - nh) / 2) as i64);
    IconPixels {
        width: w,
        height: h,
        data: Arc::new(canvas.into_raw()),
    }
}

/// Raw RGBA pixel buffer for a rasterized icon.
pub struct IconPixels {
    pub width: u32,
    pub height: u32,
    pub data: Arc<Vec<u8>>,
}

/// Path → rasterized icon. `None` means we tried and the path had no usable icon,
/// so callers (and re-renders) don't keep retrying AppKit.
static CACHE: OnceLock<Mutex<HashMap<PathBuf, Option<Arc<IconPixels>>>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<PathBuf, Option<Arc<IconPixels>>>> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Get the system icon for `path` as RGBA pixels, cached by path.
///
/// `NSWorkspace`'s `iconForFile:` returns a generic document icon for unknown
/// paths rather than `nil`, so this effectively always succeeds for `.app`
/// bundles. Call from the main thread (it's invoked during render).
pub fn icon_for_file(path: &Path) -> Option<Arc<IconPixels>> {
    let key = path.to_path_buf();
    if let Ok(map) = cache().lock() {
        if let Some(entry) = map.get(&key) {
            return entry.clone();
        }
    }
    let pixels = rasterize(path).map(normalize).map(Arc::new);
    if let Ok(mut map) = cache().lock() {
        map.insert(key, pixels.clone());
    }
    // Debug aid: dump the rasterized icon to /tmp so we can verify rasterization
    // independently of gpui rendering. Set SPOTLIGHT_DUMP_ICONS to enable.
    if let Some(ref p) = pixels {
        if std::env::var_os("SPOTLIGHT_DUMP_ICONS").is_some() {
            dump_icon_png(p, path);
        }
    }
    pixels
}

// --- AppKit FFI structs ---------------------------------------------------

#[repr(C)]
struct NSPoint {
    x: f64,
    y: f64,
}
#[repr(C)]
struct NSSize {
    width: f64,
    height: f64,
}
#[repr(C)]
struct NSRect {
    origin: NSPoint,
    size: NSSize,
}

/// `NSCompositingOperation.NSCompositeSourceOver`.
const NS_COMPOSITE_SOURCE_OVER: i64 = 2;
/// Bitmap format for the rasterization rep: `0` = no flags, i.e. **premultiplied
/// alpha as the last component** → RGBA in memory.
///
/// We can't use straight (non-premultiplied) alpha here: Core Graphics refuses to
/// create a drawing context for a non-premultiplied bitmap, so
/// `graphicsContextWithBitmapImageRep:` would return nil. Premultiplied-last is
/// the natural RGBA layout; `resolve_icon` in the `ui` crate un-premultiplies and
/// reorders to the BGRA gpui wants. (`1` would be `NSBitmapFormatAlphaFirst`,
/// yielding ARGB — the source of the old blue-tint bug.)
const NS_BITMAP_FORMAT_RGBA_PREMULTIPLIED: usize = 0;

fn rasterize(path: &Path) -> Option<IconPixels> {
    let c_path = CString::new(path.to_str()?).ok()?;
    autoreleasepool(|| unsafe {
        let workspace: *mut Object = msg_send![class!(NSWorkspace), sharedWorkspace];
        if workspace.is_null() {
            return None;
        }
        let ns_path: *mut Object =
            msg_send![class!(NSString), stringWithUTF8String: c_path.as_ptr()];
        if ns_path.is_null() {
            return None;
        }
        let img: *mut Object = msg_send![workspace, iconForFile: ns_path];
        if img.is_null() {
            return None;
        }
        // The image reps are often 128×128 (or 16-bit); we draw into a fixed
        // 8-bit RGBA rep below, so no need to setSize.

        // Color-space name (NSString). NSDeviceRGBColorSpace.
        let cs_name: *mut Object = msg_send![
            class!(NSString),
            stringWithUTF8String: b"NSDeviceRGBColorSpace\0".as_ptr() as *const i8
        ];
        if cs_name.is_null() {
            return None;
        }

        // Allocate a straight-alpha 8bpc RGBA bitmap rep at ICON_PX×ICON_PX.
        let rep: *mut Object = msg_send![
            class!(NSBitmapImageRep),
            alloc
        ];
        if rep.is_null() {
            return None;
        }
        let rep: *mut Object = msg_send![
            rep,
            initWithBitmapDataPlanes: std::ptr::null_mut::<*mut u8>()
            pixelsWide: ICON_PX
            pixelsHigh: ICON_PX
            bitsPerSample: 8usize
            samplesPerPixel: 4usize
            hasAlpha: 1i8
            isPlanar: 0i8
            colorSpaceName: cs_name
            bitmapFormat: NS_BITMAP_FORMAT_RGBA_PREMULTIPLIED
            bytesPerRow: 0usize
            bitsPerPixel: 0usize
        ];
        if rep.is_null() {
            return None;
        }

        // Draw the NSImage into the rep. NSBitmapImageRep's graphics context is
        // top-left origin, so the icon draws upright.
        let dest_rect = NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: NSSize {
                width: ICON_PX as f64,
                height: ICON_PX as f64,
            },
        };
        let zero_rect = NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: NSSize { width: 0.0, height: 0.0 },
        };
        let gfx_ctx: *mut Object =
            msg_send![class!(NSGraphicsContext), graphicsContextWithBitmapImageRep: rep];
        if gfx_ctx.is_null() {
            return None;
        }
        let _: () = msg_send![class!(NSGraphicsContext), saveGraphicsState];
        let _: () = msg_send![class!(NSGraphicsContext), setCurrentContext: gfx_ctx];
        // fromRect: NSZeroRect ⇒ use the whole image; operation: source-over.
        let _: () = msg_send![
            img,
            drawInRect: dest_rect
            fromRect: zero_rect
            operation: NS_COMPOSITE_SOURCE_OVER
            fraction: 1.0f64
        ];
        let _: () = msg_send![class!(NSGraphicsContext), restoreGraphicsState];

        // Read pixels. bytesPerRow may include padding; copy row-by-row.
        let width: usize = msg_send![rep, pixelsWide];
        let height: usize = msg_send![rep, pixelsHigh];
        let bpr: usize = msg_send![rep, bytesPerRow];
        let data: *mut u8 = msg_send![rep, bitmapData];
        if data.is_null() || width == 0 || height == 0 {
            return None;
        }
        let total = bpr.checked_mul(height)?;
        let slice = std::slice::from_raw_parts(data, total);
        let row_bytes = width.checked_mul(4)?;
        let mut out = Vec::with_capacity(row_bytes * height);
        for y in 0..height {
            let start = y * bpr;
            out.extend_from_slice(&slice[start..start + row_bytes]);
        }

        Some(IconPixels {
            width: width as u32,
            height: height as u32,
            data: Arc::new(out),
        })
    })
}
