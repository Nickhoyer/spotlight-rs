//! Headless HTML rendering via Blitz (Servo's Stylo styles + Taffy layout +
//! CPU Vello painting), shared by extensions that show remote HTML in-app
//! (Gmail message bodies, Jira issue descriptions). Documents render light or
//! dark per [`Scheme`] — email brings its own white-page palette, while
//! documents we style ourselves match the app. No JavaScript, and by
//! default no network provider — remote images and trackers are never fetched
//! unless the caller explicitly opts in (`load_images`), which grants http(s)
//! GETs only. An optional `Authorization` header is sent with those GETs, and
//! only to the `base_url` origin, so API credentials never leak to third-party
//! image hosts.
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
/// paint time for pathological documents. Anything longer is clipped.
const MAX_PHYSICAL_HEIGHT: f64 = 12_000.0;

/// How to render a document. `base_url` must be set: real-world HTML is full
/// of relative and protocol-relative URLs (`//fonts.googleapis.com/...`,
/// `cid:` images, bare paths), and blitz's `resolve_url` panics on the first
/// such href without a base. Nothing is fetched during resolution — it just
/// must not fail.
pub struct RenderOptions {
    /// Logical (CSS-pixel) width to lay the document out at.
    pub logical_width: u32,
    /// Rasterization scale (the window's backing scale factor).
    pub scale: f64,
    /// Fetch remote http(s) images. Off = fully offline render.
    pub load_images: bool,
    /// Base URL for resolving relative URLs, e.g. `https://mail.google.com/`.
    pub base_url: String,
    /// `Authorization` header value sent with image fetches — but only to
    /// requests on `base_url`'s origin (scheme + host + port).
    pub auth: Option<String>,
    /// What the canvas is painted on, and which way the document resolves
    /// `prefers-color-scheme` and UA colors.
    pub scheme: Scheme,
}

/// Which way to render a document's canvas.
///
/// The distinction is who owns the document's colors. Email supplies its own,
/// written for a white page, so it must render [`Light`](Scheme::Light) — a
/// dark canvas would leave dark author text on a dark background. Where *we*
/// author the stylesheet (Jira's rendered fields carry structure and class
/// hooks, not a palette), [`Dark`](Scheme::Dark) lets the document match the
/// app instead of sitting in a white box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// Paint on white and resolve as a light UI.
    Light,
    /// Paint on `background` and inherit `text` (both `0xRRGGBB`), resolving
    /// as a dark UI.
    ///
    /// `text` is not optional because blitz's UA stylesheet keeps its black
    /// default text color no matter what `color-scheme` says — a dark canvas
    /// without an inherited light color renders black-on-black.
    Dark { background: u32, text: u32 },
}

impl Scheme {
    /// The `html { … }` declarations that establish the canvas.
    fn css_canvas(self) -> String {
        match self {
            Scheme::Light => "background: #ffffff; color-scheme: light;".to_string(),
            Scheme::Dark { background, text } => format!(
                "background: #{:06x}; color: #{:06x}; color-scheme: dark;",
                background & 0xff_ffff,
                text & 0xff_ffff
            ),
        }
    }

    fn color_scheme(self) -> ColorScheme {
        match self {
            Scheme::Light => ColorScheme::Light,
            Scheme::Dark { .. } => ColorScheme::Dark,
        }
    }
}

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

/// A rendered document: straight-alpha BGRA pixels (gpui's `RenderImage`
/// byte order) plus the layout size to present them at.
pub struct RenderedHtml {
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
    /// The href under logical document coordinates `(x, y)`, if any. Returned
    /// as written in the document — relative hrefs stay relative.
    pub fn hit(&self, x: f32, y: f32) -> Option<String> {
        let (reply_tx, reply_rx) = channel();
        self.query.send((x, y, reply_tx)).ok()?;
        reply_rx.recv_timeout(Duration::from_millis(100)).ok()?
    }
}

