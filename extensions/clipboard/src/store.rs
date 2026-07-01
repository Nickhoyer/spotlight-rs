//! The clipboard history itself: the entry model, content classification, an
//! encrypted on-disk store, and the background monitor that watches the system
//! pasteboard and records new copies.
//!
//! The in-memory [`ClipStore`] is the single source of truth shared by the
//! monitor (writer), the search extension, and the panel view (readers/mutators),
//! all through an `Arc`. Text/link/color content lives inline in the encrypted
//! index; images are written to their own encrypted files and loaded lazily.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::crypto::Cipher;

/// How often the monitor polls the pasteboard change counter.
const POLL: Duration = Duration::from_millis(600);

/// What kind of content an entry holds, driving its preview.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipKind {
    Text,
    Link,
    Color,
    Image,
}

/// A single clipboard-history entry. Text/link/color keep their content in
/// `text`; images keep pixel dimensions here and their bytes in a sidecar file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClipEntry {
    /// Content hash, stable across runs; also the sidecar image filename.
    pub id: String,
    pub kind: ClipKind,
    /// The textual content for text/link/color kinds.
    #[serde(default)]
    pub text: Option<String>,
    /// Parsed 0xRRGGBBAA color for [`ClipKind::Color`].
    #[serde(default)]
    pub color: Option<u32>,
    /// Pixel dimensions for [`ClipKind::Image`].
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub pinned: bool,
    /// Unix seconds of the most recent copy.
    pub ts: u64,
}

impl ClipEntry {
    /// A short single-line label for lists (first non-empty line, trimmed).
    pub fn title(&self) -> String {
        match self.kind {
            ClipKind::Image => format!(
                "Image · {} × {}",
                self.width.unwrap_or(0),
                self.height.unwrap_or(0)
            ),
            _ => self
                .text
                .as_deref()
                .unwrap_or_default()
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
                .to_string(),
        }
    }

    /// The text used for fuzzy search (empty for images).
    pub fn search_text(&self) -> &str {
        self.text.as_deref().unwrap_or_default()
    }
}

/// Mutable, in-memory state behind the store's lock.
struct Inner {
    entries: Vec<ClipEntry>,
    last_change: i64,
}

/// The shared clipboard history. Cheap to `clone` the `Arc`; all mutation goes
/// through the internal lock and bumps [`ClipStore::version`] so open views can
/// notice changes.
pub struct ClipStore {
    inner: Mutex<Inner>,
    crypto: Cipher,
    version: AtomicU64,
    enabled: AtomicBool,
    capture_images: AtomicBool,
    max_items: AtomicUsize,
}

impl ClipStore {
    /// Load the persisted history (decrypting it) and apply `cfg`.
    pub fn load(cfg: &crate::ClipboardConfig) -> Arc<Self> {
        let crypto = Cipher::load_or_create();
        let entries = read_index(&crypto);
        let store = Arc::new(Self {
            inner: Mutex::new(Inner {
                entries,
                // Seed with the current change count so items already on the
                // pasteboard at launch aren't re-captured as "new".
                last_change: spotlight_platform_macos::clipboard::change_count(),
            }),
            crypto,
            version: AtomicU64::new(0),
            enabled: AtomicBool::new(cfg.enabled),
            capture_images: AtomicBool::new(cfg.capture_images),
            max_items: AtomicUsize::new(cfg.max_items.max(1)),
        });
        store.enforce_cap();
        store
    }

