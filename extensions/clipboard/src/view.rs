//! The Clipboard History panel: a filterable, keyboard-navigable list on the
//! left with rich per-kind previews on the right (full text, link + host, a
//! color swatch with values, or an image). Enter copies the selection back to
//! the pasteboard and dismisses the launcher; ⌘P pins, ⌘⌫ deletes.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::prelude::*;
use gpui::{
    div, img, px, rgba, AnyElement, Context, FocusHandle, ImageSource, MouseButton, ObjectFit,
    RenderImage, Window,
};

use spotlight_ui::list::{self, ListNav};
use spotlight_ui::theme;

use crate::store::{ClipEntry, ClipKind, ClipStore};

/// A human label for a content kind.
pub fn kind_label(kind: ClipKind) -> &'static str {
    match kind {
        ClipKind::Text => "Text",
        ClipKind::Link => "Link",
        ClipKind::Color => "Color",
        ClipKind::Image => "Image",
    }
}

pub struct ClipboardView {
    store: Arc<ClipStore>,
    /// Full history snapshot, most-recent first.
    entries: Vec<ClipEntry>,
    /// Store version this snapshot reflects, to notice background changes.
    version: u64,
    /// Positions into `entries` shown in the list: pinned first, then history,
    /// filtered by `filter`.
    visible: Vec<usize>,
    /// How many of the leading `visible` items are pinned (for the section label).
    pinned_count: usize,
    /// In-panel filter text (typed characters).
    filter: String,
    nav: ListNav,
    hovered: Option<usize>,
    /// Decoded image previews, keyed by entry id (`None` = failed to decode).
    img_cache: HashMap<String, Option<Arc<RenderImage>>>,
    focus_handle: FocusHandle,
    focused_once: bool,
    watching: bool,
}

impl ClipboardView {
    pub fn new(store: Arc<ClipStore>, cx: &mut Context<Self>) -> Self {
        let entries = store.snapshot();
        let version = store.version();
        let mut view = Self {
            store,
            entries,
            version,
            visible: Vec::new(),
            pinned_count: 0,
            filter: String::new(),
            nav: ListNav::new(),
            hovered: None,
            img_cache: HashMap::new(),
            focus_handle: cx.focus_handle(),
            focused_once: false,
            watching: false,
        };
        view.recompute();
        view
    }

    // ---- data ------------------------------------------------------------

    /// Rebuild `visible`/`pinned_count` from `entries` and the current filter.
    fn recompute(&mut self) {
        let needle = self.filter.trim().to_lowercase();
        let mut pinned = Vec::new();
        let mut rest = Vec::new();
        for (i, e) in self.entries.iter().enumerate() {
            if !needle.is_empty() {
                let hay = if e.kind == ClipKind::Image {
                    e.title().to_lowercase()
                } else {
                    e.search_text().to_lowercase()
                };
                if !hay.contains(&needle) {
                    continue;
                }
            }
            if e.pinned {
                pinned.push(i);
            } else {
                rest.push(i);
            }
        }
        self.pinned_count = pinned.len();
        pinned.extend(rest);
        self.visible = pinned;
        self.nav.clamp(self.visible.len());
    }

    /// Pull a fresh snapshot from the store (after our own or the monitor's
    /// changes) and re-filter.
    fn reload(&mut self, cx: &mut Context<Self>) {
        self.entries = self.store.snapshot();
        self.version = self.store.version();
        self.recompute();
        cx.notify();
    }

    fn entry_at(&self, pos: usize) -> Option<&ClipEntry> {
        self.visible.get(pos).and_then(|&i| self.entries.get(i))
    }

    fn selected_entry(&self) -> Option<&ClipEntry> {
        self.entry_at(self.nav.selected)
    }

    /// Decode + cache image previews for the currently-visible image entries.
    fn prime_images(&mut self) {
        let ids: Vec<String> = self
            .visible
            .iter()
            .filter_map(|&i| {
                let e = &self.entries[i];
                (e.kind == ClipKind::Image).then(|| e.id.clone())
            })
            .collect();
        for id in ids {
            if !self.img_cache.contains_key(&id) {
                let decoded = self
                    .store
                    .image_bytes(&id)
                    .and_then(|b| spotlight_ui::render_image_from_png_bytes(&b));
                self.img_cache.insert(id, decoded);
            }
        }
    }

