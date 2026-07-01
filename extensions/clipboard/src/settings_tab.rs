//! The "Clipboard" settings tab: toggle monitoring and image capture, cap the
//! history size, and clear everything. Saving persists [`ClipboardConfig`] and
//! applies the changes to the live store immediately.
//!
//! Every control is a focus/tab stop, so the tab is fully keyboard navigable.

use std::sync::Arc;

use gpui::prelude::*;
use gpui::{div, px, Context, Entity, FocusHandle, Window};

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
    save_focus: FocusHandle,
    status: Option<String>,
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
            save_focus: cx.focus_handle(),
            status: None,
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
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
        let saved = app.save();

        // Apply to the running store immediately.
        self.store.set_enabled(cfg.enabled);
        self.store.set_capture_images(cfg.capture_images);
        self.store.set_max_items(cfg.max_items);

        self.status = Some(match saved {
            Ok(()) => "Saved.".to_string(),
            Err(e) => format!("Couldn't save: {e}"),
        });
        cx.notify();
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.store.clear();
        self.status = Some("Cleared clipboard history.".to_string());
        cx.notify();
    }

    /// A labelled On/Off toggle button row.
    fn toggle_row(
        label: &str,
        hint: &str,
        on: bool,
        focus: &FocusHandle,
        cx: &mut Context<Self>,
        toggle: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .justify_between()
            .pb_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(div().text_color(theme::text()).child(label.to_string()))
                    .child(div().text_xs().text_color(theme::muted()).child(hint.to_string())),
            )
            .child(controls::button(
                focus,
                if on { "On" } else { "Off" },
                cx,
                toggle,
            ))
    }
}

impl Render for ClipboardSettingsTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("clipboard-settings")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .child(Self::toggle_row(
                "Monitor clipboard",
                "Record what you copy into local, encrypted history.",
                self.enabled,
                &self.enable_focus,
                cx,
                |this, _, cx| {
                    this.enabled = !this.enabled;
                    cx.notify();
                },
            ))
            .child(Self::toggle_row(
                "Capture images",
                "Also store copied images (they can be large).",
                self.capture_images,
                &self.images_focus,
                cx,
                |this, _, cx| {
                    this.capture_images = !this.capture_images;
                    cx.notify();
                },
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .pb_3()
                    .child(div().text_color(theme::text()).child("History size"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::muted())
                            .child("Maximum un-pinned items to keep."),
                    )
                    .child(div().w(px(120.)).child(self.max_items.clone())),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .pt_2()
                    .child(controls::button(&self.save_focus, "Save", cx, |this, _, cx| {
                        this.save(cx)
                    }))
                    .child(controls::button(&self.clear_focus, "Clear history", cx, |this, _, cx| {
                        this.clear(cx)
                    }))
                    .when_some(self.status.clone(), |this, status| {
                        this.child(div().text_xs().text_color(theme::muted()).child(status))
                    }),
            )
    }
}
