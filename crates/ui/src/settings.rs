//! Built-in "General" settings tab. Extension-provided tabs (e.g. Jira) are
//! supplied as `AnyView`s through [`crate::UiExtensions`]; this is the one tab
//! the shell always shows.

use gpui::prelude::*;
use gpui::{div, Context, Window};

use crate::theme;

pub struct GeneralSettingsView {
    hotkey: String,
}

impl GeneralSettingsView {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            hotkey: std::env::var("SPOTLIGHT_HOTKEY").unwrap_or_else(|_| "cmd+space".to_string()),
        }
    }

    fn row(label: &str, value: String) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .justify_between()
            .py_2()
            .child(div().text_color(theme::muted()).child(label.to_string()))
            .child(div().text_color(theme::text()).child(value))
    }
}

impl Render for GeneralSettingsView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_color(theme::text())
                    .child("Spotlight")
                    .text_xl()
                    .pb_2(),
            )
            .child(Self::row("Version", env!("CARGO_PKG_VERSION").to_string()))
            .child(Self::row("Global hotkey", self.hotkey.clone()))
            .child(
                div()
                    .pt_4()
                    .text_xs()
                    .text_color(theme::muted())
                    .child("Set SPOTLIGHT_HOTKEY to change the summon shortcut."),
            )
    }
}