    // ---- actions ---------------------------------------------------------

    fn copy_pos(&mut self, pos: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entry_at(pos).cloned() else {
            return;
        };
        match entry.kind {
            ClipKind::Image => {
                if let Some(png) = self.store.image_bytes(&entry.id) {
                    spotlight_platform_macos::clipboard::write_image_png(&png);
                }
            }
            _ => {
                if let Some(text) = &entry.text {
                    spotlight_platform_macos::clipboard::write_text(text);
                }
            }
        }
        // Copy, dismiss (which returns focus to the previous app), then paste
        // into it. A short delay lets that app become key again first.
        spotlight_ui::hide_launcher_window(window);
        cx.spawn(async move |_this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            spotlight_platform_macos::input::paste();
        })
        .detach();
    }

    fn pin_pos(&mut self, pos: usize, cx: &mut Context<Self>) {
        if let Some(id) = self.entry_at(pos).map(|e| e.id.clone()) {
            self.store.toggle_pin(&id);
            self.reload(cx);
        }
    }

    fn delete_pos(&mut self, pos: usize, cx: &mut Context<Self>) {
        if let Some(id) = self.entry_at(pos).map(|e| e.id.clone()) {
            self.store.delete(&id);
            self.img_cache.remove(&id);
            self.reload(cx);
        }
    }

    fn on_key_down(&mut self, event: &gpui::KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let cmd = event.keystroke.modifiers.platform;
        let ctrl = event.keystroke.modifiers.control;
        let len = self.visible.len();

        match key {
            // Escape bubbles to the shell (back to Home).
            "escape" => return,
            "down" => self.nav.next(len),
            "up" => self.nav.prev(),
            "enter" => {
                let pos = self.nav.selected;
                self.copy_pos(pos, window, cx);
                cx.stop_propagation();
                return;
            }
            "p" if cmd => {
                let pos = self.nav.selected;
                self.pin_pos(pos, cx);
            }
            "backspace" if cmd => {
                let pos = self.nav.selected;
                self.delete_pos(pos, cx);
            }
            "backspace" => {
                self.filter.pop();
                self.recompute();
                cx.notify();
            }
            _ => {
                if !cmd && !ctrl {
                    if let Some(ch) = &event.keystroke.key_char {
                        self.filter.push_str(ch);
                        self.recompute();
                        cx.notify();
                    } else {
                        return;
                    }
                } else {
                    return;
                }
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    // ---- rendering -------------------------------------------------------

    /// The leading preview slot for a row (swatch / thumbnail / glyph tile).
    fn row_leading(&self, entry: &ClipEntry) -> AnyElement {
        match entry.kind {
            ClipKind::Color => div()
                .size(px(28.))
                .rounded_md()
                .border_1()
                .border_color(theme::divider())
                .bg(rgba(entry.color.unwrap_or(0)))
                .into_any_element(),
            ClipKind::Image => match self.img_cache.get(&entry.id).and_then(|o| o.clone()) {
                Some(image) => div()
                    .size(px(28.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        img(ImageSource::Render(image))
                            .w(px(28.))
                            .h(px(28.))
                            .object_fit(ObjectFit::Contain),
                    )
                    .into_any_element(),
                None => glyph_tile("🖼"),
            },
            ClipKind::Link => glyph_tile("🔗"),
            ClipKind::Text => glyph_tile("📄"),
        }
    }

    fn row(&self, pos: usize, entry: &ClipEntry, cx: &mut Context<Self>) -> AnyElement {
        let selected = pos == self.nav.selected;
        let active = selected || self.hovered == Some(pos);
        let subtitle = format!("{} · {}", kind_label(entry.kind), relative_time(entry.ts));

        let mut trailing = div().flex().items_center().gap_1();
        if entry.pinned {
            trailing = trailing.child(div().text_xs().text_color(theme::accent()).child("📌"));
        }
        if active {
            trailing = trailing
                .child(mini_button(
                    if entry.pinned { "📍" } else { "📌" },
                    cx.listener(move |this, _, _, cx| this.pin_pos(pos, cx)),
                ))
                .child(mini_button(
                    "✕",
                    cx.listener(move |this, _, _, cx| this.delete_pos(pos, cx)),
                ));
        }

        div()
            .id(("clip-row", pos))
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .rounded_lg()
            .when(selected, |r| r.bg(theme::selected()))
            .hover(|s| s.bg(theme::hover()))
            .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                if *hovered {
                    this.hovered = Some(pos);
                } else if this.hovered == Some(pos) {
                    this.hovered = None;
                }
                cx.notify();
            }))
            .child(self.row_leading(entry))
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .child(
                        div()
                            .truncate()
                            .text_color(theme::text())
                            .child(display_title(entry)),
                    )
                    .child(div().text_xs().text_color(theme::muted()).child(subtitle)),
            )
            .child(trailing)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| this.copy_pos(pos, window, cx)),
            )
            .into_any_element()
    }

    fn list(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut list = div()
            .id("clip-list")
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .py_2()
            .flex_1()
            .overflow_y_scroll()
            .track_scroll(&self.nav.scroll);

        let show_sections = self.pinned_count > 0;
        for (pos, &i) in self.visible.iter().enumerate() {
            if show_sections && pos == 0 {
                list = list.child(section_label("Pinned"));
            }
            if show_sections && pos == self.pinned_count {
                list = list.child(section_label("History"));
            }
            list = list.child(self.row(pos, &self.entries[i], cx));
        }
        list::faded_scroll(&self.nav.scroll, true, list.into_any_element())
    }

    /// The right-hand detail preview for the selected entry.
    fn detail(&self) -> AnyElement {
        let Some(entry) = self.selected_entry() else {
            return centered("Select an item to preview it.");
        };
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(div().text_sm().text_color(theme::accent()).child(kind_label(entry.kind)))
            .child(
                div()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(relative_time(entry.ts)),
            );

        let body: AnyElement = match entry.kind {
            ClipKind::Image => self.detail_image(entry),
            ClipKind::Color => detail_color(entry),
            ClipKind::Link => detail_link(entry),
            ClipKind::Text => detail_text(entry),
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .gap_3()
            .p_4()
            .child(header)
            .child(div().flex_1().min_h(px(0.)).child(body))
            .child(hints())
            .into_any_element()
    }

    fn detail_image(&self, entry: &ClipEntry) -> AnyElement {
        let content = match self.img_cache.get(&entry.id).and_then(|o| o.clone()) {
            Some(image) => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    img(ImageSource::Render(image))
                        .max_w_full()
                        .max_h_full()
                        .object_fit(ObjectFit::Contain),
                )
                .into_any_element(),
            None => centered("Couldn't load image."),
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .gap_2()
            .child(content)
            .child(
                div()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(format!(
                        "{} × {} px",
                        entry.width.unwrap_or(0),
                        entry.height.unwrap_or(0)
                    )),
            )
            .into_any_element()
    }
}