/// Render an HTML document per `opts`. Blocks until the first paint completes
/// (callers run it on the background executor). With `load_images`, remote
/// http(s) images are fetched (the only time this module touches the network)
/// and the render waits — within a budget — for them to arrive.
///
/// Blitz is young and panics on some malformed documents; panics on the
/// render thread degrade to an error (→ the caller's fallback) rather than
/// poisoning the app.
pub fn render_html(html: &str, opts: RenderOptions) -> Result<(RenderedHtml, HitTester)> {
    let (result_tx, result_rx) = channel();
    let (query_tx, query_rx) = channel::<(f32, f32, Sender<Option<String>>)>();
    let html = html.to_string();
    let scale = opts.scale;

    std::thread::Builder::new()
        .name("htmlview".to_string())
        .spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                build_and_render(&html, &opts)
            }))
            .unwrap_or_else(|payload| {
                let msg = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
                    .unwrap_or("renderer panicked");
                Err(anyhow!("HTML renderer failed: {msg}"))
            });
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
/// document's images before rendering proceeds with whatever has arrived.
const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
const IMAGE_LOAD_BUDGET: Duration = Duration::from_secs(12);

/// A blocking, thread-per-request network provider used only when the caller
/// explicitly asks to load a document's remote images. GET over http(s) only;
/// anything else (cid:, data:, form posts) is dropped, which blitz treats as
/// a resource that simply never arrives.
struct UreqNetProvider {
    agent: ureq::Agent,
    in_flight: Arc<AtomicUsize>,
    /// `Authorization` value sent only to requests matching this origin.
    auth: Option<(url::Origin, String)>,
}

