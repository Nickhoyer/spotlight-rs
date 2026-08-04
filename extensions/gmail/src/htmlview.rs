//! Headless HTML rendering for email bodies via Blitz (Servo's Stylo styles +
//! Taffy layout + CPU Vello painting). No JavaScript, and no network provider
//! is configured, so remote images and trackers are never fetched.
//!
//! The document itself is `!Send`, so each render runs on its own named
//! thread, which then stays alive serving click→link hit-tests over a channel
//! (glyph-exact for inline links, precomputed boxes for padded buttons) until
//! the [`HitTester`] is dropped. Everything crossing back is plain `Send`
//! data: pixels, dimensions, hrefs.

use std::sync::mpsc::{channel, Sender};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use anyrender::render_to_buffer;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::{local_name, BaseDocument, DocumentConfig};
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use blitz_traits::shell::{ColorScheme, Viewport};

/// Cap on rendered document height in *physical* px. Metal refuses textures
/// above 16384px, and gpui uploads the image as one texture — anything taller
/// displays as nothing at all. Kept well under the limit; it also bounds CPU
/// paint time for pathological newsletters. Anything longer is clipped.
const MAX_PHYSICAL_HEIGHT: f64 = 12_000.0;

/// Base URL for resolving the relative / protocol-relative URLs that real
/// emails are full of (`//fonts.googleapis.com/...`, `cid:` images, bare
/// paths). Without one, blitz's `resolve_url` panics on the first such href.
/// Nothing is ever fetched — there is no net provider — resolution just must
/// not fail.
const BASE_URL: &str = "https://mail.google.com/";

/// An absolute link box in logical (CSS-pixel) document coordinates.
#[derive(Debug, Clone)]
struct LinkBox {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    href: String,
}

impl LinkBox {
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x0 && x <= self.x1 && y >= self.y0 && y <= self.y1
    }
}

/// A rendered email body: straight-alpha BGRA pixels (gpui's `RenderImage`
/// byte order) plus the layout size to present them at.
pub struct RenderedEmail {
    /// Physical pixel dimensions of `bgra`.
    pub width: u32,
    pub height: u32,
    /// Logical (CSS-pixel) dimensions to lay the image out at.
    pub logical_width: f32,
    pub logical_height: f32,
    pub bgra: Vec<u8>,
}

/// Handle to the render thread's document for click→link resolution. Dropping
/// it lets the thread exit.
pub struct HitTester {
    query: Sender<(f32, f32, Sender<Option<String>>)>,
}

impl HitTester {
    /// The href under logical document coordinates `(x, y)`, if any.
    pub fn hit(&self, x: f32, y: f32) -> Option<String> {
        let (reply_tx, reply_rx) = channel();
        self.query.send((x, y, reply_tx)).ok()?;
        reply_rx.recv_timeout(Duration::from_millis(100)).ok()?
    }
}

/// Render an HTML email body at `logical_width` CSS px wide, rasterized at
/// `scale`× for the screen. Blocks until the first paint completes (callers
/// run it on the background executor).
///
/// Blitz is young and panics on some malformed documents; panics on the
/// render thread degrade to an error (→ the caller's text fallback) rather
/// than poisoning the app.
pub fn render_email(html: &str, logical_width: u32, scale: f64) -> Result<(RenderedEmail, HitTester)> {
    let (result_tx, result_rx) = channel();
    let (query_tx, query_rx) = channel::<(f32, f32, Sender<Option<String>>)>();
    let html = html.to_string();

    std::thread::Builder::new()
        .name("gmail-htmlview".to_string())
        .spawn(move || {
            crate::debug_log(&format!(
                "render: start {}B html at {logical_width}px @{scale}x",
                html.len()
            ));
            let started = std::time::Instant::now();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                build_and_render(&html, logical_width, scale)
            }))
            .unwrap_or_else(|payload| {
                let msg = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
                    .unwrap_or("renderer panicked");
                Err(anyhow!("HTML renderer failed: {msg}"))
            });
            match &outcome {
                Ok((_, r, _)) => crate::debug_log(&format!(
                    "render: ok {}x{} physical ({}x{} logical) in {}ms",
                    r.width,
                    r.height,
                    r.logical_width,
                    r.logical_height,
                    started.elapsed().as_millis()
                )),
                Err(e) => crate::debug_log(&format!(
                    "render: FAILED after {}ms: {e}",
                    started.elapsed().as_millis()
                )),
            }
            match outcome {
                Ok((mut document, rendered, boxes)) => {
                    if result_tx.send(Ok(rendered)).is_err() {
                        return;
                    }
                    // Serve hit-tests until the HitTester is dropped.
                    while let Ok((x, y, reply)) = query_rx.recv() {
                        let href = hit_href(document.as_mut(), x, y, scale, &boxes);
                        let _ = reply.send(href);
                    }
                }
                Err(e) => {
                    let _ = result_tx.send(Err(e));
                }
            }
        })?;

    let rendered = result_rx
        .recv()
        .map_err(|_| anyhow!("HTML renderer thread died"))??;
    Ok((rendered, HitTester { query: query_tx }))
}