    // ---- config-backed toggles -------------------------------------------

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }
    pub fn set_capture_images(&self, on: bool) {
        self.capture_images.store(on, Ordering::Relaxed);
    }
    pub fn set_max_items(&self, n: usize) {
        self.max_items.store(n.max(1), Ordering::Relaxed);
        self.enforce_cap();
    }

    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }
    fn capture_images(&self) -> bool {
        self.capture_images.load(Ordering::Relaxed)
    }
    fn max_items(&self) -> usize {
        self.max_items.load(Ordering::Relaxed)
    }

    /// A monotonically increasing counter bumped on every change, so a view can
    /// cheaply detect that its snapshot is stale.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    /// A clone of the current entries, most-recent first.
    pub fn snapshot(&self) -> Vec<ClipEntry> {
        self.inner.lock().unwrap().entries.clone()
    }

    // ---- mutation --------------------------------------------------------

    /// Toggle an entry's pinned flag.
    pub fn toggle_pin(&self, id: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(e) = inner.entries.iter_mut().find(|e| e.id == id) {
            e.pinned = !e.pinned;
        }
        self.after_mutation(inner);
    }

    /// Remove an entry (and its sidecar image, if any).
    pub fn delete(&self, id: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.retain(|e| e.id != id);
        remove_image_file(id);
        self.after_mutation(inner);
    }

    /// Remove everything, pinned included, and all sidecar images.
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.clear();
        let _ = std::fs::remove_dir_all(images_dir());
        self.after_mutation(inner);
    }

    /// Decrypt and return the PNG bytes for an image entry.
    pub fn image_bytes(&self, id: &str) -> Option<Vec<u8>> {
        let data = std::fs::read(image_path(id)).ok()?;
        self.crypto.decrypt(&data)
    }

    /// Inject a few demo entries for headless screenshots. Gated by the caller
    /// behind `SPOTLIGHT_CLIPBOARD_SEED`; writes to whatever `SPOTLIGHT_CONFIG_DIR`
    /// points at, never the real store.
    pub fn seed_demo(&self) {
        self.record_text(
            "Meeting notes: ship the Clipboard History extension by Friday — \
             everything is encrypted at rest."
                .to_string(),
        );
        self.record_text("https://github.com/gpui-ce/gpui-ce".to_string());
        self.record_text("rgba(18, 20, 28, 0.95)".to_string());
        self.record_text("#6EE7FF".to_string());
        self.record_text("cargo build -p spotlight".to_string());
        // Pin the most recent so the "Pinned" section is exercised.
        if let Some(id) = self.snapshot().first().map(|e| e.id.clone()) {
            self.toggle_pin(&id);
        }
    }

    // ---- capture (monitor thread) ----------------------------------------

    /// Record newly-copied text, classifying it. No-op for empty text.
    fn record_text(&self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        let (kind, color) = classify(&text);
        let entry = ClipEntry {
            id: hash_id(text.as_bytes()),
            kind,
            text: Some(text),
            color,
            width: None,
            height: None,
            pinned: false,
            ts: now(),
        };
        self.insert(entry, None);
    }

    /// Record a newly-copied image (PNG bytes), persisting its sidecar file.
    fn record_image(&self, png: Vec<u8>) {
        let id = hash_id(&png);
        let (width, height) = png_dimensions(&png).unzip();
        let entry = ClipEntry {
            id,
            kind: ClipKind::Image,
            text: None,
            color: None,
            width,
            height,
            pinned: false,
            ts: now(),
        };
        self.insert(entry, Some(png));
    }

    /// Insert or refresh an entry at the front (de-duplicated by id). `image` is
    /// the PNG payload to persist for image entries.
    fn insert(&self, entry: ClipEntry, image: Option<Vec<u8>>) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(existing) = inner.entries.iter().position(|e| e.id == entry.id) {
            // Seen before: bump it to the front, keeping its pinned state.
            let mut e = inner.entries.remove(existing);
            e.ts = entry.ts;
            inner.entries.insert(0, e);
        } else {
            if let Some(png) = image {
                write_image_file(&self.crypto, &entry.id, &png);
            }
            inner.entries.insert(0, entry);
        }
        self.trim(&mut inner);
        self.after_mutation(inner);
    }

    /// Drop the oldest un-pinned entries beyond the cap, deleting their images.
    fn trim(&self, inner: &mut std::sync::MutexGuard<'_, Inner>) {
        let max = self.max_items();
        while inner.entries.iter().filter(|e| !e.pinned).count() > max {
            // Remove the oldest un-pinned entry (search from the back).
            if let Some(pos) = inner.entries.iter().rposition(|e| !e.pinned) {
                let removed = inner.entries.remove(pos);
                remove_image_file(&removed.id);
            } else {
                break;
            }
        }
    }

    /// Apply the cap outside a specific mutation (e.g. after a settings change).
    fn enforce_cap(&self) {
        let mut inner = self.inner.lock().unwrap();
        self.trim(&mut inner);
        self.after_mutation(inner);
    }

    /// Bump the version, persist the index, and release the lock.
    fn after_mutation(&self, inner: std::sync::MutexGuard<'_, Inner>) {
        self.version.fetch_add(1, Ordering::Relaxed);
        write_index(&self.crypto, &inner.entries);
    }
}

