//! The "Clipboard" settings tab: toggle monitoring and image capture, cap the
//! history size, and clear everything. Changes apply live — toggles persist and
//! take effect immediately; the history-size field persists when it loses focus.
//!
//! Every control is a focus/tab stop, so the tab is fully keyboard navigable.

use std::sync::Arc;

use gpui::prelude::*;
use gpui::{div, px, Context, Entity, FocusHandle, Subscription, Window};

use spotlight_config::AppConfig;
use spotlight_ui::text_input::TextInput;
use spotlight_ui::{controls, theme};

use crate::store::ClipStore;
use crate::ClipboardConfig;

pub struct ClipboardSettingsTab {
    store: Arc<ClipStore>,
    enabled: bool,
    capture_images: bool,
    max_items: Entity<TextInput>,
    enable_focus: FocusHandle,
    images_focus: FocusHandle,
    clear_focus: FocusHandle,
    status: Option<String>,
    subs: Vec<Subscription>,
    registered: bool,
}

impl ClipboardSettingsTab {
    pub fn new(store: Arc<ClipStore>, cx: &mut Context<Self>) -> Self {
        let cfg = crate::load_config();
        let max_items = cx.new(|cx| {
            let mut t = TextInput::new(cx, "200", false);
            t.set_value(cfg.max_items.to_string());
            t
        });
        Self {
            store,
            enabled: cfg.enabled,
            capture_images: cfg.capture_images,
            max_items,
            enable_focus: cx.focus_handle(),
            images_focus: cx.focus_handle(),
            clear_focus: cx.focus_handle(),
            status: None,
            subs: Vec::new(),
            registered: false,
        }
    }

    /// Persist config and apply it to the running store immediately.
    fn persist(&mut self, cx: &mut Context<Self>) {
        let max_items = self
            .max_items
            .read(cx)
            .value()
            .trim()
            .parse::<usize>()
            .unwrap_or(200)
            .clamp(1, 5000);
        let cfg = ClipboardConfig {
            enabled: self.enabled,
            capture_images: self.capture_images,
            max_items,
        };
        let mut app = AppConfig::load();
        let _ = app.set(crate::EXT_ID, &cfg);
        let _ = app.save();

        self.store.set_enabled(cfg.enabled);
        self.store.set_capture_images(cfg.capture_images);
        self.store.set_max_items(cfg.max_items);
        cx.notify();
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.store.clear();
        self.status = Some("Cleared clipboard history.".to_string());
        cx.notify();
    }

    /// Register the blur handler on the history-size field once (needs a `Window`,
    /// which only `render` provides).
    fn ensure_blur_subs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.registered {
            return;
        }
        self.registered = true;
        let handle = self.max_items.read(cx).focus_handle().clone();
        let sub = cx.on_focus_out(&handle, window, |this, _ev, _win, cx| this.persist(cx));
        self.subs.push(sub);
    }
}

impl Render for ClipboardSettingsTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_blur_subs(window, cx);

        div()
            .id("clipboard-settings")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .child(controls::settings_row(
                "Monitor clipboard",
                "Record what you copy into local, encrypted history.",
                controls::toggle(&self.enable_focus, self.enabled, cx, |this, _, cx| {
                    this.enabled = !this.enabled;
                    this.persist(cx);
                }),
            ))
            .child(controls::settings_row(
                "Capture images",
                "Also store copied images (they can be large).",
                controls::toggle(&self.images_focus, self.capture_images, cx, |this, _, cx| {
                    this.capture_images = !this.capture_images;
                    this.persist(cx);
                }),
            ))
            .child(controls::settings_row(
                "History size",
                "Maximum un-pinned items to keep.",
                div().w(px(120.)).child(self.max_items.clone()),
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .pt_3()
                    .child(controls::button(&self.clear_focus, "Clear history", cx, |this, _, cx| {
                        this.clear(cx)
                    }))
                    .when_some(self.status.clone(), |this, status| {
                        this.child(div().text_xs().text_color(theme::muted()).child(status))
                    }),
            )
    }
}
