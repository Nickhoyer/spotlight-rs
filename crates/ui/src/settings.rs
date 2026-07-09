//! Built-in "General" settings tab. Extension-provided tabs (e.g. Jira) are
//! supplied as `AnyView`s through [`crate::UiExtensions`]; this is the one tab
//! the shell always shows.

use gpui::prelude::*;
use gpui::{div, Context, Window};

use crate::{controls, theme};

pub struct GeneralSettingsView {
    hotkey: String,
}

impl GeneralSettingsView {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            hotkey: std::env::var("SPOTLIGHT_HOTKEY").unwrap_or_else(|_| "cmd+space".to_string()),
        }
    }

    fn value(text: String) -> impl IntoElement {
        div().text_color(theme::text()).child(text)
    }
}

impl Render for GeneralSettingsView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let about = div()
            .flex()
            .flex_col()
            .child(controls::settings_row(
                "Version",
                "",
                Self::value(env!("CARGO_PKG_VERSION").to_string()),
            ))
            .child(controls::settings_row(
                "Global hotkey",
                "Set SPOTLIGHT_HOTKEY to change the summon shortcut.",
                Self::value(self.hotkey.clone()),
            ));

        div()
            .flex()
            .flex_col()
            .child(div().text_color(theme::text()).child("Spotlight").text_xl().pb_2())
            .child(controls::section("About", about))
    }
}
