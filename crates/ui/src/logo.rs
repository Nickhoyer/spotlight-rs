//! Generated app-style logos for built-in entries that have no real app icon
//! (Clipboard, Settings). We draw them as SVG — a gradient rounded-square plus a
//! white symbol, mirroring how macOS app icons look — and rasterize once with
//! resvg, then hand them to the same `img(ImageSource::Render(..))` path used for
//! real icons so they share the tile's size and corner rounding.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use gpui::RenderImage;
use resvg::{tiny_skia, usvg};

/// Rasterize at 2× the on-screen slot so the logos stay crisp on retina.
const RASTER_PX: u32 = 96;

/// The full app logo (colored) and the monochrome menu-bar template glyph, kept
/// alongside the code so bundling and the status-bar icon need no runtime assets.
const APP_LOGO_SVG: &str = include_str!("../../../assets/logo.svg");
const MENUBAR_SVG: &str = include_str!("../../../assets/logo-menubar.svg");

/// Render the menu-bar template glyph to premultiplied RGBA at `size`×`size`.
/// Returns `(width, height, rgba)`. The status bar builds an `NSImage` from this
/// and flags it as a template so macOS tints it for the current menu-bar theme.
pub fn menubar_icon_rgba(size: u32) -> Option<(u32, u32, Vec<u8>)> {
    let pixmap = render_svg(MENUBAR_SVG, size)?;
    Some((size, size, pixmap.data().to_vec()))
}

/// Write the standard macOS `.iconset` PNGs for the app logo into `out_dir`
/// (which must exist). `iconutil -c icns` turns the result into `AppIcon.icns`.
pub fn emit_iconset(out_dir: &Path) -> std::io::Result<()> {
    // (point size, scale) → Apple's iconset file-name convention.
    const VARIANTS: &[(u32, u32)] = &[
        (16, 1),
        (16, 2),
        (32, 1),
        (32, 2),
        (128, 1),
        (128, 2),
        (256, 1),
        (256, 2),
        (512, 1),
        (512, 2),
    ];
    for &(pt, scale) in VARIANTS {
        let px = pt * scale;
        let png = render_svg(APP_LOGO_SVG, px)
            .and_then(|p| p.encode_png().ok())
            .ok_or_else(|| std::io::Error::other(format!("render logo @{px}px failed")))?;
        let suffix = if scale == 2 { "@2x" } else { "" };
        std::fs::write(out_dir.join(format!("icon_{pt}x{pt}{suffix}.png")), png)?;
    }
    Ok(())
}

/// Rasterize `svg` into a `size`×`size` tiny_skia pixmap (premultiplied RGBA),
/// scaling the SVG's viewBox to fill the square.
fn render_svg(svg: &str, size: u32) -> Option<tiny_skia::Pixmap> {
    let tree = usvg::Tree::from_str(svg, &usvg::Options::default()).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(size, size)?;
    let s = tree.size();
    let transform =
        tiny_skia::Transform::from_scale(size as f32 / s.width(), size as f32 / s.height());
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Some(pixmap)
}

/// Return the cached rasterized logo for `kind` (`"clipboard"`, `"settings"`, or
/// `"llm"` for AI Chat), or `None` for an unknown kind / rasterization failure.
pub fn logo(kind: &str) -> Option<Arc<RenderImage>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<Arc<RenderImage>>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(map) = cache.lock() {
        if let Some(hit) = map.get(kind) {
            return hit.clone();
        }
    }
    let svg = match kind {
        "clipboard" => clipboard_svg(),
        "settings" => settings_svg(),
        "llm" => ai_svg(),
        _ => return None,
    };
    let image = rasterize(&svg, RASTER_PX).map(Arc::new);
    if let Ok(mut map) = cache.lock() {
        map.insert(kind.to_string(), image.clone());
    }
    image
}

/// Render `svg` into a `size`×`size` `RenderImage`. tiny_skia produces
/// premultiplied RGBA; gpui's `RenderImage` wants straight-alpha BGRA (the same
/// convention `resolve_icon` targets), so we un-premultiply and swap R/B.
fn rasterize(svg: &str, size: u32) -> Option<RenderImage> {
    let pixmap = render_svg(svg, size)?;

    let src = pixmap.data();
    let mut out = vec![0u8; src.len()];
    for (i, p) in src.chunks_exact(4).enumerate() {
        let a = p[3];
        let un = |c: u8| {
            if a == 0 {
                0
            } else {
                ((c as u16 * 255 + a as u16 / 2) / a as u16).min(255) as u8
            }
        };
        let o = &mut out[i * 4..i * 4 + 4];
        o[0] = un(p[2]); // B
        o[1] = un(p[1]); // G
        o[2] = un(p[0]); // R
        o[3] = a;
    }
    let buffer = image::RgbaImage::from_raw(size, size, out)?;
    Some(RenderImage::new(vec![image::Frame::new(buffer)]))
}

// ── ICON GUIDELINES ─────────────────────────────────────────────────────────
// The built-in panel logos (Settings, Clipboard, AI Chat) form one cohesive set.
// Keep new ones consistent while leaving room for per-icon character:
//
//  • Canvas: a 40×40 viewBox with the shared rounded tile (`rect rx=9`,
//    `url(#bg)` gunmetal gradient). Never a bare/transparent symbol — always the
//    tile, so search results and Home tiles round identically.
//  • Symbol: the app's cyan accent (`url(#sym)`), drawn twice — a blurred glow
//    copy underneath (`fill="#28c8f0" filter="url(#glow)"`, ~0.5–0.65 opacity)
//    then the crisp gradient on top. This is the "glowing HUD" look.
//  • Center the symbol with even margins (roughly the 6–34 band on both axes).
//  • Creative license: one extra accent per icon is welcome for identity — a
//    bright-white highlight (AI's sparkle), engraved dark detail on a filled
//    shape (Clipboard's ruled lines), etc. Keep the tile + cyan glow constant.
//
// Compose via `logo_svg(body)`; `body` is the symbol markup (glow + crisp).

