//! Isolation test for the "no text renders" bug. Opens a NORMAL opaque window
//! (like gpui's own examples) and draws text three ways. Whatever shows tells
//! us whether text is broken globally or only on our Transparent/PopUp window.

use gpui::{
    div, prelude::*, px, rgb, size, App, Bounds, Context, Render, Window, WindowBounds,
    WindowOptions,
};

struct TextCheck;

impl Render for TextCheck {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(0x202028))
            .flex()
            .flex_col()
            .gap_4()
            .p_8()
            .text_color(rgb(0xffffff))
            .child(div().child("1. DEFAULT (no size/font)"))
            .child(div().text_xl().child("2. text_xl() rem-based"))
            .child(
                div()
                    .text_size(px(40.))
                    .font_family("Helvetica Neue")
                    .child("3. px(40) Helvetica"),
            )
            .child(div().text_size(px(40.)).child("4. px(40) default font"))
    }
}

fn main() {
    env_logger::init();
    gpui_platform::application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(520.), px(320.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| TextCheck),
        )
        .unwrap();
        cx.activate(true);
    });
}
