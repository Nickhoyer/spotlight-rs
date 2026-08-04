//! Headless HTML rendering for email bodies via Blitz (Servo's Stylo styles +
//! Taffy layout + CPU Vello painting). No JavaScript, and no network provider
//! is configured, so remote images and trackers are never fetched.
//!
//! `render_email` is pure computation returning only `Send` data (pixels +
//! link rects), so it can run on gpui's background executor; the `HtmlDocument`
//! itself never leaves the call. Link rects are extracted up front (absolute
//! CSS-pixel boxes for every `<a href>`) because the document isn't kept
//! around for click-time hit-testing.

use anyhow::{bail, Result};
use anyrender::render_to_buffer;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::{local_name, BaseDocument, DocumentConfig};
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use blitz_traits::shell::{ColorScheme, Viewport};

/// Cap on rendered document height (logical px) — keeps one pathological
/// newsletter from allocating a gigapixel buffer. Anything longer is clipped.
const MAX_HEIGHT: f64 = 12_000.0;

/// An absolute link box in logical (CSS-pixel) document coordinates.
#[derive(Debug, Clone)]
pub struct LinkBox {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub href: String,
}

impl LinkBox {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x0 && x <= self.x1 && y >= self.y0 && y <= self.y1
    }
}

/// A rendered email body: straight-alpha BGRA pixels (gpui's `RenderImage`
/// byte order) plus clickable link boxes.
pub struct RenderedEmail {
    /// Physical pixel dimensions of `bgra`.
    pub width: u32,
    pub height: u32,
    /// Logical (CSS-pixel) dimensions to lay the image out at.
    pub logical_width: f32,
    pub logical_height: f32,
    pub bgra: Vec<u8>,
    pub links: Vec<LinkBox>,
}

/// Render an HTML email body at `logical_width` CSS px wide, rasterized at
/// `scale`× for the screen. Emails assume a white canvas, so one is injected.
pub fn render_email(html: &str, logical_width: u32, scale: f64) -> Result<RenderedEmail> {
    // Emails render on white regardless of app theme; inject a base style
    // (author styles still override backgrounds).
    let html = format!(
        "<style>html {{ background: #ffffff; }} body {{ margin: 8px; }}</style>{html}"
    );

    let mut document = HtmlDocument::from_html(
        &html,
        DocumentConfig {
            viewport: Some(Viewport::new(
                logical_width * (scale as u32),
                800 * (scale as u32),
                scale as f32,
                ColorScheme::Light,
            )),
            ..Default::default()
        },
    );
    document.as_mut().resolve(0.0);

    let content_height = document.as_ref().root_element().final_layout.size.height;
    let logical_height = (content_height as f64).clamp(24.0, MAX_HEIGHT);
    let width = (logical_width as f64 * scale) as u32;
    let height = (logical_height * scale) as u32;
    if width == 0 || height == 0 {
        bail!("empty render");
    }

    let mut bgra = render_to_buffer::<VelloCpuImageRenderer, _>(
        |scene| paint_scene(scene, document.as_mut(), scale, width, height, 0, 0),
        width,
        height,
    );
    // RGBA (blitz) → BGRA (gpui RenderImage).
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }

    let links = collect_links(document.as_ref());
    Ok(RenderedEmail {
        width,
        height,
        logical_width: logical_width as f32,
        logical_height: logical_height as f32,
        bgra,
        links,
    })
}

/// Absolute boxes for every `<a href>` with a non-empty layout.
///
/// A node's `final_layout.location` is relative to its parent's box (for
/// inline-level nodes, to the inline root's *content* box), so walking the
/// parent chain sums locations, adding padding+border when crossing an inline
/// root — mirroring the coordinate handling in blitz's own hit-testing.
fn collect_links(doc: &BaseDocument) -> Vec<LinkBox> {
    let mut links = Vec::new();
    for node_id in 0.. {
        let Some(node) = doc.get_node(node_id) else {
            break;
        };
        let Some(href) = node.attr(local_name!("href")).filter(|h| !h.is_empty()) else {
            continue;
        };
        let size = node.final_layout.size;
        if size.width <= 0.0 || size.height <= 0.0 {
            continue;
        }
        let (mut x, mut y) = (node.final_layout.location.x, node.final_layout.location.y);
        let mut parent = node.parent;
        while let Some(parent_id) = parent {
            let Some(p) = doc.get_node(parent_id) else {
                break;
            };
            if p.flags.is_inline_root() {
                x += p.final_layout.padding.left + p.final_layout.border.left;
                y += p.final_layout.padding.top + p.final_layout.border.top;
            }
            x += p.final_layout.location.x;
            y += p.final_layout.location.y;
            parent = p.parent;
        }
        links.push(LinkBox {
            x0: x,
            y0: y,
            x1: x + size.width,
            y1: y + size.height,
            href: href.to_string(),
        });
    }
    links
}

#[cfg(test)]
mod tests {
    use super::render_email;

    const BUTTON_EMAIL: &str = r#"
<table width="100%" cellpadding="0" cellspacing="0"><tr><td align="center" style="padding:24px 0;">
  <table width="560" cellpadding="0" cellspacing="0"><tr><td align="center" style="padding:40px;">
    <a href="https://example.com/invoice" style="background:#1a73e8; color:#fff; padding:12px 32px; border-radius:24px; display:inline-block;">View invoice</a>
  </td></tr></table>
</td></tr></table>"#;

    #[test]
    fn renders_and_finds_link_boxes() {
        let rendered = render_email(BUTTON_EMAIL, 660, 2.0).unwrap();
        assert_eq!(rendered.width, 1320);
        assert!(rendered.height > 100);
        assert_eq!(
            rendered.bgra.len(),
            (rendered.width * rendered.height * 4) as usize
        );

        let link = rendered
            .links
            .iter()
            .find(|l| l.href == "https://example.com/invoice")
            .expect("anchor box collected");
        // The button is horizontally centered; its box must contain the
        // horizontal center of the document at its own vertical midpoint.
        let (cx, cy) = (330.0, (link.y0 + link.y1) / 2.0);
        assert!(
            link.contains(cx, cy),
            "expected {link:?} to contain ({cx},{cy})"
        );
        assert!(link.x1 - link.x0 > 80.0 && link.y1 - link.y0 > 20.0);

        // The white canvas actually painted: the top-left pixel is opaque white.
        assert_eq!(&rendered.bgra[0..4], &[255, 255, 255, 255]);
    }
}
