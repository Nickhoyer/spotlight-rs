//! Headless HTML rendering for email bodies via Blitz (Servo's Stylo styles +
//! Taffy layout + CPU Vello painting). No JavaScript, and by default no
//! network provider — remote images and trackers are never fetched unless the
//! user explicitly opts in per message (`load_images`), which grants http(s)
//! GETs only.
//!
//! The document itself is `!Send`, so each render runs on its own named
//! thread, which then stays alive serving click→link hit-tests over a channel
//! (glyph-exact for inline links, precomputed boxes for padded buttons) until
//! the [`HitTester`] is dropped. Everything crossing back is plain `Send`
//! data: pixels, dimensions, hrefs.

use std::sync::mpsc::{channel, Sender};
use std::time::Duration;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use anyrender::render_to_buffer;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::{local_name, BaseDocument, DocumentConfig};
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use blitz_traits::net::{Bytes, Method, NetHandler, NetProvider, Request};
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
/// run it on the background executor). With `load_images`, remote http(s)
/// images are fetched (the only time this module touches the network) and
/// the render waits — within a budget — for them to arrive.
///
/// Blitz is young and panics on some malformed documents; panics on the
/// render thread degrade to an error (→ the caller's text fallback) rather
/// than poisoning the app.
pub fn render_email(
    html: &str,
    logical_width: u32,
    scale: f64,
    load_images: bool,
) -> Result<(RenderedEmail, HitTester)> {
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
            // With the dump knob on, also save the exact source we're about
            // to render so a transparent output can be reproduced offline.
            if std::env::var_os("SPOTLIGHT_GMAIL_DUMP_RENDER").is_some() {
                let path = spotlight_config::cache_dir().join("gmail-render-source.html");
                match std::fs::write(&path, &html) {
                    Ok(()) => crate::debug_log(&format!("render: source dumped to {}", path.display())),
                    Err(e) => crate::debug_log(&format!("render: source dump failed: {e}")),
                }
            }
            let started = std::time::Instant::now();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                build_and_render(&html, logical_width, scale, load_images)
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

/// Byte cap per fetched image, and overall wall-clock budget for loading a
/// message's images before rendering proceeds with whatever has arrived.
const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
const IMAGE_LOAD_BUDGET: Duration = Duration::from_secs(12);

/// A blocking, thread-per-request network provider used only when the user
/// explicitly asks to load a message's remote images. GET over http(s) only;
/// anything else (cid:, data:, form posts) is dropped, which blitz treats as
/// a resource that simply never arrives.
struct UreqNetProvider {
    agent: ureq::Agent,
    in_flight: Arc<AtomicUsize>,
}

impl UreqNetProvider {
    fn new() -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(10))
                .build(),
            in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }
}