/// Shared palette + filters: gunmetal tile background, cyan symbol gradient, and
/// the soft glow. See ICON GUIDELINES above.
const LOGO_DEFS: &str = r##"
  <linearGradient id="bg" x1="0" y1="0" x2="40" y2="40" gradientUnits="userSpaceOnUse">
    <stop offset="0" stop-color="#2b3444"/><stop offset="1" stop-color="#111621"/>
  </linearGradient>
  <linearGradient id="sym" x1="0" y1="4" x2="0" y2="36" gradientUnits="userSpaceOnUse">
    <stop offset="0" stop-color="#a7f2ff"/><stop offset="1" stop-color="#37c6ec"/>
  </linearGradient>
  <filter id="glow" x="-40%" y="-40%" width="180%" height="180%">
    <feGaussianBlur stdDeviation="1.5"/>
  </filter>
"##;

/// Wrap symbol `body` in the shared tile (rounded gunmetal square) + defs.
fn logo_svg(body: &str) -> String {
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 40 40"><defs>{defs}</defs><rect width="40" height="40" rx="9" fill="url(#bg)"/>{body}</svg>"##,
        defs = LOGO_DEFS,
        body = body,
    )
}

/// Gunmetal tile with a glowing cyan gear (crisp cog over a soft glow) — the
/// reference for the set's futuristic HUD look.
fn settings_svg() -> String {
    let gear = gear_path();
    logo_svg(&format!(
        r##"<path d="{gear}" fill="#28c8f0" fill-rule="evenodd" filter="url(#glow)" opacity="0.65"/><path d="{gear}" fill="url(#sym)" fill-rule="evenodd"/>"##
    ))
}

/// Glowing cyan clipboard with dark engraved ruled lines.
fn clipboard_svg() -> String {
    let board = r##"<rect x="10" y="9" width="20" height="24" rx="3.4"/><rect x="15" y="5.6" width="10" height="6" rx="2.6"/>"##;
    logo_svg(&format!(
        r##"<g fill="#28c8f0" filter="url(#glow)" opacity="0.5">{board}</g><g fill="url(#sym)">{board}</g><g fill="#182231"><rect x="14" y="16.4" width="12" height="2.1" rx="1.05"/><rect x="14" y="20.6" width="12" height="2.1" rx="1.05"/><rect x="14" y="24.8" width="8" height="2.1" rx="1.05"/></g>"##
    ))
}

/// Glowing cyan speech bubble with a bright white AI sparkle.
fn ai_svg() -> String {
    let bubble = r##"<path d="M12 9H28a5 5 0 0 1 5 5v9a5 5 0 0 1-5 5H19l-6 5v-5a5 5 0 0 1-5-5V14a5 5 0 0 1 5-5Z"/>"##;
    logo_svg(&format!(
        r##"<g fill="#28c8f0" filter="url(#glow)" opacity="0.6">{bubble}</g><g fill="url(#sym)">{bubble}</g><path d="M20.2 12.4c.8 3.9 2.1 5.2 6 6-3.9.8-5.2 2.1-6 6-.8-3.9-2.1-5.2-6-6 3.9-.8 5.2-2.1 6-6Z" fill="#f2feff"/><path d="M28.5 12c.4 1.9 1 2.5 2.9 2.9-1.9.4-2.5 1-2.9 2.9-.4-1.9-1-2.5-2.9-2.9 1.9-.4 2.5-1 2.9-2.9Z" fill="#eafcff" opacity="0.9"/>"##
    ))
}

/// A solid cog silhouette (eight trapezoidal teeth) with a round hub cut out via
/// the even-odd rule — the shape SF Symbols draws for `gearshape.fill`.
fn gear_path() -> String {
    const CX: f32 = 20.0;
    const CY: f32 = 20.0;
    const R_TIP: f32 = 15.8;
    const R_ROOT: f32 = 12.0;
    const HOLE: f32 = 5.6;
    const TEETH: usize = 8;

    let pitch = std::f32::consts::TAU / TEETH as f32;
    let tip_half = pitch * 0.17;
    let root_half = pitch * 0.30;
    let top = -std::f32::consts::FRAC_PI_2; // start the first tooth at 12 o'clock
    let point = |r: f32, a: f32| (CX + r * a.cos(), CY + r * a.sin());

    let mut d = String::new();
    for i in 0..TEETH {
        let a = top + i as f32 * pitch;
        for (j, (x, y)) in [
            point(R_ROOT, a - root_half),
            point(R_TIP, a - tip_half),
            point(R_TIP, a + tip_half),
            point(R_ROOT, a + root_half),
        ]
        .iter()
        .enumerate()
        {
            d.push_str(&format!(
                "{}{x:.2} {y:.2} ",
                if i == 0 && j == 0 { "M" } else { "L" }
            ));
        }
    }
    d.push('Z');
    // Round hub, punched out by the even-odd fill rule.
    d.push_str(&format!(
        " M{:.2} {CY:.2} a{HOLE:.2} {HOLE:.2} 0 1 0 {:.2} 0 a{HOLE:.2} {HOLE:.2} 0 1 0 {:.2} 0 Z",
        CX - HOLE,
        HOLE * 2.0,
        -HOLE * 2.0
    ));
    d
}