impl UreqNetProvider {
    fn new(auth: Option<(url::Origin, String)>) -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(10))
                // The same-origin check below only sees the URL the document
                // asked for, and hosts redirect: a Jira attachment bounces to
                // api.media.atlassian.com. Stated explicitly (it is also
                // ureq's default) so the credential can't follow a redirect
                // off-origin if that default ever moves.
                .redirect_auth_headers(ureq::RedirectAuthHeaders::Never)
                .build(),
            in_flight: Arc::new(AtomicUsize::new(0)),
            auth,
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
        // Same-origin only: never hand the API credential to third-party hosts
        // an author might have linked images from.
        let auth = self
            .auth
            .as_ref()
            .filter(|(origin, _)| request.url.origin() == *origin)
            .map(|(_, header)| header.clone());
        std::thread::spawn(move || {
            let mut req = agent.get(request.url.as_str());
            if let Some(auth) = &auth {
                req = req.set("Authorization", auth);
            }
            let result = req.call();
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
/// rules) before parsing. Stylesheets are never fetched (only images are,
/// and only on opt-in) — and blitz suppresses ALL painting while such
/// "critical resources" are pending, which turned every email with a
/// webfont/stylesheet link into a fully transparent render. Documents lose
/// nothing: an unfetched `<link>` can't contribute styles anyway (and not
/// fetching is the privacy point).
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
    opts: &RenderOptions,
) -> Result<(HtmlDocument, RenderedHtml, Vec<LinkBox>)> {
    // Base style, injected first so author styles still win on everything but
    // the canvas. `color-scheme` is declared as well as passed to the viewport
    // so UA-derived colors (form controls, default text) follow the scheme.
    // The border-collapse override neutralizes a blitz bug where
    // `border-collapse: collapse` — boilerplate in email CSS resets, both in
    // stylesheets and inline on `display:table` divs, hence the universal
    // selector — paints phantom thick black borders on borderless tables;
    // `separate` is visually equivalent for them.
    let html = format!(
        "<style>\
         html {{ {canvas} }} body {{ margin: 8px; }}\
         * {{ border-collapse: separate !important; }}\
         </style>{body}",
        canvas = opts.scheme.css_canvas(),
        body = sanitize(html)
    );
    let scale = opts.scale;

    let auth = opts.auth.clone().and_then(|header| {
        let origin = url::Url::parse(&opts.base_url).ok()?.origin();
        Some((origin, header))
    });
    let provider = opts.load_images.then(|| Arc::new(UreqNetProvider::new(auth)));
    let mut document = HtmlDocument::from_html(
        &html,
        DocumentConfig {
            base_url: Some(opts.base_url.clone()),
            net_provider: provider
                .clone()
                .map(|p| p as Arc<dyn NetProvider>),
            viewport: Some(Viewport::new(
                opts.logical_width * (scale as u32),
                800 * (scale as u32),
                scale as f32,
                opts.scheme.color_scheme(),
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
    let width = (opts.logical_width as f64 * scale) as u32;
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
    let rendered = RenderedHtml {
        width,
        height,
        logical_width: opts.logical_width as f32,
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
    use super::{render_html, RenderOptions, Scheme};
    use std::collections::HashSet;

    /// Options matching the Gmail defaults the tests were written against.
    fn opts(logical_width: u32, scale: f64, load_images: bool) -> RenderOptions {
        RenderOptions {
            logical_width,
            scale,
            load_images,
            base_url: "https://mail.google.com/".to_string(),
            auth: None,
            scheme: Scheme::Light,
        }
    }

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
    /// blitz's suppressed paint. A healthy render is ~0%.
    fn transparent_pct(rendered: &super::RenderedHtml) -> f64 {
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
        let (rendered, _hit) = render_html(html, opts(660, 2.0, false)).unwrap();
        assert!(
            transparent_pct(&rendered) < 1.0,
            "paint was suppressed: {:.1}% transparent",
            transparent_pct(&rendered)
        );
    }

    /// A throwaway local HTTP server that serves one red 8x8 PNG to every
    /// request and records each request's raw header block.
    fn png_server() -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use image::ImageEncoder as _;
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[255u8, 0, 0, 255].repeat(64), 8, 8, image::ExtendedColorType::Rgba8)
            .unwrap();
        let seen = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let png = png.clone();
                let seen = seen.clone();
                std::thread::spawn(move || {
                    let mut buf = [0u8; 2048];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    seen.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&buf[..n]).into_owned());
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        png.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(&png);
                });
            }
        });
        (port, requests)
    }

    /// A server that 302s every request to `location`, recording what it saw.
    fn redirect_server(location: String) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let location = location.clone();
                let seen = seen.clone();
                std::thread::spawn(move || {
                    let mut buf = [0u8; 2048];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    seen.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&buf[..n]).into_owned());
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    );
                });
            }
        });
        (port, requests)
    }

    fn red_pixels(r: &super::RenderedHtml) -> usize {
        r.bgra
            .chunks_exact(4)
            .filter(|p| p[2] > 200 && p[1] < 60 && p[0] < 60 && p[3] == 255)
            .count()
    }

    #[test]
    fn loads_images_from_local_server_only_when_asked() {
        let (port, _requests) = png_server();
        let html =
            format!(r#"<img src="http://127.0.0.1:{port}/img.png" width="100" height="100">"#);

        let (blocked, _) = render_html(&html, opts(400, 1.0, false)).unwrap();
        assert_eq!(red_pixels(&blocked), 0, "images fetched without opt-in");

        let (loaded, _) = render_html(&html, opts(400, 1.0, true)).unwrap();
        assert!(
            red_pixels(&loaded) > 1000,
            "image did not render: {} red pixels",
            red_pixels(&loaded)
        );
    }

    #[test]
    fn dark_scheme_paints_the_requested_canvas() {
        // A document that sets no background of its own: every pixel is either
        // the canvas or the text drawn on it, so the corner proves the canvas
        // and the mean proves we didn't paint a white page and darken nothing.
        let html = "<p>Dark mode</p>";
        let (rendered, _hit) = render_html(
            html,
            RenderOptions {
                scheme: Scheme::Dark {
                    background: 0x23252c,
                    text: 0xe8ecf4,
                },
                ..opts(400, 1.0, false)
            },
        )
        .unwrap();
        // BGRA, opaque.
        assert_eq!(&rendered.bgra[0..4], &[0x2c, 0x25, 0x23, 255]);
        assert!(
            transparent_pct(&rendered) < 1.0,
            "paint was suppressed: {:.1}% transparent",
            transparent_pct(&rendered)
        );
        // The scheme's text color is inherited, so the copy is light-on-dark:
        // some pixel must be brighter than the canvas. (Blitz's UA stylesheet
        // would otherwise paint it black — invisible here.)
        let brightest = rendered
            .bgra
            .chunks_exact(4)
            .map(|px| px[0].max(px[1]).max(px[2]))
            .max()
            .unwrap_or(0);
        assert!(brightest > 0x80, "no light text painted (max {brightest:#x})");
    }

    #[test]
    fn auth_header_stays_on_base_origin() {
        // Two servers = two origins. The image on the base origin gets the
        // Authorization header; the cross-origin one must not see it.
        let (same_port, same_requests) = png_server();
        let (other_port, other_requests) = png_server();
        let html = format!(
            r#"<img src="/a.png" width="20" height="20">
               <img src="http://127.0.0.1:{other_port}/b.png" width="20" height="20">"#
        );
        let (rendered, _) = render_html(
            &html,
            RenderOptions {
                logical_width: 400,
                scale: 1.0,
                load_images: true,
                base_url: format!("http://127.0.0.1:{same_port}/"),
                auth: Some("Basic c2VjcmV0".to_string()),
                scheme: Scheme::Light,
            },
        )
        .unwrap();
        assert!(red_pixels(&rendered) > 500, "images did not render");

        let same = same_requests.lock().unwrap().join("\n");
        let other = other_requests.lock().unwrap().join("\n");
        assert!(
            same.contains("Authorization: Basic c2VjcmV0"),
            "same-origin fetch lacked auth: {same}"
        );
        assert!(!other.is_empty(), "cross-origin image was never fetched");
        assert!(
            !other.contains("Authorization"),
            "auth leaked cross-origin: {other}"
        );
    }

    #[test]
    fn auth_does_not_follow_a_cross_origin_redirect() {
        // Real shape: a Jira attachment is same-origin, so it gets the header,
        // but Jira 302s it to api.media.atlassian.com. The credential must not
        // make that hop — the destination is pre-signed and needs no auth.
        let (dest_port, dest_requests) = png_server();
        let (base_port, base_requests) =
            redirect_server(format!("http://127.0.0.1:{dest_port}/signed.png"));

        let (rendered, _) = render_html(
            r#"<img src="/attachment/1" width="20" height="20">"#,
            RenderOptions {
                logical_width: 200,
                scale: 1.0,
                load_images: true,
                base_url: format!("http://127.0.0.1:{base_port}/"),
                auth: Some("Basic c2VjcmV0".to_string()),
                scheme: Scheme::Light,
            },
        )
        .unwrap();

        let base = base_requests.lock().unwrap().join("\n");
        let dest = dest_requests.lock().unwrap().join("\n");
        assert!(
            base.contains("Authorization: Basic c2VjcmV0"),
            "same-origin request lacked auth: {base}"
        );
        assert!(!dest.is_empty(), "redirect was not followed");
        assert!(
            !dest.contains("Authorization"),
            "auth followed the redirect off-origin: {dest}"
        );
        // The redirect was still followed to a real image.
        assert!(red_pixels(&rendered) > 100, "redirected image did not render");
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
        let (rendered, _hit) = render_html(html, opts(660, 2.0, false)).unwrap();
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
        let (rendered, hit) = render_html(html, opts(660, 2.0, false)).unwrap();
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
        let (rendered, hit) = render_html(BUTTON_EMAIL, opts(660, 2.0, false)).unwrap();
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