/// Spawn the background pasteboard monitor for `store`. Polls the change
/// counter and records new text/image copies while monitoring is enabled.
pub fn spawn_monitor(store: Arc<ClipStore>) {
    std::thread::Builder::new()
        .name("clipboard-monitor".into())
        .spawn(move || loop {
            std::thread::sleep(POLL);
            if !store.enabled() {
                continue;
            }
            let count = spotlight_platform_macos::clipboard::change_count();
            {
                let mut inner = store.inner.lock().unwrap();
                if count == inner.last_change {
                    continue;
                }
                inner.last_change = count;
            }
            if let Some(text) = spotlight_platform_macos::clipboard::read_text() {
                store.record_text(text);
            } else if store.capture_images() {
                if let Some(png) = spotlight_platform_macos::clipboard::read_image_png() {
                    store.record_image(png);
                }
            }
        })
        .expect("spawn clipboard monitor");
}

// --- classification --------------------------------------------------------

/// Classify text as a color, link, or plain text.
fn classify(text: &str) -> (ClipKind, Option<u32>) {
    let t = text.trim();
    if let Some(rgba) = parse_color(t) {
        return (ClipKind::Color, Some(rgba));
    }
    if is_url(t) {
        return (ClipKind::Link, None);
    }
    (ClipKind::Text, None)
}

/// A single-token `http(s)://…` string counts as a link.
fn is_url(t: &str) -> bool {
    (t.starts_with("http://") || t.starts_with("https://"))
        && !t.contains(char::is_whitespace)
        && t.len() > 10
}

/// Parse `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`, or `rgb()/rgba()` into a packed
/// `0xRRGGBBAA` value. Returns `None` if the whole string isn't a color.
pub fn parse_color(t: &str) -> Option<u32> {
    let s = t.trim();
    if let Some(hex) = s.strip_prefix('#') {
        return parse_hex_color(hex);
    }
    let lower = s.to_ascii_lowercase();
    if let Some(inner) = lower
        .strip_prefix("rgba(")
        .or_else(|| lower.strip_prefix("rgb("))
    {
        let inner = inner.strip_suffix(')')?;
        let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
        if parts.len() != 3 && parts.len() != 4 {
            return None;
        }
        let r: u8 = parts[0].parse().ok()?;
        let g: u8 = parts[1].parse().ok()?;
        let b: u8 = parts[2].parse().ok()?;
        let a: u8 = if parts.len() == 4 {
            let f: f32 = parts[3].parse().ok()?;
            (f.clamp(0.0, 1.0) * 255.0).round() as u8
        } else {
            255
        };
        return Some(u32::from_be_bytes([r, g, b, a]));
    }
    None
}