impl NetProvider for UreqNetProvider {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        if request.method != Method::GET
            || !matches!(request.url.scheme(), "http" | "https")
        {
            return;
        }
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        let agent = self.agent.clone();
        let in_flight = self.in_flight.clone();
        std::thread::spawn(move || {
            let result = agent.get(request.url.as_str()).call();
            if let Ok(response) = result {
                let mut buf = Vec::new();
                use std::io::Read as _;
                let ok = response
                    .into_reader()
                    .take(MAX_IMAGE_BYTES)
                    .read_to_end(&mut buf)
                    .is_ok();
                if ok {
                    handler.bytes(request.url.to_string(), Bytes::from(buf));
                }
            }
            in_flight.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

/// Remove external-stylesheet references (`<link …>` tags and `@import`
/// rules) before parsing. There is no net provider, so they could never load
/// — and blitz suppresses ALL painting while such "critical resources" are
/// pending, which turned every email with a webfont/stylesheet link into a
/// fully transparent render. Emails lose nothing: without network, a `<link>`
/// can't contribute styles anyway (and not fetching is the privacy point).
fn sanitize(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while i < html.len() {
        let rest = &lower[i..];
        if rest.starts_with("<link")
            && matches!(
                rest.as_bytes().get(5),
                Some(b' ' | b'\t' | b'\n' | b'\r' | b'/' | b'>')
            )
        {
            i += rest.find('>').map(|e| e + 1).unwrap_or(rest.len());
        } else if rest.starts_with("@import") {
            // Drop through the terminating ';' (or stop at a tag boundary if
            // the rule is malformed).
            let end = match (rest.find(';'), rest.find('<')) {
                (Some(s), Some(t)) if s < t => s + 1,
                (Some(_) | None, Some(t)) => t,
                (Some(s), None) => s + 1,
                (None, None) => rest.len(),
            };
            i += end;
        } else {
            let ch = html[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn build_and_render(
    html: &str,
    logical_width: u32,
    scale: f64,
    load_images: bool,
) -> Result<(HtmlDocument, RenderedEmail, Vec<LinkBox>)> {
    // Emails render on white regardless of app theme; inject a base style
    // (author styles still override the backgrounds). The border-collapse
    // override neutralizes a blitz bug where `border-collapse: collapse` —
    // boilerplate in email CSS resets, both in stylesheets and inline on
    // `display:table` divs, hence the universal selector — paints phantom
    // thick black borders on borderless tables; `separate` is visually
    // equivalent for them.
    let html = format!(
        "<style>\
         html {{ background: #ffffff; }} body {{ margin: 8px; }}\
         * {{ border-collapse: separate !important; }}\
         </style>{}",
        sanitize(html)
    );

    let provider = load_images.then(|| Arc::new(UreqNetProvider::new()));
    let mut document = HtmlDocument::from_html(
        &html,
        DocumentConfig {
            base_url: Some(BASE_URL.to_string()),
            net_provider: provider
                .clone()
                .map(|p| p as Arc<dyn NetProvider>),
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

    // Wait (within budget) for image fetches, re-resolving so completed
    // responses are ingested — late arrivals after the budget are dropped
    // with the document.
    if let Some(provider) = &provider {
        let deadline = std::time::Instant::now() + IMAGE_LOAD_BUDGET;
        while provider.in_flight() > 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(30));
            document.as_mut().resolve(0.0);
        }
        document.as_mut().resolve(0.0);
    }

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

    // Diagnostic: what did the rasterizer actually produce? ~100% transparent
    // means vello painted nothing (the injected white page background didn't
    // even land) — the classic symptom being a blank card in the UI while
    // every pipeline stage reports success. ~100% white with no content means
    // layout/paint ran but text didn't (e.g. font loading failure).
    let sampled = bgra.chunks_exact(4).step_by(16);
    let (mut total, mut transparent, mut white, mut content) = (0u64, 0u64, 0u64, 0u64);
    for px in sampled {
        total += 1;
        match px {
            [_, _, _, 0] => transparent += 1,
            [255, 255, 255, 255] => white += 1,
            _ => content += 1,
        }
    }
    let pct = |n: u64| (n as f64 / total.max(1) as f64) * 100.0;
    crate::debug_log(&format!(
        "render: pixels {:.1}% transparent / {:.1}% white / {:.1}% content",
        pct(transparent),
        pct(white),
        pct(content)
    ));
    if std::env::var_os("SPOTLIGHT_GMAIL_DUMP_RENDER").is_some() {
        dump_render(&bgra, width, height);
    }

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

/// Diagnostic (`SPOTLIGHT_GMAIL_DUMP_RENDER=1`): write the still-RGBA render
/// to `<cache>/gmail-render-<WxH>.png` so a suspect image can be inspected
/// directly. The png codec comes in via the workspace `image` features.
fn dump_render(rgba: &[u8], width: u32, height: u32) {
    use image::ImageEncoder as _;
    let path = spotlight_config::cache_dir().join(format!("gmail-render-{width}x{height}.png"));
    let _ = std::fs::create_dir_all(spotlight_config::cache_dir());
    let encode = || -> anyhow::Result<()> {
        let file = std::fs::File::create(&path)?;
        image::codecs::png::PngEncoder::new(std::io::BufWriter::new(file)).write_image(
            rgba,
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )?;
        Ok(())
    };
    match encode() {
        Ok(()) => crate::debug_log(&format!("render: dumped {}", path.display())),
        Err(e) => crate::debug_log(&format!("render: dump failed: {e}")),
    }
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

    /// Fraction of pixels that are fully transparent — the signature of
    /// blitz's suppressed paint. A healthy email render is ~0%.
    fn transparent_pct(rendered: &super::RenderedEmail) -> f64 {
        let transparent = rendered
            .bgra
            .chunks_exact(4)
            .filter(|px| px[3] == 0)
            .count() as f64;
        transparent / (rendered.width as f64 * rendered.height as f64) * 100.0
    }

    #[test]
    fn stylesheet_links_dont_suppress_painting() {
        // The exact shape that shipped blank: XHTML doctype + a Google Fonts
        // stylesheet <link> + an @import. With no net provider these stay
        // "pending" forever, and blitz refuses to paint anything while
        // critical resources are pending — unless we strip them.
        let html = r#"<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Transitional//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-transitional.dtd">
<html xmlns="http://www.w3.org/1999/xhtml"><head>
<link href="https://fonts.googleapis.com/css?family=Lato:400,700" rel="stylesheet" type="text/css">
<style>@import url("https://example.com/more.css"); .x { color: #333; }</style>
</head><body style="background-color:#fff"><p class="x">Hola mundo</p></body></html>"#;
        let (rendered, _hit) = render_email(html, 660, 2.0, false).unwrap();
        assert!(
            transparent_pct(&rendered) < 1.0,
            "paint was suppressed: {:.1}% transparent",
            transparent_pct(&rendered)
        );
    }

    #[test]
    fn loads_images_from_local_server_only_when_asked() {
        use image::ImageEncoder as _;
        use std::io::{Read as _, Write as _};

        // A throwaway local server offering one red 8x8 PNG.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[255u8, 0, 0, 255].repeat(64), 8, 8, image::ExtendedColorType::Rgba8)
            .unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let png = png.clone();
                std::thread::spawn(move || {
                    let mut buf = [0u8; 2048];
                    let _ = stream.read(&mut buf);
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        png.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(&png);
                });
            }
        });

        let html =
            format!(r#"<img src="http://127.0.0.1:{port}/img.png" width="100" height="100">"#);
        let red_pixels = |r: &super::RenderedEmail| {
            r.bgra
                .chunks_exact(4)
                .filter(|p| p[2] > 200 && p[1] < 60 && p[0] < 60 && p[3] == 255)
                .count()
        };

        let (blocked, _) = render_email(&html, 400, 1.0, false).unwrap();
        assert_eq!(red_pixels(&blocked), 0, "images fetched without opt-in");

        let (loaded, _) = render_email(&html, 400, 1.0, true).unwrap();
        assert!(
            red_pixels(&loaded) > 1000,
            "image did not render: {} red pixels",
            red_pixels(&loaded)
        );
    }

    #[test]
    fn collapsed_borderless_tables_dont_paint_black_bars() {
        // Email-reset boilerplate + an unloadable image inside a borderless
        // table: blitz's border-collapse:collapse paints thick phantom
        // borders (giant black bars) unless we force `separate`.
        let html = r#"<html><head>
<style>table,td,tr{border-collapse:collapse;vertical-align:top}</style>
</head><body>
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0"><tbody><tr>
<td align="center"><img src="https://cdn.example.net/logo.png" alt="Logo" width="493" style="width:85%"></td>
</tr></tbody></table>
</body></html>"#;
        let (rendered, _hit) = render_email(html, 660, 2.0, false).unwrap();
        let content = rendered
            .bgra
            .chunks_exact(4)
            .filter(|px| px[3] != 0 && *px != &[255, 255, 255, 255])
            .count() as f64
            / (rendered.width as f64 * rendered.height as f64)
            * 100.0;
        assert!(
            content < 3.0,
            "phantom table borders painted: {content:.1}% content pixels"
        );
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
        let (rendered, hit) = render_email(html, 660, 2.0, false).unwrap();
        assert!(rendered.height > 0);
        // Pixels actually painted (this test's fonts <link> used to leave the
        // whole render transparent — and the old assertions never noticed).
        assert!(transparent_pct(&rendered) < 1.0);
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
        let (rendered, hit) = render_email(BUTTON_EMAIL, 660, 2.0, false).unwrap();
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