fn build_and_render(
    html: &str,
    logical_width: u32,
    scale: f64,
) -> Result<(HtmlDocument, RenderedEmail, Vec<LinkBox>)> {
    // Emails render on white regardless of app theme; inject a base style
    // (author styles still override backgrounds).
    let html = format!(
        "<style>html {{ background: #ffffff; }} body {{ margin: 8px; }}</style>{html}"
    );

    let mut document = HtmlDocument::from_html(
        &html,
        DocumentConfig {
            base_url: Some(BASE_URL.to_string()),
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
    let logical_height = (content_height as f64).clamp(24.0, MAX_PHYSICAL_HEIGHT / scale);
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

    let boxes = collect_links(document.as_ref());
    let rendered = RenderedEmail {
        width,
        height,
        logical_width: logical_width as f32,
        logical_height: logical_height as f32,
        bgra,
    };
    Ok((document, rendered, boxes))
}

/// Resolve a click to a link. Primary path: blitz's own hit-testing, which is
/// glyph-exact for inline links (the hit lands on the text node, whose
/// ancestors include the `<a>`). Fallback: precomputed anchor boxes, which
/// cover the padding of `display:inline-block` buttons where a hit resolves
/// to the container instead.
fn hit_href(doc: &mut BaseDocument, x: f32, y: f32, scale: f64, boxes: &[LinkBox]) -> Option<String> {
    let hit = doc.root_element().hit(x, y, scale);
    if let Some(hit) = hit {
        let mut id = Some(hit.node_id);
        while let Some(node_id) = id {
            let node = doc.get_node(node_id)?;
            if let Some(href) = node.attr(local_name!("href")) {
                return Some(href.to_string());
            }
            id = node.parent;
        }
    }
    boxes.iter().find(|b| b.contains(x, y)).map(|b| b.href.clone())
}

/// Absolute boxes for every `<a href>` with a non-empty Taffy layout (padded
/// buttons and other atomic inline-/block-level anchors; plain inline text
/// links have no box here and are handled by live hit-testing instead).
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
    use std::collections::HashSet;

    /// Sweep a coordinate grid over the document and collect every href the
    /// hit-tester resolves.
    fn scan_links(hit: &super::HitTester, w: f32, h: f32) -> HashSet<String> {
        let mut found = HashSet::new();
        let mut y = 2.0;
        while y < h {
            let mut x = 2.0;
            while x < w {
                if let Some(href) = hit.hit(x, y) {
                    found.insert(href);
                }
                x += 8.0;
            }
            y += 8.0;
        }
        found
    }

    #[test]
    fn survives_real_world_urls_and_hits_inline_links() {
        // Modeled on Google's own mail: a protocol-relative webfont link plus
        // relative and cid: image sources. This exact shape used to hit
        // blitz's resolve_url panic when no base_url was set.
        let html = r#"
<head><link href="//fonts.googleapis.com/css?family=Google+Sans" rel="stylesheet"></head>
<body>
  <img src="cid:logo@mail" width="10" height="10">
  <img src="images/footer.png" width="10" height="10">
  <p style="font-family:'Google Sans',Roboto,Arial;">A new sign-in — <a href="https://example.com/check">check activity</a>.</p>
</body>"#;
        let (rendered, hit) = render_email(html, 660, 2.0).unwrap();
        assert!(rendered.height > 0);
        // The plain inline link is clickable via glyph-level hit-testing.
        let found = scan_links(&hit, rendered.logical_width, rendered.logical_height);
        assert!(found.contains("https://example.com/check"), "found: {found:?}");
    }

    const BUTTON_EMAIL: &str = r#"
<table width="100%" cellpadding="0" cellspacing="0"><tr><td align="center" style="padding:24px 0;">
  <table width="560" cellpadding="0" cellspacing="0"><tr><td align="center" style="padding:40px;">
    <a href="https://example.com/invoice" style="background:#1a73e8; color:#fff; padding:12px 32px; border-radius:24px; display:inline-block;">View invoice</a>
  </td></tr></table>
</td></tr></table>"#;

    #[test]
    fn renders_and_hits_button_links() {
        let (rendered, hit) = render_email(BUTTON_EMAIL, 660, 2.0).unwrap();
        assert_eq!(rendered.width, 1320);
        assert!(rendered.height > 100);
        assert_eq!(
            rendered.bgra.len(),
            (rendered.width * rendered.height * 4) as usize
        );
        // The white canvas actually painted: the top-left pixel is opaque white.
        assert_eq!(&rendered.bgra[0..4], &[255, 255, 255, 255]);

        // The whole button — padding included — resolves to the link.
        let found = scan_links(&hit, rendered.logical_width, rendered.logical_height);
        assert!(found.contains("https://example.com/invoice"), "found: {found:?}");

        // A click nowhere near a link resolves to nothing.
        assert_eq!(hit.hit(4.0, 4.0), None);
    }
}