impl Render for ClipboardView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused_once {
            window.focus(&self.focus_handle, cx);
            self.focused_once = true;
        }
        // Keep the open panel live: poll the store version and refresh when the
        // background monitor (or another action) changes history.
        if !self.watching {
            self.watching = true;
            cx.spawn(async move |this, cx| loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let keep = this
                    .update(cx, |this, cx| {
                        if this.store.version() != this.version {
                            this.reload(cx);
                        }
                    })
                    .is_ok();
                if !keep {
                    break;
                }
            })
            .detach();
        }
        self.prime_images();

        let filter_row = div()
            .flex()
            .items_center()
            .gap_2()
            .px_4()
            .py_3()
            .child(div().text_color(theme::accent()).child("⌕"))
            .child(if self.filter.is_empty() {
                div()
                    .text_color(theme::muted())
                    .child("Filter clipboard history…")
            } else {
                div().text_color(theme::text()).child(self.filter.clone())
            });

        let left = div()
            .flex()
            .flex_col()
            .w(px(300.))
            .h_full()
            .child(filter_row)
            .child(div().h(px(1.)).bg(theme::divider()))
            .child(if self.visible.is_empty() {
                centered(if self.entries.is_empty() {
                    "Nothing copied yet. Copy something and it'll show up here."
                } else {
                    "No matches."
                })
            } else {
                self.list(cx)
            });

        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .size_full()
            .flex()
            .flex_row()
            .child(left)
            .child(div().w(px(1.)).h_full().bg(theme::divider()))
            .child(div().flex_1().min_w(px(0.)).h_full().child(self.detail()))
    }
}