fn parse_hex_color(hex: &str) -> Option<u32> {
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let expand = |c: char| {
        let d = c.to_digit(16).unwrap() as u8;
        d << 4 | d
    };
    let (r, g, b, a) = match hex.len() {
        3 => {
            let c: Vec<char> = hex.chars().collect();
            (expand(c[0]), expand(c[1]), expand(c[2]), 255)
        }
        4 => {
            let c: Vec<char> = hex.chars().collect();
            (expand(c[0]), expand(c[1]), expand(c[2]), expand(c[3]))
        }
        6 => (
            u8::from_str_radix(&hex[0..2], 16).ok()?,
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
            255,
        ),
        8 => (
            u8::from_str_radix(&hex[0..2], 16).ok()?,
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
            u8::from_str_radix(&hex[6..8], 16).ok()?,
        ),
        _ => return None,
    };
    Some(u32::from_be_bytes([r, g, b, a]))
}

// --- persistence -----------------------------------------------------------

fn store_dir() -> PathBuf {
    spotlight_config::config_dir().join("clipboard")
}
fn index_path() -> PathBuf {
    store_dir().join("index.dat")
}
fn images_dir() -> PathBuf {
    store_dir().join("img")
}
fn image_path(id: &str) -> PathBuf {
    images_dir().join(format!("{id}.dat"))
}

/// Decrypt and parse the on-disk index, or return an empty history.
fn read_index(crypto: &Cipher) -> Vec<ClipEntry> {
    let Ok(data) = std::fs::read(index_path()) else {
        return Vec::new();
    };
    crypto
        .decrypt(&data)
        .and_then(|plain| serde_json::from_slice(&plain).ok())
        .unwrap_or_default()
}

fn write_index(crypto: &Cipher, entries: &[ClipEntry]) {
    let _ = std::fs::create_dir_all(store_dir());
    if let Ok(json) = serde_json::to_vec(entries) {
        if let Ok(sealed) = crypto.encrypt(&json) {
            let _ = std::fs::write(index_path(), sealed);
        }
    }
}

fn write_image_file(crypto: &Cipher, id: &str, png: &[u8]) {
    let _ = std::fs::create_dir_all(images_dir());
    if let Ok(sealed) = crypto.encrypt(png) {
        let _ = std::fs::write(image_path(id), sealed);
    }
}

fn remove_image_file(id: &str) {
    let _ = std::fs::remove_file(image_path(id));
}

// --- small helpers ---------------------------------------------------------

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A stable hex id derived from content bytes (deterministic across runs, so
/// re-copying the same content de-duplicates rather than piling up).
fn hash_id(bytes: &[u8]) -> String {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Read width/height from a PNG's IHDR chunk (bytes 16..24, big-endian).
fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[0..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(png[20..24].try_into().ok()?);
    Some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_content() {
        assert_eq!(classify("plain text").0, ClipKind::Text);
        assert_eq!(classify("https://example.com/x").0, ClipKind::Link);
        assert_eq!(classify("http://a").0, ClipKind::Text); // too short to be a link
        assert_eq!(classify("not a url with spaces http://x").0, ClipKind::Text);
        assert_eq!(classify("#6EE7FF").0, ClipKind::Color);
        assert_eq!(classify("rgb(110, 231, 255)").0, ClipKind::Color);
    }

    #[test]
    fn parses_colors_to_rrggbbaa() {
        assert_eq!(parse_color("#fff"), Some(0xffffffff));
        assert_eq!(parse_color("#6EE7FF"), Some(0x6ee7ffff));
        assert_eq!(parse_color("#6EE7FF80"), Some(0x6ee7ff80));
        assert_eq!(parse_color("rgb(110, 231, 255)"), Some(0x6ee7ffff));
        assert_eq!(parse_color("rgba(0, 0, 0, 1)"), Some(0x000000ff));
        assert_eq!(parse_color("nope"), None);
        assert_eq!(parse_color("#12"), None);
    }

    #[test]
    fn png_dimensions_from_ihdr() {
        // 8-byte signature, 4-byte length, "IHDR", then 2 = width, then height.
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0, 0, 0, 13]); // IHDR length
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&64u32.to_be_bytes());
        png.extend_from_slice(&48u32.to_be_bytes());
        assert_eq!(png_dimensions(&png), Some((64, 48)));
        assert_eq!(png_dimensions(b"not a png"), None);
    }
}