// --- free-standing render helpers ------------------------------------------

/// The list-row title: for images the dimensions label, otherwise the first
/// meaningful line with whitespace collapsed.
fn display_title(entry: &ClipEntry) -> String {
    let t = entry.title();
    if t.is_empty() {
        "(empty)".to_string()
    } else {
        t
    }
}

fn detail_text(entry: &ClipEntry) -> AnyElement {
    let text = entry.text.clone().unwrap_or_default();
    let chars = text.chars().count();
    let lines = text.lines().count().max(1);
    div()
        .flex()
        .flex_col()
        .size_full()
        .gap_2()
        .child(
            div()
                .text_xs()
                .text_color(theme::muted())
                .child(format!("{chars} chars · {lines} lines")),
        )
        .child(
            div()
                .id("clip-detail-text")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .p_3()
                .rounded_lg()
                .bg(rgba(0x00_0000_33))
                .text_sm()
                .text_color(theme::text())
                .child(text),
        )
        .into_any_element()
}

fn detail_link(entry: &ClipEntry) -> AnyElement {
    let url = entry.text.clone().unwrap_or_default();
    let host = url
        .split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or("").to_string())
        .unwrap_or_default();
    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(div().text_2xl().child("🔗"))
        .when(!host.is_empty(), |c| {
            c.child(div().text_color(theme::accent()).child(host))
        })
        .child(
            div()
                .p_3()
                .rounded_lg()
                .bg(rgba(0x00_0000_33))
                .text_sm()
                .text_color(theme::text())
                .child(url),
        )
        .into_any_element()
}

fn detail_color(entry: &ClipEntry) -> AnyElement {
    let color = entry.color.unwrap_or(0);
    let [r, g, b, a] = color.to_be_bytes();
    let hex = if a == 255 {
        format!("#{:02X}{:02X}{:02X}", r, g, b)
    } else {
        format!("#{:02X}{:02X}{:02X}{:02X}", r, g, b, a)
    };
    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .w_full()
                .h(px(120.))
                .rounded_xl()
                .border_1()
                .border_color(theme::divider())
                .bg(rgba(color)),
        )
        .child(div().text_xl().text_color(theme::text()).child(hex))
        .child(
            div()
                .text_sm()
                .text_color(theme::muted())
                .child(format!("rgb({r}, {g}, {b})")),
        )
        .into_any_element()
}

/// Footer key hints.
fn hints() -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_4()
        .text_xs()
        .text_color(theme::muted())
        .child("↵ Paste")
        .child("⌘P Pin")
        .child("⌘⌫ Delete")
        .into_any_element()
}

fn section_label(text: &str) -> AnyElement {
    div()
        .px_3()
        .pt_2()
        .pb_1()
        .text_xs()
        .text_color(theme::muted())
        .child(text.to_string())
        .into_any_element()
}

fn glyph_tile(glyph: &str) -> AnyElement {
    div()
        .size(px(28.))
        .rounded_md()
        .bg(theme::tile())
        .flex()
        .items_center()
        .justify_center()
        .text_color(theme::accent())
        .child(glyph.to_string())
        .into_any_element()
}

/// A tiny hover/selection action button (pin/delete) shown at a row's trailing.
fn mini_button(
    glyph: &str,
    on_click: impl Fn(&gpui::MouseDownEvent, &mut Window, &mut gpui::App) + 'static,
) -> AnyElement {
    div()
        .id(gpui::SharedString::from(format!("clip-mini-{glyph}")))
        .size(px(22.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_xs()
        .text_color(theme::muted())
        .hover(|s| s.bg(theme::hover_strong()))
        .child(glyph.to_string())
        .on_mouse_down(MouseButton::Left, on_click)
        .into_any_element()
}

fn centered(msg: &str) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .p_4()
        .text_color(theme::muted())
        .child(msg.to_string())
        .into_any_element()
}

/// "just now", "5m ago", "3h ago", "2d ago", from a unix-seconds timestamp.
fn relative_time(ts: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(ts);
    let secs = now.saturating_sub(ts);
    match secs {
        0..=9 => "just now".to_string(),
        10..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}
